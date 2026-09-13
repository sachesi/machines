//! Adding hardware to a machine: disks and CD/DVD drives, network interfaces, and devices
//! of the host passed through to it.

use std::cell::RefCell;
use std::rc::Rc;

use gettextrs::gettext;

use crate::adw::prelude::*;
use crate::dialogs::{self, new_machine};
use crate::domain_xml::{self, Disk, MachineConfig, NetworkSource};
use crate::host_xml::{HostDevice, HostDeviceId, HostDisk};
use crate::hypervisor::{HostUse, MachineInfo, NewStorage, Pool, Volume};
use crate::machine_view::MachineView;
use crate::window::MachinesWindow;
use crate::{adw, glib, gtk};

const DEFAULT_DISK_GIB: f64 = 32.0;
const NIC_MODELS: [&str; 3] = ["virtio", "e1000e", "rtl8139"];

fn window(view: &MachineView) -> Option<MachinesWindow> {
    view.root().and_downcast()
}

pub async fn confirm_remove_disk(view: &MachineView, disk: &Disk) -> bool {
    let body = if disk.kind == "block" {
        gettext("A running guest loses the disk at once. What is on it is kept.")
    } else {
        gettext("A running guest loses the disk at once. Its image file is kept.")
    };
    dialogs::confirm(view, &gettext("Remove Disk?"), &body, &gettext("_Remove")).await
}

/// The machines, `machines`, whose disks include the host's `disk`.
pub fn disk_users(disk: &HostDisk, machines: &[MachineInfo]) -> Vec<String> {
    machines
        .iter()
        .filter(|m| {
            m.config.as_ref().is_some_and(|c| {
                c.disks.iter().any(|d| {
                    d.kind == "block"
                        && d.source
                            .as_deref()
                            .is_some_and(|s| s == disk.path || s == disk.block)
                })
            })
        })
        .map(|m| m.name.clone())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageKind {
    NewDisk,
    Volume,
    Image,
    HostDisk,
    Cdrom,
}

/// A disk of the host as the form offers it: why it cannot be taken, if it cannot.
struct HostDiskChoice {
    disk: HostDisk,
    unavailable: Option<String>,
    warning: Option<String>,
}

struct StorageForm {
    kinds: Vec<StorageKind>,
    kind: adw::ComboRow,
    pools: Vec<String>,
    pool: adw::ComboRow,
    size: adw::SpinRow,
    /// Volumes no machine has, with the name of their pool.
    volumes: Vec<(Volume, String)>,
    volume: adw::ComboRow,
    host_disks: Vec<HostDiskChoice>,
    host_disk: adw::ComboRow,
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
        self.host_disk.set_visible(kind == StorageKind::HostDisk);
        self.volume.set_visible(kind == StorageKind::Volume);
        if kind == StorageKind::Volume {
            let choice = self.volumes.get(self.volume.selected() as usize);
            self.volume
                .set_subtitle(&choice.map_or_else(String::new, |(vol, pool)| {
                    format!("{pool} · {}", dialogs::size(vol.capacity))
                }));
            self.add.set_sensitive(choice.is_some());
            return;
        }
        self.file_row
            .set_visible(matches!(kind, StorageKind::Image | StorageKind::Cdrom));
        if kind == StorageKind::HostDisk {
            let choice = self.host_disks.get(self.host_disk.selected() as usize);
            self.host_disk
                .set_subtitle(&choice.map_or_else(String::new, |c| {
                    let place = format!("{} · {}", dialogs::size(c.disk.size), c.disk.path);
                    let note = c.unavailable.as_ref().or(c.warning.as_ref());
                    match note {
                        Some(note) => format!("{place}\n{note}"),
                        None => place,
                    }
                }));
            self.add
                .set_sensitive(choice.is_some_and(|c| c.unavailable.is_none()));
            return;
        }
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
            StorageKind::Volume => NewStorage::PoolVolume(
                self.volumes
                    .get(self.volume.selected() as usize)?
                    .0
                    .path
                    .clone(),
            ),
            StorageKind::HostDisk => {
                let choice = self.host_disks.get(self.host_disk.selected() as usize)?;
                if choice.unavailable.is_some() {
                    return None;
                }
                NewStorage::HostDisk(choice.disk.path.clone())
            }
            StorageKind::Cdrom => NewStorage::Cdrom(self.file.borrow().clone()),
        })
    }
}

/// "Add Storage": a new disk in one of the pools, a volume or disk image that is already
/// there, a disk of the host, or a CD/DVD drive.
pub fn add_storage(view: &MachineView) {
    let Some(win) = window(view) else {
        return;
    };
    glib::spawn_future_local(glib::clone!(
        #[weak]
        view,
        async move {
            let Some(Ok((pools, disks))) = win.call(|hv| Ok((hv.pools(), hv.host_disks()))).await
            else {
                return;
            };
            let pools = match pools {
                Ok(pools) => pools,
                Err(e) => {
                    win.toast(&e);
                    Vec::new()
                }
            };
            let machines = win.machine_infos();
            let used: Vec<&str> = machines
                .iter()
                .filter_map(|m| m.config.as_ref())
                .flat_map(|c| c.disks.iter())
                .filter_map(|d| d.source.as_deref())
                .collect();
            let volumes = pools
                .iter()
                .filter(|p| p.active)
                .flat_map(|p| p.volumes.iter().map(move |v| (v.clone(), p.name.clone())))
                .filter(|(v, _)| !used.contains(&v.path.as_str()))
                .collect();
            let pools = pools
                .into_iter()
                .filter(Pool::makes_volumes)
                .map(|p| p.name)
                .collect();
            let disks = disks
                .unwrap_or_default()
                .into_iter()
                .map(|(disk, host_use)| {
                    let users = disk_users(&disk, &machines);
                    let unavailable = if host_use == HostUse::InUse {
                        Some(gettext("The host uses it"))
                    } else if !users.is_empty() {
                        Some(gettext("Used by {machines}").replace("{machines}", &users.join(", ")))
                    } else {
                        None
                    };
                    let warning = (host_use == HostUse::Unknown)
                        .then(|| gettext("Whether the host uses it cannot be checked from here"));
                    HostDiskChoice {
                        disk,
                        unavailable,
                        warning,
                    }
                })
                .collect();
            present_storage(&view, pools, volumes, disks);
        }
    ));
}

fn present_storage(
    view: &MachineView,
    pools: Vec<String>,
    volumes: Vec<(Volume, String)>,
    host_disks: Vec<HostDiskChoice>,
) {
    let mut kinds = Vec::new();
    let mut labels = Vec::new();
    if !pools.is_empty() {
        kinds.push(StorageKind::NewDisk);
        labels.push(gettext("New Disk"));
    }
    if !volumes.is_empty() {
        kinds.push(StorageKind::Volume);
        labels.push(gettext("Existing Volume"));
    }
    kinds.push(StorageKind::Image);
    labels.push(gettext("Existing Disk Image"));
    if !host_disks.is_empty() {
        kinds.push(StorageKind::HostDisk);
        labels.push(gettext("Host Disk"));
    }
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
    let volume_labels: Vec<&str> = volumes.iter().map(|(v, _)| v.name.as_str()).collect();
    let volume = adw::ComboRow::builder()
        .title(gettext("_Volume"))
        .use_underline(true)
        .model(&gtk::StringList::new(&volume_labels))
        .build();
    let disk_labels: Vec<String> = host_disks.iter().map(|c| c.disk.name()).collect();
    let disk_labels: Vec<&str> = disk_labels.iter().map(String::as_str).collect();
    let host_disk = adw::ComboRow::builder()
        .title(gettext("_Disk"))
        .use_underline(true)
        .model(&gtk::StringList::new(&disk_labels))
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
    group.add(&volume);
    group.add(&host_disk);
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
        volumes,
        volume,
        host_disks,
        host_disk,
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
    form.volume.connect_selected_notify(glib::clone!(
        #[strong]
        form,
        move |_| form.sync()
    ));
    form.host_disk.connect_selected_notify(glib::clone!(
        #[strong]
        form,
        move |_| form.sync()
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
/// "Add Host Device": opens at once, and lists the devices once the host has, which the
/// first time after the node device daemon starts can take a while.
pub fn add_host_device(view: &MachineView, config: &MachineConfig) {
    let Some(win) = window(view) else {
        return;
    };
    let attached: Vec<HostDeviceId> = config.host_devices.iter().map(|d| d.id.clone()).collect();
    let stack = gtk::Stack::new();
    stack.add_named(
        &adw::Spinner::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .width_request(32)
            .height_request(32)
            .build(),
        Some("loading"),
    );
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&stack));
    let dialog = adw::Dialog::builder()
        .title(gettext("Add Host Device"))
        .content_width(480)
        .content_height(560)
        .child(&toolbar)
        .build();
    dialog.present(Some(view));
    glib::spawn_future_local(glib::clone!(
        #[weak]
        view,
        #[weak]
        dialog,
        async move {
            let page = match win.call(|hv| hv.host_devices()).await {
                Some(Ok(devices)) => host_devices_page(&view, &dialog, &devices, &attached),
                Some(Err(e)) => adw::StatusPage::builder()
                    .icon_name("dialog-warning-symbolic")
                    .title(gettext("No Host Devices"))
                    .description(e)
                    .css_classes(["compact"])
                    .build()
                    .upcast(),
                None => return,
            };
            stack.add_named(&page, Some("devices"));
            stack.set_visible_child(&page);
        }
    ));
}

fn host_devices_page(
    view: &MachineView,
    dialog: &adw::Dialog,
    devices: &[HostDevice],
    attached: &[HostDeviceId],
) -> gtk::Widget {
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
        let xml = dev.passthrough_id(devices).hostdev_xml();
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
    page.upcast()
}

/// "USB Devices": the host's USB devices, each with a switch that plugs it into the
/// running machine or pulls it out, as a cable would, without touching the definition.
pub fn plug_usb(view: &MachineView) {
    let (Some(win), Some(info)) = (window(view), view.info()) else {
        return;
    };
    let plugged: Vec<HostDeviceId> = info
        .live
        .iter()
        .flat_map(|l| l.host_devices.iter().map(|d| d.id.clone()))
        .collect();
    let group = adw::PreferencesGroup::builder()
        .description(gettext(
            "Plugged in here, a device stays with the virtual machine until it is unplugged \
             or the machine stops. To give it one for good, add it under Host Devices in \
             Details.",
        ))
        .build();
    let page = adw::PreferencesPage::new();
    page.add(&group);
    let toast = adw::ToastOverlay::new();
    toast.set_child(Some(&page));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&toast));
    let dialog = adw::Dialog::builder()
        .title(gettext("USB Devices"))
        .content_width(480)
        .content_height(480)
        .child(&toolbar)
        .build();
    dialog.present(Some(view));
    let uuid = info.uuid.clone();
    glib::spawn_future_local(glib::clone!(
        #[weak]
        group,
        #[weak]
        toast,
        async move {
            let devices = match win.call(|hv| hv.host_devices()).await {
                Some(Ok(devices)) => devices,
                Some(Err(e)) => {
                    group.set_description(Some(&e));
                    return;
                }
                None => return,
            };
            let usb: Vec<&HostDevice> = devices
                .iter()
                .filter(|d| matches!(d.id, HostDeviceId::Usb { .. }) && d.can_pass_through())
                .collect();
            if usb.is_empty() {
                group.add(
                    &adw::ActionRow::builder()
                        .title(gettext("No USB devices"))
                        .css_classes(["dim-label"])
                        .build(),
                );
            }
            for dev in usb {
                let row = adw::SwitchRow::builder()
                    .title(device_title(dev))
                    .subtitle(device_subtitle(dev))
                    .active(plugged.iter().any(|p| p.matches(&dev.id)))
                    .build();
                let xml = dev.passthrough_id(&devices).hostdev_xml();
                // Set while the switch goes back after a failure, which is no request.
                let reverting = Rc::new(std::cell::Cell::new(false));
                let (win, uuid, toast) = (win.clone(), uuid.clone(), toast.clone());
                row.connect_active_notify(move |row| {
                    if reverting.get() {
                        return;
                    }
                    let (on, xml, uuid) = (row.is_active(), xml.clone(), uuid.clone());
                    glib::spawn_future_local(glib::clone!(
                        #[weak]
                        row,
                        #[weak]
                        toast,
                        #[strong]
                        win,
                        #[strong]
                        reverting,
                        async move {
                            if let Some(Err(e)) = win.call(move |hv| hv.plug(&uuid, &xml, on)).await
                            {
                                toast.add_toast(adw::Toast::new(&e));
                                reverting.set(true);
                                row.set_active(!on);
                                reverting.set(false);
                            }
                            win.refresh();
                        }
                    ));
                });
                group.add(&row);
            }
        }
    ));
}
