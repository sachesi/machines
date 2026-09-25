//! Renaming and cloning a machine, its snapshots, and the order it boots from its devices in.

use std::cell::RefCell;
use std::rc::Rc;

use gettextrs::{gettext, ngettext};
use sourceview5::prelude::*;

use crate::adw::prelude::*;
use crate::domain_xml::{BootDevice, DiskDevice, MachineConfig};
use crate::host_xml::HostDeviceId;
use crate::hypervisor::Change;
use crate::machine_view::MachineView;
use crate::window::MachinesWindow;
use crate::{adw, dialogs, gdk, gio, glib, gtk};

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
        let row = adw::ActionRow::new();
        dialogs::set_plain_text(&row, &title, &subtitle);
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
    glib::spawn_future_local(async move {
        let uuid = info.uuid.clone();
        let xml = match win.call(move |hv| hv.xml(&uuid)).await {
            Some(Ok(xml)) => xml,
            Some(Err(e)) => return win.toast(&e),
            None => return,
        };
        let title = gettext("Definition of “{name}”").replace("{name}", &info.name);
        let target = win.clone();
        edit_text(&win, &title, &xml, "xml", move |xml| {
            let (win, uuid) = (target.clone(), info.uuid.clone());
            async move {
                let change = win.call(move |hv| hv.define(&uuid, &xml)).await?;
                Some(change.map(|change| {
                    if change == Change::AtNextStart {
                        win.toast(&gettext(
                            "The change takes effect the next time the virtual machine starts",
                        ));
                    }
                    win.refresh();
                }))
            }
        });
    });
}

/// A window to edit `text` in, highlighted as `language`, for `save` to save. It closes
/// once `save` gives `Some(Ok)`, shows the error of `Some(Err)`, and stays as it is with
/// `None`, where nothing was done.
pub fn edit_text<F, Fut>(win: &MachinesWindow, title: &str, text: &str, language: &str, on_save: F)
where
    F: Fn(String) -> Fut + 'static,
    Fut: Future<Output = Option<Result<(), String>>> + 'static,
{
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(sourceview5::init);
    let buffer = sourceview5::Buffer::new(None);
    buffer.set_language(
        sourceview5::LanguageManager::default()
            .language(language)
            .as_ref(),
    );
    follow_style(&buffer);
    // Not a step to undo, and not a change to save.
    buffer.begin_irreversible_action();
    buffer.set_text(text);
    buffer.end_irreversible_action();
    buffer.set_modified(false);
    let text = sourceview5::View::builder()
        .buffer(&buffer)
        .monospace(true)
        .show_line_numbers(true)
        .highlight_current_line(true)
        .auto_indent(true)
        .tab_width(2)
        .indent_width(2)
        .insert_spaces_instead_of_tabs(true)
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
    let find = FindBar::new(&text, &buffer);
    let find_button = gtk::ToggleButton::builder()
        .icon_name("edit-find-symbolic")
        .tooltip_text(gettext("Find and Replace"))
        .build();
    find_button
        .bind_property("active", &find.bar, "search-mode-enabled")
        .bidirectional()
        .sync_create()
        .build();
    header.pack_end(&find_button);
    let toolbar = adw::ToolbarView::builder()
        .top_bar_style(adw::ToolbarStyle::Raised)
        .content(&scroller)
        .build();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&error);
    toolbar.add_top_bar(&find.bar);
    // A window rather than a dialog, so it can be made as large as the definition is
    // long; it opens as it was last left, but no larger than the main window.
    let settings = crate::prefs::settings();
    let (width, height): (i32, i32) = settings.get("definition-editor-size");
    let fit = |size: i32, room: i32| if room > 0 { size.min(room) } else { size };
    let dialog = adw::Window::builder()
        .title(title)
        .default_width(fit(width, win.width()))
        .default_height(fit(height, win.height()))
        .width_request(360)
        .height_request(294)
        .modal(true)
        .transient_for(win)
        .content(&toolbar)
        .build();
    dialog.set_application(win.application().as_ref());
    let shortcuts = gtk::ShortcutController::new();
    let add = |trigger: &str, f: Box<dyn Fn()>| {
        shortcuts.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string(trigger),
            Some(gtk::CallbackAction::new(move |_, _| {
                f();
                glib::Propagation::Stop
            })),
        ));
    };
    add(
        "<Control>f",
        Box::new(glib::clone!(
            #[strong]
            find,
            move || find.open(false)
        )),
    );
    add(
        "<Control>h",
        Box::new(glib::clone!(
            #[strong]
            find,
            move || find.open(true)
        )),
    );
    add(
        "<Control>g",
        Box::new(glib::clone!(
            #[strong]
            find,
            move || find.step(true)
        )),
    );
    add(
        "<Shift><Control>g",
        Box::new(glib::clone!(
            #[strong]
            find,
            move || find.step(false)
        )),
    );
    // The find bar first, then the window.
    add(
        "Escape",
        Box::new(glib::clone!(
            #[strong]
            find,
            #[weak]
            dialog,
            move || {
                if find.bar.is_search_mode() {
                    find.bar.set_search_mode(false);
                } else {
                    dialog.close();
                }
            }
        )),
    );
    dialog.add_controller(shortcuts);
    dialog.connect_close_request(glib::clone!(
        #[weak]
        buffer,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |dialog| {
            let _ = settings.set("definition-editor-size", dialog.default_size());
            if !buffer.is_modified() {
                return glib::Propagation::Proceed;
            }
            glib::spawn_future_local(glib::clone!(
                #[weak]
                dialog,
                #[weak]
                buffer,
                async move {
                    let discard = dialogs::confirm(
                        &dialog,
                        &gettext("Discard Changes?"),
                        &gettext("The changes have not been saved."),
                        &gettext("_Discard"),
                    )
                    .await;
                    if discard {
                        buffer.set_modified(false);
                        dialog.close();
                    }
                }
            ));
            glib::Propagation::Stop
        }
    ));
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    let on_save = Rc::new(on_save);
    save.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        error,
        move |save| {
            let text = buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .to_string();
            let saved = on_save(text);
            save.set_sensitive(false);
            glib::spawn_future_local(glib::clone!(
                #[weak]
                save,
                #[weak]
                buffer,
                async move {
                    let saved = saved.await;
                    save.set_sensitive(true);
                    match saved {
                        Some(Ok(())) => {
                            buffer.set_modified(false);
                            dialog.close();
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
    dialog.present();
}

/// Find, and find and replace, in the definition editor.
#[derive(Clone)]
struct FindBar {
    bar: gtk::SearchBar,
    find: gtk::SearchEntry,
    replacing: gtk::ToggleButton,
    view: sourceview5::View,
    context: sourceview5::SearchContext,
}

/// A [`FindBar`] for its own handlers to hold, which would otherwise keep it, and the
/// editor with it, alive for good. The editor's shortcuts hold the bar itself.
#[derive(Clone)]
struct WeakFindBar {
    bar: glib::WeakRef<gtk::SearchBar>,
    find: glib::WeakRef<gtk::SearchEntry>,
    replacing: glib::WeakRef<gtk::ToggleButton>,
    view: glib::WeakRef<sourceview5::View>,
    context: glib::WeakRef<sourceview5::SearchContext>,
}

impl WeakFindBar {
    fn upgrade(&self) -> Option<FindBar> {
        Some(FindBar {
            bar: self.bar.upgrade()?,
            find: self.find.upgrade()?,
            replacing: self.replacing.upgrade()?,
            view: self.view.upgrade()?,
            context: self.context.upgrade()?,
        })
    }
}

impl FindBar {
    fn new(view: &sourceview5::View, buffer: &sourceview5::Buffer) -> Self {
        let settings = sourceview5::SearchSettings::new();
        settings.set_wrap_around(true);
        let context = sourceview5::SearchContext::new(buffer, Some(&settings));
        context.set_highlight(false);

        let find = gtk::SearchEntry::builder()
            .placeholder_text(gettext("Find"))
            .hexpand(true)
            .build();
        // As wide as "13 matches" whatever it says, so the entry does not move as it changes.
        let count = gtk::Label::builder()
            .width_chars(10)
            .xalign(1.0)
            .css_classes(["dim-label", "numeric"])
            .build();
        let previous = gtk::Button::builder()
            .icon_name("go-up-symbolic")
            .tooltip_text(gettext("Previous Match"))
            .build();
        let next = gtk::Button::builder()
            .icon_name("go-down-symbolic")
            .tooltip_text(gettext("Next Match"))
            .build();
        let steps = gtk::Box::builder().css_classes(["linked"]).build();
        steps.append(&previous);
        steps.append(&next);
        let replacing = gtk::ToggleButton::builder()
            .icon_name("edit-find-replace-symbolic")
            .tooltip_text(gettext("Replace"))
            .build();
        let options = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        for (label, property) in [
            (gettext("_Match Case"), "case-sensitive"),
            (gettext("Match _Whole Words"), "at-word-boundaries"),
            (gettext("_Regular Expression"), "regex-enabled"),
        ] {
            let check = gtk::CheckButton::with_mnemonic(&label);
            check
                .bind_property("active", &settings, property)
                .sync_create()
                .build();
            options.append(&check);
        }
        let options_button = gtk::MenuButton::builder()
            .icon_name("emblem-system-symbolic")
            .tooltip_text(gettext("Search Options"))
            .popover(&gtk::Popover::builder().child(&options).build())
            .build();
        let find_row = gtk::Box::builder().spacing(6).build();
        find_row.append(&find);
        find_row.append(&count);
        find_row.append(&steps);
        find_row.append(&replacing);
        find_row.append(&options_button);

        let replace = gtk::Entry::builder()
            .placeholder_text(gettext("Replace"))
            .hexpand(true)
            .build();
        let replace_one = gtk::Button::builder()
            .label(gettext("_Replace"))
            .use_underline(true)
            .build();
        let replace_all = gtk::Button::builder()
            .label(gettext("Replace _All"))
            .use_underline(true)
            .build();
        let replace_row = gtk::Box::builder().spacing(6).build();
        replace_row.append(&replace);
        replace_row.append(&replace_one);
        replace_row.append(&replace_all);
        replacing
            .bind_property("active", &replace_row, "visible")
            .sync_create()
            .build();

        let rows = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        rows.append(&find_row);
        rows.append(&replace_row);
        let bar = gtk::SearchBar::builder()
            .child(&adw::Clamp::builder().maximum_size(720).child(&rows).build())
            .build();
        bar.connect_entry(&find);

        let this = Self {
            bar,
            find: find.clone(),
            replacing,
            view: view.clone(),
            context: context.clone(),
        };
        let weak = WeakFindBar {
            bar: this.bar.downgrade(),
            find: find.downgrade(),
            replacing: this.replacing.downgrade(),
            view: view.downgrade(),
            context: context.downgrade(),
        };

        let update = glib::clone!(
            #[strong]
            weak,
            #[weak]
            count,
            #[weak]
            replace_one,
            #[weak]
            replace_all,
            move || {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let total = this.context.occurrences_count();
                let searching = this.context.settings().search_text().is_some();
                let error = this.context.regex_error();
                let text = if !searching || total < 0 {
                    String::new()
                } else if error.is_some() || total == 0 {
                    gettext("No results")
                } else {
                    let buffer = this.view.buffer();
                    let at = buffer
                        .selection_bounds()
                        .map_or(0, |(s, e)| this.context.occurrence_position(&s, &e));
                    if at > 0 {
                        gettext("{n} of {total}")
                            .replace("{n}", &at.to_string())
                            .replace("{total}", &total.to_string())
                    } else {
                        ngettext("{total} match", "{total} matches", total as u32)
                            .replace("{total}", &total.to_string())
                    }
                };
                count.set_label(&text);
                this.find
                    .set_tooltip_text(error.as_ref().map(|e| e.message()));
                if searching && (error.is_some() || total == 0) {
                    this.find.add_css_class("error");
                } else {
                    this.find.remove_css_class("error");
                }
                replace_one.set_sensitive(total > 0);
                replace_all.set_sensitive(total > 0);
            }
        );
        let update = Rc::new(update);
        context.connect_occurrences_count_notify(glib::clone!(
            #[strong]
            update,
            move |_| update()
        ));
        context.connect_notify_local(
            Some("regex-error"),
            glib::clone!(
                #[strong]
                update,
                move |_, _| update()
            ),
        );
        buffer.connect_mark_set(glib::clone!(
            #[strong]
            update,
            move |buffer, _, mark| {
                if *mark == buffer.get_insert() {
                    update();
                }
            }
        ));

        find.connect_search_changed(glib::clone!(
            #[strong]
            weak,
            move |find| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let text = find.text();
                this.context
                    .settings()
                    .set_search_text(Some(text.as_str()).filter(|t| !t.is_empty()));
                // From where the current match starts, so it stays as long as it matches.
                let buffer = this.view.buffer();
                let from = buffer
                    .selection_bounds()
                    .map_or_else(|| buffer.iter_at_mark(&buffer.get_insert()), |(s, _)| s);
                this.select(this.context.forward(&from));
            }
        ));
        find.connect_activate(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(this) = weak.upgrade() {
                    this.step(true);
                }
            }
        ));
        let back = gtk::ShortcutController::new();
        back.add_shortcut(gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Shift>Return"),
            Some(gtk::CallbackAction::new(glib::clone!(
                #[strong]
                weak,
                move |_, _| {
                    if let Some(this) = weak.upgrade() {
                        this.step(false);
                    }
                    glib::Propagation::Stop
                }
            ))),
        ));
        find.add_controller(back);
        previous.connect_clicked(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(this) = weak.upgrade() {
                    this.step(false);
                }
            }
        ));
        next.connect_clicked(glib::clone!(
            #[strong]
            weak,
            move |_| {
                if let Some(this) = weak.upgrade() {
                    this.step(true);
                }
            }
        ));
        replace_one.connect_clicked(glib::clone!(
            #[strong]
            weak,
            #[weak]
            replace,
            move |_| {
                if let Some(this) = weak.upgrade() {
                    this.replace(&replace.text());
                }
            }
        ));
        replace.connect_activate(glib::clone!(
            #[strong]
            weak,
            move |replace| {
                if let Some(this) = weak.upgrade() {
                    this.replace(&replace.text());
                }
            }
        ));
        replace_all.connect_clicked(glib::clone!(
            #[strong]
            weak,
            #[weak]
            replace,
            move |_| {
                if let Some(this) = weak.upgrade() {
                    let _ = this.context.replace_all(&replace.text());
                }
            }
        ));
        this.bar.connect_search_mode_enabled_notify(glib::clone!(
            #[strong]
            weak,
            move |bar| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                let on = bar.is_search_mode();
                this.context.set_highlight(on);
                if !on {
                    this.view.grab_focus();
                }
            }
        ));
        update();
        this
    }

    /// Show the bar, with the replace row if `replace`, to find the selected text if a line
    /// or less is selected.
    fn open(&self, replace: bool) {
        let buffer = self.view.buffer();
        if let Some((start, end)) = buffer.selection_bounds() {
            let selected = buffer.text(&start, &end, false);
            if !selected.contains('\n') {
                self.find.set_text(&selected);
            }
        }
        if replace {
            self.replacing.set_active(true);
        }
        self.bar.set_search_mode(true);
        self.find.grab_focus();
    }

    /// Select the next match after the selection, or with `forward` false, the one before.
    fn step(&self, forward: bool) {
        let buffer = self.view.buffer();
        let insert = buffer.iter_at_mark(&buffer.get_insert());
        let (start, end) = buffer.selection_bounds().unwrap_or((insert, insert));
        self.select(if forward {
            self.context.forward(&end)
        } else {
            self.context.backward(&start)
        });
    }

    fn select(&self, found: Option<(gtk::TextIter, gtk::TextIter, bool)>) {
        if let Some((start, end, _)) = found {
            let buffer = self.view.buffer();
            buffer.select_range(&start, &end);
            self.view
                .scroll_to_mark(&buffer.get_insert(), 0.25, false, 0.0, 0.0);
        }
    }

    /// Replace the selected match with `text` and select the next one; with no match
    /// selected, only select the next.
    fn replace(&self, text: &str) {
        let buffer = self.view.buffer();
        if let Some((mut start, mut end)) = buffer.selection_bounds()
            && self.context.occurrence_position(&start, &end) > 0
            && self.context.replace(&mut start, &mut end, text).is_ok()
        {
            buffer.place_cursor(&end);
        }
        self.step(true);
    }
}

/// Colour the buffer with GtkSourceView's Adwaita scheme, light or dark as the rest of the
/// app is, for as long as the buffer is around.
fn follow_style(buffer: &sourceview5::Buffer) {
    let style = adw::StyleManager::default();
    let apply = |buffer: &sourceview5::Buffer, dark: bool| {
        let name = if dark { "Adwaita-dark" } else { "Adwaita" };
        buffer.set_style_scheme(
            sourceview5::StyleSchemeManager::default()
                .scheme(name)
                .as_ref(),
        );
    };
    apply(buffer, style.is_dark());
    let handler = style.connect_dark_notify(glib::clone!(
        #[weak]
        buffer,
        move |style| apply(&buffer, style.is_dark())
    ));
    let handler = RefCell::new(Some(handler));
    buffer.add_weak_ref_notify_local(move || {
        if let Some(handler) = handler.take() {
            adw::StyleManager::default().disconnect(handler);
        }
    });
}

/// Save what the machine's screen shows as a PNG file the user picks.
pub fn screenshot(view: &MachineView) {
    let (Some(win), Some(info)) = (window(view), view.info()) else {
        return;
    };
    glib::spawn_future_local(async move {
        let uuid = info.uuid.clone();
        let image = match win.call(move |hv| hv.screenshot(&uuid)).await {
            Some(Ok(image)) => image,
            Some(Err(e)) => return win.toast(&e),
            None => return,
        };
        let png = match gdk::Texture::from_bytes(&glib::Bytes::from_owned(image)) {
            Ok(texture) => texture.save_to_png_bytes(),
            Err(e) => return win.toast(&e.to_string()),
        };
        let time = glib::DateTime::now_local()
            .and_then(|t| t.format("%Y-%m-%d %H-%M-%S"))
            .map(|t| t.to_string())
            .unwrap_or_default();
        let dialog = gtk::FileDialog::builder()
            .title(gettext("Save Screenshot"))
            .initial_name(format!("{} {time}.png", info.name).replace('/', "-"))
            .build();
        if let Some(pictures) = glib::user_special_dir(glib::UserDirectory::Pictures) {
            dialog.set_initial_folder(Some(&gio::File::for_path(pictures)));
        }
        let Ok(file) = dialog.save_future(Some(&win)).await else {
            return;
        };
        if let Err((_, e)) = file
            .replace_contents_future(png, None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
            .await
        {
            win.toast(&e.to_string());
        }
    });
}
