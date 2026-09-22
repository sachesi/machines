//! Renaming and cloning a machine, its snapshots, and the order it boots from its devices in.

use std::cell::RefCell;
use std::rc::Rc;

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::domain_xml::{BootDevice, DiskDevice, MachineConfig};
use crate::host_xml::HostDeviceId;
use crate::hypervisor::Change;
use crate::machine_view::MachineView;
use crate::window::MachinesWindow;
use crate::{adw, dialogs, glib, gtk};

/// Every device the firmware could boot from, in order, and whether it does.
type BootList = Vec<(BootDevice, bool)>;

fn window(view: &MachineView) -> Option<MachinesWindow> {
    view.root().and_downcast()
}

/// Ask for a name that `allowed` takes and none of `taken` has; resolves to it, or None if
/// cancelled.
async fn ask_name(
    view: &MachineView,
    dialog: adw::AlertDialog,
    action: &str,
    initial: &str,
    taken: Vec<String>,
    allowed: impl Fn(&str) -> bool + 'static,
    extra: Option<gtk::Widget>,
) -> Option<String> {
    dialog.add_responses(&[("cancel", &gettext("_Cancel")), ("confirm", action)]);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Suggested);
    dialog.set_close_response("cancel");
    dialog.set_default_response(Some("confirm"));
    let entry = adw::EntryRow::builder()
        .title(gettext("Name"))
        .text(initial)
        .activates_default(true)
        .build();
    let valid = move |name: &str| {
        let name = name.trim();
        !name.is_empty() && allowed(name) && !taken.iter().any(|t| t == name)
    };
    dialog.set_response_enabled("confirm", valid(initial));
    entry.connect_changed(glib::clone!(
        #[weak]
        dialog,
        move |entry| dialog.set_response_enabled("confirm", valid(&entry.text()))
    ));
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&entry);
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    content.append(&list);
    if let Some(extra) = extra {
        content.append(&extra);
    }
    dialog.set_extra_child(Some(&content));
    dialog.set_focus(Some(&entry));
    (dialog.choose_future(Some(view)).await == "confirm").then(|| entry.text().trim().to_owned())
}

fn machine_name(name: &str) -> bool {
    !name.contains('/')
}

/// What QEMU takes as the ID of the job that saves a running machine's snapshot.
fn snapshot_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c))
}

pub fn rename(view: &MachineView) {
    let (Some(win), Some(info)) = (window(view), view.info()) else {
        return;
    };
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Rename “{name}”").replace("{name}", &info.name))
        .build();
    glib::spawn_future_local(glib::clone!(
        #[weak]
        view,
        async move {
            let taken = win.machine_names();
            if let Some(name) = ask_name(
                &view,
                dialog,
                &gettext("_Rename"),
                &info.name,
                taken,
                machine_name,
                None,
            )
            .await
            {
                view.run(move |hv, uuid| hv.rename(uuid, &name));
            }
        }
    ));
}

pub fn clone(view: &MachineView) {
    let (Some(win), Some(info)) = (window(view), view.info()) else {
        return;
    };
    glib::spawn_future_local(glib::clone!(
        #[weak]
        view,
        async move {
            let uuid = info.uuid.clone();
            let plan = match win.call(move |hv| hv.clone_plan(&uuid)).await {
                Some(Ok(plan)) => plan,
                Some(Err(e)) => {
                    win.toast(&e);
                    return;
                }
                None => return,
            };
            let taken = win.machine_names();
            let initial = (1..)
                .map(|i| match i {
                    1 => gettext("{name} (Copy)").replace("{name}", &info.name),
                    i => gettext("{name} (Copy {n})")
                        .replace("{name}", &info.name)
                        .replace("{n}", &i.to_string()),
                })
                .find(|n| !taken.contains(n))
                .expect("an unused name");
            let dialog = adw::AlertDialog::builder()
                .heading(gettext("Clone “{name}”").replace("{name}", &info.name))
                .body(gettext(
                    "The copy gets network addresses and firmware variables of its own.",
                ))
                .build();
            let details = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .build();
            for (heading, files) in [
                (gettext("Disks copied"), &plan.copied),
                (
                    gettext("Left out, as they are in no storage pool"),
                    &plan.left_out,
                ),
            ] {
                if files.is_empty() {
                    continue;
                }
                details.append(
                    &gtk::Label::builder()
                        .label(heading)
                        .xalign(0.0)
                        .css_classes(["heading"])
                        .build(),
                );
                details.append(
                    &gtk::Label::builder()
                        .label(files.join("\n"))
                        .wrap(true)
                        .wrap_mode(gtk::pango::WrapMode::WordChar)
                        .xalign(0.0)
                        .selectable(true)
                        .css_classes(["caption", "dim-label"])
                        .build(),
                );
            }
            let Some(name) = ask_name(
                &view,
                dialog,
                &gettext("_Clone"),
                &initial,
                taken,
                machine_name,
                Some(details.upcast()),
            )
            .await
            else {
                return;
            };
            if !plan.copied.is_empty() {
                win.toast(&gettext("Copying the disks of “{name}”…").replace("{name}", &name));
            }
            let uuid = info.uuid.clone();
            match win.call(move |hv| hv.clone_machine(&uuid, &name)).await {
                Some(Ok(uuid)) => win.select_when_listed(&uuid),
                Some(Err(e)) => win.toast(&e),
                None => {}
            }
            win.refresh();
        }
    ));
}

pub fn take_snapshot(view: &MachineView) {
    let (Some(win), Some(info)) = (window(view), view.info()) else {
        return;
    };
    let taken: Vec<String> = info.snapshots.iter().map(|s| s.name.clone()).collect();
    let initial = (1..)
        .map(|i| format!("snapshot-{i}"))
        .find(|n| !taken.contains(n))
        .expect("an unused name");
    let dialog = adw::AlertDialog::builder()
        .heading(gettext("Take Snapshot"))
        .body(if info.state.is_active() {
            gettext(
                "The snapshot keeps the memory of the running machine as well as its disks, \
                 so reverting to it resumes the machine where it was. The machine pauses \
                 while its memory is saved.\n\nNames take letters, digits, “-”, “.” and “_”.",
            )
        } else {
            gettext(
                "The snapshot keeps the machine’s disks and settings as they are now.\n\n\
                 Names take letters, digits, “-”, “.” and “_”.",
            )
        })
        .build();
    let description = adw::EntryRow::builder()
        .title(gettext("Description"))
        .activates_default(true)
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&description);
    glib::spawn_future_local(glib::clone!(
        #[weak]
        view,
        async move {
            let Some(name) = ask_name(
                &view,
                dialog,
                &gettext("_Take"),
                &initial,
                taken,
                snapshot_name,
                Some(list.upcast()),
            )
            .await
            else {
                return;
            };
            let description = description.text().trim().to_owned();
            win.toast(&gettext("Taking “{name}”…").replace("{name}", &name));
            view.run(move |hv, uuid| hv.take_snapshot(uuid, &name, &description));
        }
    ));
}

pub fn revert_snapshot(view: &MachineView, name: &str) {
    let name = name.to_owned();
    glib::spawn_future_local(glib::clone!(
        #[weak]
        view,
        async move {
            let confirmed = dialogs::confirm(
                &view,
                &gettext("Revert to “{name}”?").replace("{name}", &name),
                &gettext(
                    "The virtual machine goes back to how it was when the snapshot was taken. \
                     What changed on its disks since then is lost, unless another snapshot \
                     has it.",
                ),
                &gettext("_Revert"),
            )
            .await;
            if confirmed {
                view.run(move |hv, uuid| hv.revert_snapshot(uuid, &name));
            }
        }
    ));
}

pub fn delete_snapshot(view: &MachineView, name: &str) {
    let name = name.to_owned();
    glib::spawn_future_local(glib::clone!(
        #[weak]
        view,
        async move {
            let confirmed = dialogs::confirm(
                &view,
                &gettext("Delete “{name}”?").replace("{name}", &name),
                &gettext(
                    "The virtual machine stays as it is; only the way back to this snapshot \
                     goes.",
                ),
                &gettext("_Delete"),
            )
            .await;
            if confirmed {
                view.run(move |hv, uuid| hv.delete_snapshot(uuid, &name));
            }
        }
    ));
}

/// What `device` is, and which of its kind.
pub fn boot_label(config: &MachineConfig, device: BootDevice) -> (String, String) {
    match device {
        BootDevice::Disk(n) => {
            let Some(disk) = config.disks.get(n) else {
                return Default::default();
            };
            let title = match disk.device {
                DiskDevice::Cdrom => gettext("CD/DVD Drive"),
                DiskDevice::Floppy => gettext("Floppy Drive"),
                _ if disk.kind == "block" => gettext("Host Disk"),
                _ => gettext("Disk"),
            };
            let subtitle = [Some(disk.target.as_str()), disk.source.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            (title, subtitle)
        }
        BootDevice::Nic(n) => {
            let nic = config.nics.get(n);
            let subtitle = [
                nic.and_then(|n| n.source.as_deref()),
                nic.and_then(|n| n.mac.as_deref()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            (gettext("Network (PXE)"), subtitle)
        }
        BootDevice::HostDev(n) => {
            let Some(dev) = config.host_devices.get(n) else {
                return Default::default();
            };
            let title = match dev.id {
                HostDeviceId::Usb { .. } => gettext("USB Device"),
                HostDeviceId::Pci(_) => gettext("PCI Device"),
            };
            (title, dev.id.to_string())
        }
    }
}

/// Every device of `config` the firmware could boot from, the ones it does first.
fn boot_candidates(config: &MachineConfig) -> BootList {
    let all = (0..config.disks.len())
        .map(BootDevice::Disk)
        .chain((0..config.nics.len()).map(BootDevice::Nic))
        .chain((0..config.host_devices.len()).map(BootDevice::HostDev));
    config
        .boot
        .iter()
        .map(|d| (*d, true))
        .chain(all.filter(|d| !config.boot.contains(d)).map(|d| (d, false)))
        .collect()
}

pub fn boot_order(view: &MachineView, config: &MachineConfig, running: bool) {
    let order = Rc::new(RefCell::new(boot_candidates(config)));
    let group = adw::PreferencesGroup::builder()
        .description(if running {
            gettext("The machine boots in this order the next time it starts.")
        } else {
            gettext("The machine tries the devices ticked here, top to bottom.")
        })
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    group.add(&list);
    let page = adw::PreferencesPage::new();
    page.add(&group);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    let dialog = adw::Dialog::builder()
        .title(gettext("Boot Order"))
        .content_width(480)
        .child(&toolbar)
        .build();
    fill_boot_list(view, config, &list, &order);
    dialog.present(Some(view));
}

fn fill_boot_list(
    view: &MachineView,
    config: &MachineConfig,
    list: &gtk::ListBox,
    order: &Rc<RefCell<BootList>>,
) {
    list.remove_all();
    let entries = order.borrow().clone();
    let last = entries.len().saturating_sub(1);
    for (i, (device, on)) in entries.into_iter().enumerate() {
        let (title, subtitle) = boot_label(config, device);
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .build();
        let check = gtk::CheckButton::builder()
            .active(on)
            .valign(gtk::Align::Center)
            .build();
        row.add_prefix(&check);
        row.set_activatable_widget(Some(&check));
        let change = {
            let (view, config, list, order) =
                (view.clone(), config.clone(), list.clone(), order.clone());
            move |edit: &dyn Fn(&mut BootList)| {
                edit(&mut order.borrow_mut());
                let boot: Vec<BootDevice> = order
                    .borrow()
                    .iter()
                    .filter(|(_, on)| *on)
                    .map(|(d, _)| *d)
                    .collect();
                view.run(move |hv, uuid| hv.set_boot_order(uuid, &boot));
                // Out of the handler that asked for it, as it removes the row it came from.
                glib::idle_add_local_once(glib::clone!(
                    #[strong]
                    view,
                    #[strong]
                    config,
                    #[strong]
                    list,
                    #[strong]
                    order,
                    move || fill_boot_list(&view, &config, &list, &order)
                ));
            }
        };
        let change = Rc::new(change);
        check.connect_toggled(glib::clone!(
            #[strong]
            change,
            move |check| {
                let on = check.is_active();
                change(&|order| order[i].1 = on);
            }
        ));
        for (icon, tooltip, sensitive, to) in [
            (
                "go-up-symbolic",
                gettext("Move Up"),
                i > 0,
                i.wrapping_sub(1),
            ),
            ("go-down-symbolic", gettext("Move Down"), i < last, i + 1),
        ] {
            let button = gtk::Button::builder()
                .icon_name(icon)
                .tooltip_text(tooltip)
                .sensitive(sensitive)
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            button.connect_clicked(glib::clone!(
                #[strong]
                change,
                move |_| change(&|order| order.swap(i, to))
            ));
            row.add_suffix(&button);
        }
        list.append(&row);
    }
}

/// The machine's definition as XML, to change what the details page has no row for.
pub fn edit_xml(view: &MachineView) {
    let (Some(win), Some(info)) = (window(view), view.info()) else {
        return;
    };
    glib::spawn_future_local(glib::clone!(
        #[weak]
        view,
        async move {
            let uuid = info.uuid.clone();
            let xml = match win.call(move |hv| hv.xml(&uuid)).await {
                Some(Ok(xml)) => xml,
                Some(Err(e)) => return win.toast(&e),
                None => return,
            };
            let buffer = gtk::TextBuffer::new(None);
            buffer.set_text(&xml);
            let text = gtk::TextView::builder()
                .buffer(&buffer)
                .monospace(true)
                .top_margin(12)
                .bottom_margin(12)
                .left_margin(12)
                .right_margin(12)
                .build();
            let scroller = gtk::ScrolledWindow::builder()
                .child(&text)
                .vexpand(true)
                .build();
            let error = adw::Banner::builder().use_markup(false).build();
            let save = gtk::Button::builder()
                .label(gettext("_Save"))
                .use_underline(true)
                .css_classes(["suggested-action"])
                .build();
            let cancel = gtk::Button::builder()
                .label(gettext("_Cancel"))
                .use_underline(true)
                .build();
            let header = adw::HeaderBar::builder()
                .show_start_title_buttons(false)
                .show_end_title_buttons(false)
                .build();
            header.pack_start(&cancel);
            header.pack_end(&save);
            let toolbar = adw::ToolbarView::builder()
                .top_bar_style(adw::ToolbarStyle::Raised)
                .content(&scroller)
                .build();
            toolbar.add_top_bar(&header);
            toolbar.add_top_bar(&error);
            let dialog = adw::Dialog::builder()
                .title(gettext("Definition of “{name}”").replace("{name}", &info.name))
                .content_width(720)
                .content_height(640)
                .child(&toolbar)
                .build();
            cancel.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                move |_| {
                    dialog.close();
                }
            ));
            save.connect_clicked(glib::clone!(
                #[weak]
                dialog,
                #[weak]
                win,
                #[weak]
                error,
                move |save| {
                    let xml = buffer
                        .text(&buffer.start_iter(), &buffer.end_iter(), false)
                        .to_string();
                    let uuid = info.uuid.clone();
                    save.set_sensitive(false);
                    glib::spawn_future_local(glib::clone!(
                        #[weak]
                        save,
                        async move {
                            let defined = win.call(move |hv| hv.define(&uuid, &xml)).await;
                            save.set_sensitive(true);
                            match defined {
                                Some(Ok(change)) => {
                                    dialog.close();
                                    if change == Change::AtNextStart {
                                        win.toast(&gettext(
                                            "The change takes effect the next time the virtual \
                                             machine starts",
                                        ));
                                    }
                                    win.refresh();
                                }
                                Some(Err(e)) => {
                                    error.set_title(&e);
                                    error.set_revealed(true);
                                }
                                None => {}
                            }
                        }
                    ));
                }
            ));
            dialog.present(Some(&view));
        }
    ));
}
