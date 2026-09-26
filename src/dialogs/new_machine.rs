//! "New Virtual Machine": what to boot, a name, the guest's family and firmware, and how
//! much of the host it gets. An installation ISO the osinfo database knows sets the rest to what its
//! system recommends.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::dialogs;
use crate::domain_xml::{Firmware, GuestOs};
use crate::hypervisor::{CreateRequest, InstallSource, Pool};
use crate::osinfo::{self, FirmwareNeed, Os};
use crate::window::MachinesWindow;
use crate::{adw, gio, glib, gtk};

const DEFAULT_DISK_GIB: f64 = 32.0;
const WINDOWS_DISK_GIB: f64 = 64.0;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// The pool new disks go in unless another is chosen, which is made if it is missing.
const DEFAULT_POOL: &str = "default";

struct Form {
    source: adw::ComboRow,
    file_row: adw::ActionRow,
    file: RefCell<Option<PathBuf>>,
    /// Why QEMU may not open the file, looked for once as it is chosen.
    file_warning: RefCell<Option<String>>,
    /// The system on the chosen ISO.
    detected: RefCell<Option<Os>>,
    name: adw::EntryRow,
    /// Whether the name is still the one taken from the file, and may follow it.
    name_is_derived: Cell<bool>,
    os: adw::ComboRow,
    firmware: adw::ComboRow,
    firmwares: Vec<Firmware>,
    memory: adw::SpinRow,
    vcpus: adw::SpinRow,
    disk: adw::SpinRow,
    disk_touched: Cell<bool>,
    pool: adw::ComboRow,
    /// The pools the pool row offers, by name, with their free bytes where known.
    pools: RefCell<Vec<(String, Option<u64>)>>,
    create: gtk::Button,
    taken: Vec<String>,
    /// Whether QEMU runs as a user of its own, who may not reach the file.
    qemu_is_other_user: bool,
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

    fn firmware(&self) -> Firmware {
        self.firmwares
            .get(self.firmware.selected() as usize)
            .copied()
            .unwrap_or(Firmware::Bios)
    }

    /// UEFI with Secure Boot where the host has it, as Windows 11 requires and libvirt
    /// itself picks for UEFI on most systems; otherwise the most the host has.
    fn default_firmware(&self) -> Firmware {
        self.firmwares.last().copied().unwrap_or(Firmware::Bios)
    }

    fn set_firmware(&self, firmware: Firmware) {
        if let Some(i) = self.firmwares.iter().position(|f| *f == firmware) {
            self.firmware.set_selected(i as u32);
        }
    }

    /// Offer the pools of `pools` that new disks can go in, the default one first.
    fn offer_pools(&self, pools: &[Pool]) {
        let mut offered = vec![(DEFAULT_POOL.to_owned(), None)];
        for pool in pools.iter().filter(|p| p.makes_volumes()) {
            if pool.name == DEFAULT_POOL {
                offered[0].1 = Some(pool.available);
            } else {
                offered.push((pool.name.clone(), Some(pool.available)));
            }
        }
        let names: Vec<&str> = offered.iter().map(|(name, _)| name.as_str()).collect();
        self.pool.set_model(Some(&gtk::StringList::new(&names)));
        self.pools.replace(offered);
        self.show_pool_space();
    }

    fn show_pool_space(&self) {
        let pools = self.pools.borrow();
        let free = pools
            .get(self.pool.selected() as usize)
            .and_then(|(_, f)| *f);
        self.pool.set_subtitle(&free.map_or(String::new(), |free| {
            gettext("{size} free").replace("{size}", &dialogs::size(free))
        }));
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
                FirmwareNeed::Uefi if self.firmware() == Firmware::Bios => {
                    self.set_firmware(self.default_firmware());
                }
                FirmwareNeed::Bios => self.set_firmware(Firmware::Bios),
                _ => {}
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
        self.pool.set_visible(!self.importing());
        self.file_row.set_title(&if self.importing() {
            gettext("Disk Image")
        } else {
            gettext("Installation Media")
        });
        let file = self.file.borrow();
        self.file_row.set_subtitle(&match file.as_deref() {
            Some(path) => {
                let shown = path.to_string_lossy().into_owned();
                match self.file_warning.borrow().as_deref() {
                    Some(warning) => format!("{shown}\n{warning}"),
                    None => shown,
                }
            }
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
            firmware: self.firmware(),
            memory_mib: (self.memory.value() * 1024.0).round() as u64,
            vcpus: self.vcpus.value() as u32,
            source: if self.importing() {
                InstallSource::Import { image: file }
            } else {
                InstallSource::Media {
                    iso: file,
                    disk_gib: self.disk.value() as u64,
                    pool: self
                        .pools
                        .borrow()
                        .get(self.pool.selected() as usize)
                        .map(|(name, _)| name.clone())
                        .filter(|name| name != DEFAULT_POOL),
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
    let mut firmwares = vec![Firmware::Bios];
    let mut firmware_labels = vec!["BIOS".to_owned()];
    if host.uefi {
        firmwares.push(Firmware::Uefi);
        firmware_labels.push("UEFI".to_owned());
    }
    if host.secure_boot {
        firmwares.push(Firmware::UefiSecureBoot);
        firmware_labels.push(gettext("UEFI with Secure Boot"));
    }
    let firmware_labels: Vec<&str> = firmware_labels.iter().map(String::as_str).collect();
    let firmware = adw::ComboRow::builder()
        .title(gettext("_Firmware"))
        .use_underline(true)
        .model(&gtk::StringList::new(&firmware_labels))
        .build();
    if !host.uefi {
        firmware.set_sensitive(false);
        firmware.set_subtitle(&gettext("QEMU has no UEFI firmware on this host"));
    }

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

    let pool = adw::ComboRow::builder()
        .use_markup(false)
        .title(gettext("Storage _Pool"))
        .use_underline(true)
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
        file_warning: RefCell::default(),
        detected: RefCell::default(),
        name,
        name_is_derived: Cell::new(true),
        os,
        firmware,
        firmwares,
        memory,
        vcpus,
        disk,
        disk_touched: Cell::new(false),
        pool,
        pools: RefCell::default(),
        create: create.clone(),
        taken: win.machine_names(),
        qemu_is_other_user: host.qemu_is_other_user,
    });

    let install = adw::PreferencesGroup::new();
    install.add(&form.source);
    install.add(&form.file_row);
    let system = adw::PreferencesGroup::builder()
        .title(gettext("System"))
        .build();
    system.add(&form.name);
    system.add(&form.os);
    system.add(&form.firmware);
    let resources = adw::PreferencesGroup::builder()
        .title(gettext("Resources"))
        .build();
    resources.add(&form.memory);
    resources.add(&form.vcpus);
    resources.add(&form.disk);
    resources.add(&form.pool);
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
        #[weak]
        form,
        move |_| {
            form.file.take();
            form.detect(None);
            form.sync();
        }
    ));
    form.name.connect_changed(glib::clone!(
        #[weak]
        form,
        move |_| form.sync()
    ));
    // Typing a name of one's own stops the file from renaming the machine.
    let key = gtk::EventControllerKey::new();
    key.connect_key_pressed(glib::clone!(
        #[weak]
        form,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, _, _, _| {
            form.name_is_derived.set(false);
            glib::Propagation::Proceed
        }
    ));
    form.name.add_controller(key);
    form.disk.connect_value_notify(glib::clone!(
        #[weak]
        form,
        move |_| form.disk_touched.set(true)
    ));
    form.os.connect_selected_notify(glib::clone!(
        #[weak]
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
        #[weak]
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
                    form.file_warning.take();
                    form.detect(None);
                    form.sync();
                    let importing = form.importing();
                    if form.qemu_is_other_user {
                        let warning = dialogs::qemu_access_warning(path.clone()).await;
                        if form.file.borrow().as_ref() == Some(&path) {
                            form.file_warning.replace(warning);
                            form.sync();
                        }
                    }
                    if importing {
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
        #[weak]
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
    form.set_firmware(form.default_firmware());
    form.offer_pools(&[]);
    form.pool.connect_selected_notify(glib::clone!(
        #[weak]
        form,
        move |_| form.show_pool_space()
    ));
    glib::spawn_future_local(glib::clone!(
        #[strong]
        form,
        #[weak]
        win,
        async move {
            if let Some(Ok(pools)) = win.call(|hv| hv.pools()).await {
                form.offer_pools(&pools);
            }
        }
    ));
    form.sync();
    // Its widgets hold the form weakly, as the form holds them, and the dialog alone holds it
    // strongly, for them all to go with the dialog.
    dialog.add_weak_ref_notify_local(move || drop(form));
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
    let title = if image {
        gettext("Choose a Disk Image")
    } else {
        gettext("Choose Installation Media")
    };
    dialogs::choose_on_host(parent, &title, Some(&filter), false)
        .await
        .map(PathBuf::from)
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
