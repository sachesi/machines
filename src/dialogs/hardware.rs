//! Adding hardware to a machine: disks and CD/DVD drives, network interfaces, and devices
//! of the host passed through to it.

use std::cell::RefCell;
use std::rc::Rc;

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::dialogs::{self, new_machine};
use crate::domain_xml::{self, MachineConfig, NetworkSource};
use crate::host_xml::{HostDevice, HostDeviceId};
use crate::hypervisor::{NewStorage, Pool};
use crate::machine_view::MachineView;
use crate::window::MachinesWindow;
use crate::{adw, glib, gtk};

const DEFAULT_DISK_GIB: f64 = 32.0;
const NIC_MODELS: [&str; 3] = ["virtio", "e1000e", "rtl8139"];

fn window(view: &MachineView) -> Option<MachinesWindow> {
    view.root().and_downcast()
}

pub async fn confirm_remove_disk(view: &MachineView) -> bool {
    dialogs::confirm(
        view,
        &gettext("Remove Disk?"),
        &gettext("A running guest loses the disk at once. Its image file is kept."),
        &gettext("_Remove"),
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageKind {
    NewDisk,
    Image,
    Cdrom,
}

struct StorageForm {
    kinds: Vec<StorageKind>,
    kind: adw::ComboRow,
    pools: Vec<String>,
    pool: adw::ComboRow,
    size: adw::SpinRow,
    file_row: adw::ActionRow,
    file: RefCell<Option<String>>,
    add: gtk::Button,
}

impl StorageForm {
    fn kind(&self) -> StorageKind {
        self.kinds[self.kind.selected() as usize]
    }

    fn sync(&self) {
        let kind = self.kind();
        self.pool.set_visible(kind == StorageKind::NewDisk);
        self.size.set_visible(kind == StorageKind::NewDisk);
        self.file_row.set_visible(kind != StorageKind::NewDisk);
        self.file_row.set_title(&if kind == StorageKind::Image {
            gettext("Disk Image")
        } else {
            gettext("Disc Image")
        });
        let file = self.file.borrow();
        self.file_row.set_subtitle(&match (file.as_deref(), kind) {
            (Some(path), _) => path.to_owned(),
            (None, StorageKind::Cdrom) => gettext("None, the drive starts empty"),
            (None, _) => gettext("None chosen"),
        });
        self.add
            .set_sensitive(kind != StorageKind::Image || file.is_some());
    }

    fn storage(&self) -> Option<NewStorage> {
        Some(match self.kind() {
            StorageKind::NewDisk => NewStorage::Volume {
                pool: self.pools.get(self.pool.selected() as usize)?.clone(),
                gib: self.size.value() as u64,
            },
            StorageKind::Image => NewStorage::Image(self.file.borrow().clone()?),
            StorageKind::Cdrom => NewStorage::Cdrom(self.file.borrow().clone()),
        })
    }
}

/// "Add Storage": a new disk in one of the pools, a disk image that is already there, or
/// a CD/DVD drive.
pub fn add_storage(view: &MachineView) {
    let Some(win) = window(view) else {
        return;
    };
    glib::spawn_future_local(glib::clone!(
        #[weak]
        view,
        async move {
            let pools = match win.call(|hv| hv.pools()).await {
                Some(Ok(pools)) => pools,
                Some(Err(e)) => {
                    win.toast(&e);
                    Vec::new()
                }
                None => return,
            };
            let pools = pools
                .into_iter()
                .filter(Pool::holds_images)
                .map(|p| p.name)
                .collect();
            present_storage(&view, pools);
        }
    ));
}

fn present_storage(view: &MachineView, pools: Vec<String>) {
    let mut kinds = Vec::new();
    let mut labels = Vec::new();
    if !pools.is_empty() {
        kinds.push(StorageKind::NewDisk);
        labels.push(gettext("New Disk"));
    }
    kinds.push(StorageKind::Image);
    labels.push(gettext("Existing Disk Image"));
    kinds.push(StorageKind::Cdrom);
    labels.push(gettext("CD/DVD Drive"));
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    let kind = adw::ComboRow::builder()
        .title(gettext("_Type"))
        .use_underline(true)
        .model(&gtk::StringList::new(&labels))
        .build();
    let pool_names: Vec<&str> = pools.iter().map(String::as_str).collect();
    let pool = adw::ComboRow::builder()
        .title(gettext("_Pool"))
        .use_underline(true)
        .model(&gtk::StringList::new(&pool_names))
        .build();
    if let Some(i) = pools.iter().position(|p| p == "default") {
        pool.set_selected(i as u32);
    }
    let size = adw::SpinRow::builder()
        .title(gettext("_Size"))
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
    let choose = gtk::Button::builder()
        .label(gettext("_Choose…"))
        .use_underline(true)
        .valign(gtk::Align::Center)
        .build();
    let file_row = adw::ActionRow::builder()
        .activatable_widget(&choose)
        .subtitle_selectable(true)
        .build();
    file_row.add_suffix(&choose);

    let group = adw::PreferencesGroup::new();
    group.add(&kind);
    group.add(&pool);
    group.add(&size);
    group.add(&file_row);
    let page = adw::PreferencesPage::new();
    page.add(&group);
    let (dialog, add) = dialogs::form(&gettext("Add Storage"), &gettext("_Add"), &page);

    let form = Rc::new(StorageForm {
        kinds,
        kind,
        pools,
        pool,
        size,
        file_row,
        file: RefCell::default(),
        add: add.clone(),
    });
    form.kind.connect_selected_notify(glib::clone!(
        #[strong]
        form,
        move |_| {
            form.file.take();
            form.sync();
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
                    let image = form.kind() == StorageKind::Image;
                    if let Some(path) = new_machine::choose_file(&dialog, image).await {
                        form.file.replace(Some(path.to_string_lossy().into_owned()));
                        form.sync();
                    }
                }
            ));
        }
    ));
    add.connect_clicked(glib::clone!(
        #[strong]
        form,
        #[weak]
        dialog,
        #[weak]
        view,
        move |_| {
            if let Some(storage) = form.storage() {
                dialog.close();
                view.change(move |hv, uuid| hv.add_storage(uuid, &storage));
            }
        }
    ));
    form.sync();
    dialog.present(Some(view));
}

/// "Add Network Interface": on one of libvirt's networks, a bridge of the host, or QEMU's
/// user networking.
pub fn add_interface(view: &MachineView, config: &MachineConfig) {
    let Some(win) = window(view) else {
        return;
    };
    let model = config
        .nics
        .iter()
        .find_map(|n| n.model.clone())
        .unwrap_or_else(|| NIC_MODELS[0].to_owned());
    glib::spawn_future_local(glib::clone!(
        #[weak]
        view,
        async move {
            // A session may have no network driver at all; it still has the other two.
            let networks = match win.call(|hv| hv.networks()).await {
                Some(Ok(networks)) => networks.into_iter().map(|n| n.name).collect(),
                Some(Err(_)) => Vec::new(),
                None => return,
            };
            present_interface(&view, networks, &model, win.is_session());
        }
    ));
}

fn present_interface(view: &MachineView, networks: Vec<String>, model: &str, session: bool) {
    let mut sources: Vec<NetworkSource> =
        networks.into_iter().map(NetworkSource::Network).collect();
    let bridge_index = sources.len();
    sources.push(NetworkSource::Bridge(String::new()));
    sources.push(NetworkSource::User);
    let labels: Vec<String> = sources
        .iter()
        .map(|s| match s {
            NetworkSource::Network(name) => {
                gettext("Virtual Network “{name}”").replace("{name}", name)
            }
            NetworkSource::Bridge(_) => gettext("Host Bridge"),
            NetworkSource::User => gettext("User Networking"),
        })
        .collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    let source = adw::ComboRow::builder()
        .title(gettext("_Connection"))
        .use_underline(true)
        .model(&gtk::StringList::new(&labels))
        .build();
    let default = if session || sources.len() == 2 {
        sources.len() - 1
    } else {
        sources
            .iter()
            .position(|s| *s == NetworkSource::Network("default".to_owned()))
            .unwrap_or(0)
    };
    source.set_selected(default as u32);
    let bridge = adw::EntryRow::builder()
        .title(gettext("_Bridge Name"))
        .use_underline(true)
        .build();
    let mut models: Vec<&str> = NIC_MODELS.to_vec();
    if !models.contains(&model) {
        models.push(model);
    }
    let model_row = adw::ComboRow::builder()
        .title(gettext("_Model"))
        .use_underline(true)
        .model(&gtk::StringList::new(&models))
        .selected(models.iter().position(|m| *m == model).unwrap_or(0) as u32)
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&source);
    group.add(&bridge);
    group.add(&model_row);
    let page = adw::PreferencesPage::new();
    page.add(&group);
    let (dialog, add) = dialogs::form(&gettext("Add Network Interface"), &gettext("_Add"), &page);

    let chosen = move |source: &adw::ComboRow, bridge: &adw::EntryRow| match sources
        .get(source.selected() as usize)?
    {
        NetworkSource::Bridge(_) => {
            let name = bridge.text().trim().to_owned();
            (!name.is_empty()).then_some(NetworkSource::Bridge(name))
        }
        other => Some(other.clone()),
    };
    let chosen = Rc::new(chosen);
    let sync = Rc::new(glib::clone!(
        #[weak]
        source,
        #[weak]
        bridge,
        #[weak]
        add,
        #[strong]
        chosen,
        move || {
            bridge.set_sensitive(source.selected() as usize == bridge_index);
            add.set_sensitive(chosen(&source, &bridge).is_some());
        }
    ));
    source.connect_selected_notify(glib::clone!(
        #[strong]
        sync,
        move |_| sync()
    ));
    bridge.connect_changed(glib::clone!(
        #[strong]
        sync,
        move |_| sync()
    ));
    add.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        view,
        #[weak]
        source,
        #[weak]
        bridge,
        #[weak]
        model_row,
        move |_| {
            let Some(network) = chosen(&source, &bridge) else {
                return;
            };
            let model = model_row
                .selected_item()
                .and_downcast::<gtk::StringObject>()
                .map(|s| s.string().to_string())
                .unwrap_or_else(|| NIC_MODELS[0].to_owned());
            dialog.close();
            let xml = domain_xml::interface_xml(&network, &model);
            view.change(move |hv, uuid| hv.attach(uuid, &xml));
        }
    ));
    sync();
    dialog.present(Some(view));
}

/// The product's name, or failing that a description of the device.
pub fn device_title(dev: &HostDevice) -> String {
    dev.product.clone().unwrap_or_else(|| match dev.id {
        HostDeviceId::Usb { .. } => gettext("USB Device"),
        HostDeviceId::Pci(_) => gettext("PCI Device"),
    })
}

/// The vendor, and where the device is.
pub fn device_subtitle(dev: &HostDevice) -> String {
    match &dev.vendor {
        Some(vendor) => format!("{vendor} · {}", dev.id),
        None => dev.id.to_string(),
    }
}

/// "Add Host Device": pick one of the host's USB or PCI devices to pass through.
pub fn add_host_device(view: &MachineView, config: &MachineConfig) {
    let Some(win) = window(view) else {
        return;
    };
    let attached: Vec<HostDeviceId> = config.host_devices.iter().map(|d| d.id.clone()).collect();
    glib::spawn_future_local(glib::clone!(
        #[weak]
        view,
        async move {
            match win.call(|hv| hv.host_devices()).await {
                Some(Ok(devices)) => present_host_devices(&view, devices, &attached),
                Some(Err(e)) => win.toast(&e),
                None => {}
            }
        }
    ));
}

fn present_host_devices(view: &MachineView, devices: Vec<HostDevice>, attached: &[HostDeviceId]) {
    let usb = adw::PreferencesGroup::builder()
        .title(gettext("USB Devices"))
        .build();
    let pci = adw::PreferencesGroup::builder()
        .title(gettext("PCI Devices"))
        .description(gettext(
            "The host cannot use a PCI device while the virtual machine has it. Passing one \
             through needs the IOMMU turned on in the firmware and the kernel.",
        ))
        .build();
    let page = adw::PreferencesPage::new();
    page.add(&usb);
    page.add(&pci);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    let dialog = adw::Dialog::builder()
        .title(gettext("Add Host Device"))
        .content_width(480)
        .content_height(560)
        .child(&toolbar)
        .build();

    let mut found = (false, false);
    for dev in devices
        .iter()
        .filter(|d| d.can_pass_through() && !attached.iter().any(|a| a.matches(&d.id)))
    {
        let row = adw::ActionRow::builder()
            .title(device_title(dev))
            .subtitle(device_subtitle(dev))
            .activatable(true)
            .build();
        row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
        let xml = dev.passthrough_id(&devices).hostdev_xml();
        row.connect_activated(glib::clone!(
            #[weak]
            dialog,
            #[weak]
            view,
            move |_| {
                dialog.close();
                let xml = xml.clone();
                view.change(move |hv, uuid| hv.attach(uuid, &xml));
            }
        ));
        match dev.id {
            HostDeviceId::Usb { .. } => {
                usb.add(&row);
                found.0 = true;
            }
            HostDeviceId::Pci(_) => {
                pci.add(&row);
                found.1 = true;
            }
        }
    }
    for (group, any) in [(&usb, found.0), (&pci, found.1)] {
        if !any {
            group.add(
                &adw::ActionRow::builder()
                    .title(gettext("None to pass through"))
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
    }
    dialog.present(Some(view));
}
