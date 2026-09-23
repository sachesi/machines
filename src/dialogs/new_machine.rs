//! "New Virtual Machine": what to boot, a name, the guest's family, and how much of the
//! host it gets. An installation ISO the osinfo database knows sets the rest to what its
//! system recommends.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::domain_xml::GuestOs;
use crate::hypervisor::{CreateRequest, InstallSource};
use crate::osinfo::{self, FirmwareNeed, Os};
use crate::window::MachinesWindow;
use crate::{adw, gio, glib, gtk};

const DEFAULT_DISK_GIB: f64 = 32.0;
const WINDOWS_DISK_GIB: f64 = 64.0;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

struct Form {
    source: adw::ComboRow,
    file_row: adw::ActionRow,
    file: RefCell<Option<PathBuf>>,
    /// The system on the chosen ISO.
    detected: RefCell<Option<Os>>,
    name: adw::EntryRow,
    /// Whether the name is still the one taken from the file, and may follow it.
    name_is_derived: Cell<bool>,
    os: adw::ComboRow,
    uefi: adw::SwitchRow,
    memory: adw::SpinRow,
    vcpus: adw::SpinRow,
    disk: adw::SpinRow,
    disk_touched: Cell<bool>,
    create: gtk::Button,
    taken: Vec<String>,
}

impl Form {
    fn importing(&self) -> bool {
        self.source.selected() == 1
    }

    fn os(&self) -> GuestOs {
        match self.os.selected() {
            0 => GuestOs::Linux,
            1 => GuestOs::Windows,
            _ => GuestOs::Other,
        }
    }

    /// Take in the system found on the ISO: its family, firmware and what it recommends.
    fn detect(&self, os: Option<Os>) {
        self.os
            .set_subtitle(os.as_ref().map_or("", |os| os.name.as_str()));
        if let Some(os) = &os {
            self.os.set_selected(match os.family.as_str() {
                "linux" => 0,
                family if family.starts_with("win") => 1,
                _ => 2,
            });
            match os.firmware {
                FirmwareNeed::Uefi => self.uefi.set_active(true),
                FirmwareNeed::Bios => self.uefi.set_active(false),
                FirmwareNeed::Either => {}
            }
            let r = os.resources;
            if let Some(ram) = r.ram {
                let gib = (ram as f64 / GIB / 0.5).ceil() * 0.5;
                self.memory
                    .set_value(gib.min(self.memory.adjustment().upper()));
            }
            if let Some(cpus) = r.cpus {
                self.vcpus
                    .set_value(self.vcpus.value().max(f64::from(cpus)));
            }
            if let Some(storage) = r.storage
                && !self.disk_touched.get()
            {
                let gib = (storage as f64 / GIB).ceil();
                self.disk.set_value(gib.max(self.disk.value()));
                self.disk_touched.set(false);
            }
        }
        self.detected.replace(os);
    }

    fn valid_name(&self) -> bool {
        let name = self.name.text();
        let name = name.trim();
        !name.is_empty() && !name.contains('/') && !self.taken.iter().any(|t| t == name)
    }

    fn sync(&self) {
        self.disk.set_visible(!self.importing());
        self.file_row.set_title(&if self.importing() {
            gettext("Disk Image")
        } else {
            gettext("Installation Media")
        });
        let file = self.file.borrow();
        self.file_row.set_subtitle(&match file.as_deref() {
            Some(path) => path.to_string_lossy().into_owned(),
            None => gettext("None chosen"),
        });
        self.name.remove_css_class("error");
        if !self.name.text().is_empty() && !self.valid_name() {
            self.name.add_css_class("error");
        }
        self.create
            .set_sensitive(file.is_some() && self.valid_name());
    }

    fn request(&self) -> Option<CreateRequest> {
        let file = self.file.borrow().clone()?.to_string_lossy().into_owned();
        Some(CreateRequest {
            name: self.name.text().trim().to_owned(),
            os: self.os(),
            osinfo: self.detected.borrow().as_ref().map(|os| os.id.clone()),
            uefi: self.uefi.is_active(),
            memory_mib: (self.memory.value() * 1024.0).round() as u64,
            vcpus: self.vcpus.value() as u32,
            source: if self.importing() {
                InstallSource::Import { image: file }
            } else {
                InstallSource::Media {
                    iso: file,
                    disk_gib: self.disk.value() as u64,
                }
            },
        })
    }
}

pub fn present(win: &MachinesWindow, on_create: impl Fn(&MachinesWindow, CreateRequest) + 'static) {
    let host = win.host();
    let source = adw::ComboRow::builder()
        .title(gettext("_Install From"))
        .use_underline(true)
        .model(&gtk::StringList::new(&[
            &gettext("Installation Media (ISO)"),
            &gettext("Existing Disk Image"),
        ]))
        .build();
    let choose = gtk::Button::builder()
        .label(gettext("_Choose…"))
        .use_underline(true)
        .valign(gtk::Align::Center)
        .build();
    let file_row = adw::ActionRow::builder()
        .use_markup(false)
        .activatable_widget(&choose)
        .subtitle_selectable(true)
        .build();
    file_row.add_suffix(&choose);

    let name = adw::EntryRow::builder()
        .title(gettext("_Name"))
        .use_underline(true)
        .build();
    let os = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("_Operating System"))
        .use_underline(true)
        .model(&gtk::StringList::new(&[
            "Linux",
            "Windows",
            &gettext("Other"),
        ]))
        .build();
    let uefi = adw::SwitchRow::builder()
        .title(gettext("_UEFI Firmware"))
        .subtitle(gettext("Off, the machine boots with a BIOS"))
        .use_underline(true)
        .active(true)
        .build();

    let host_gib = (host.memory_mib as f64 / 1024.0).floor().max(1.0);
    let memory = adw::SpinRow::builder()
        .title(gettext("_Memory"))
        .subtitle(gettext("GiB"))
        .use_underline(true)
        .digits(1)
        .adjustment(&gtk::Adjustment::new(
            4.0_f64.min(host_gib),
            0.5,
            host_gib,
            0.5,
            2.0,
            0.0,
        ))
        .build();
    let vcpus = adw::SpinRow::builder()
        .title(gettext("_Processors"))
        .use_underline(true)
        .adjustment(&gtk::Adjustment::new(
            f64::from(host.cpus.min(4)),
            1.0,
            f64::from(host.cpus.max(1)),
            1.0,
            4.0,
            0.0,
        ))
        .build();
    let disk = adw::SpinRow::builder()
        .title(gettext("_Disk Size"))
        .subtitle(gettext("GiB, allocated as the guest writes"))
        .use_underline(true)
        .adjustment(&gtk::Adjustment::new(
            DEFAULT_DISK_GIB,
            1.0,
            16384.0,
            1.0,
            16.0,
            0.0,
        ))
        .build();

    let create = gtk::Button::builder()
        .label(gettext("C_reate"))
        .use_underline(true)
        .sensitive(false)
        .css_classes(["suggested-action"])
        .build();
    let cancel = gtk::Button::builder()
        .label(gettext("_Cancel"))
        .use_underline(true)
        .build();

    let form = Rc::new(Form {
        source,
        file_row,
        file: RefCell::default(),
        detected: RefCell::default(),
        name,
        name_is_derived: Cell::new(true),
        os,
        uefi,
        memory,
        vcpus,
        disk,
        disk_touched: Cell::new(false),
        create: create.clone(),
        taken: win.machine_names(),
    });

    let install = adw::PreferencesGroup::new();
    install.add(&form.source);
    install.add(&form.file_row);
    let system = adw::PreferencesGroup::builder()
        .title(gettext("System"))
        .build();
    system.add(&form.name);
    system.add(&form.os);
    system.add(&form.uefi);
    let resources = adw::PreferencesGroup::builder()
        .title(gettext("Resources"))
        .build();
    resources.add(&form.memory);
    resources.add(&form.vcpus);
    resources.add(&form.disk);
    let page = adw::PreferencesPage::new();
    page.add(&install);
    page.add(&system);
    page.add(&resources);

    let header = adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build();
    header.pack_start(&cancel);
    header.pack_end(&create);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&page));
    let dialog = adw::Dialog::builder()
        .title(gettext("New Virtual Machine"))
        .content_width(520)
        .content_height(680)
        .child(&toolbar)
        .default_widget(&create)
        .build();

    form.source.connect_selected_notify(glib::clone!(
        #[strong]
        form,
        move |_| {
            form.file.take();
            form.detect(None);
            form.sync();
        }
    ));
    form.name.connect_changed(glib::clone!(
        #[strong]
        form,
        move |_| form.sync()
    ));
    // Typing a name of one's own stops the file from renaming the machine.
    let key = gtk::EventControllerKey::new();
    key.connect_key_pressed(glib::clone!(
        #[strong]
        form,
        move |_, _, _, _| {
            form.name_is_derived.set(false);
            glib::Propagation::Proceed
        }
    ));
    form.name.add_controller(key);
    form.disk.connect_value_notify(glib::clone!(
        #[strong]
        form,
        move |_| form.disk_touched.set(true)
    ));
    form.os.connect_selected_notify(glib::clone!(
        #[strong]
        form,
        move |_| {
            if !form.disk_touched.get() {
                let size = if form.os() == GuestOs::Windows {
                    WINDOWS_DISK_GIB
                } else {
                    DEFAULT_DISK_GIB
                };
                form.disk.set_value(size);
                form.disk_touched.set(false);
            }
        }
    ));
    choose.connect_clicked(glib::clone!(
        #[strong]
        form,
        #[weak]
        dialog,
        move |_| {
            glib::spawn_future_local(glib::clone!(
                #[strong]
                form,
                #[weak]
                dialog,
                async move {
                    let Some(path) = choose_file(&dialog, form.importing()).await else {
                        return;
                    };
                    if form.name_is_derived.get() {
                        form.name.set_text(&unique_name(&path, &form.taken));
                    }
                    form.file.replace(Some(path.clone()));
                    form.detect(None);
                    form.sync();
                    if form.importing() {
                        return;
                    }
                    let iso = path.clone();
                    let os = gio::spawn_blocking(move || osinfo::identify(&iso))
                        .await
                        .ok()
                        .flatten();
                    if form.file.borrow().as_ref() == Some(&path) {
                        form.detect(os);
                    }
                }
            ));
        }
    ));
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    create.connect_clicked(glib::clone!(
        #[strong]
        form,
        #[weak]
        dialog,
        #[weak]
        win,
        move |_| {
            if let Some(request) = form.request() {
                dialog.close();
                on_create(&win, request);
            }
        }
    ));
    form.sync();
    dialog.present(Some(win));
}

pub(super) async fn choose_file(parent: &adw::Dialog, image: bool) -> Option<PathBuf> {
    let filter = gtk::FileFilter::new();
    if image {
        filter.set_name(Some(&gettext("Disk Images")));
        for suffix in ["qcow2", "img", "raw", "vmdk", "vdi", "vhd", "vhdx"] {
            filter.add_suffix(suffix);
        }
    } else {
        filter.set_name(Some(&gettext("Disc Images")));
        filter.add_suffix("iso");
        filter.add_mime_type("application/x-cd-image");
    }
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder()
        .title(if image {
            gettext("Choose a Disk Image")
        } else {
            gettext("Choose Installation Media")
        })
        .filters(&filters)
        .build();
    let window = parent.root().and_downcast::<gtk::Window>();
    dialog.open_future(window.as_ref()).await.ok()?.path()
}

/// The file's name without its extension, numbered past the names already taken.
fn unique_name(path: &Path, taken: &[String]) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "machine".to_owned());
    (1..)
        .map(|i| match i {
            1 => stem.clone(),
            i => format!("{stem}-{i}"),
        })
        .find(|n| !taken.contains(n))
        .expect("an unused name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_the_file_and_stay_unique() {
        let taken = vec!["Fedora-41".to_owned(), "Fedora-41-2".to_owned()];
        assert_eq!(
            unique_name(Path::new("/isos/Fedora-41.iso"), &taken),
            "Fedora-41-3"
        );
        assert_eq!(unique_name(Path::new("/isos/debian.iso"), &taken), "debian");
    }
}
