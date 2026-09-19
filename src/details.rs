//! The Details page of a machine: what it is made of, and the settings that can change
//! without editing its XML.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gettextrs::{gettext, ngettext};

use crate::adw::prelude::*;
use crate::dialogs::{self, add_button, hardware, remove_button};
use crate::domain_xml::{
    Cpu, CpuModel, Disk, DiskDevice, Display, Firmware, Gadget, GadgetDevice, HostDev,
    MachineConfig, Nic, Protocol, Snapshot, Topology, cpu_list, parse_cpu_list,
};
use crate::host_xml::HostDeviceId;
use crate::hypervisor::MachineInfo;
use crate::machine_view::MachineView;
use crate::window::MachinesWindow;
use crate::{adw, gio, glib, gtk};

/// How long a spin row has to rest before its value is saved.
const SETTLE: Duration = Duration::from_millis(700);

/// Fill two columns with the machine's settings: what it is and what it runs with in
/// `start`, the devices it has in `end`.
pub fn fill(view: &MachineView, info: &MachineInfo, start: &gtk::Box, end: &gtk::Box) {
    let Some(config) = &info.config else {
        let group = adw::PreferencesGroup::builder()
            .description(gettext(
                "The definition of this virtual machine cannot be read.",
            ))
            .build();
        start.append(&group);
        return;
    };
    let live = info.live.as_ref();
    start.append(&overview(view, info, config));
    start.append(&resources(view, info, config));
    start.append(&display(view, info, config));
    end.append(&storage(view, config, live));
    end.append(&network(view, info, config, live));
    end.append(&host_devices(view, config, live));
    end.append(&gadgets(view, config, live));
    end.append(&snapshots(view, info));
}

/// Where a device stands between the running machine and the definition it starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    No,
    /// In the definition only: the machine gets it at its next start.
    Added,
    /// Taken out of the definition, but the running machine still has it, until the guest
    /// lets go of it or the machine stops.
    Removed,
}

impl Pending {
    fn note(self) -> Option<String> {
        match self {
            Self::No => None,
            Self::Added => Some(gettext("Comes with the next start")),
            Self::Removed => Some(gettext("Being removed; the running machine still has it")),
        }
    }
}

/// The devices of the definition, `next`, marked where the running machine lacks them,
/// then those only the running machine still has.
fn with_pending<'a, T>(
    next: &'a [T],
    live: Option<&'a [T]>,
    same: impl Fn(&T, &T) -> bool,
) -> Vec<(&'a T, Pending)> {
    let mut out: Vec<(&T, Pending)> = next
        .iter()
        .map(|d| match live {
            Some(live) if !live.iter().any(|l| same(l, d)) => (d, Pending::Added),
            _ => (d, Pending::No),
        })
        .collect();
    if let Some(live) = live {
        out.extend(
            live.iter()
                .filter(|l| !next.iter().any(|d| same(l, d)))
                .map(|l| (l, Pending::Removed)),
        );
    }
    out
}

/// `subtitle`, with what `pending` says under it.
fn noted(subtitle: String, pending: Pending) -> String {
    match pending.note() {
        Some(note) => format!("{subtitle}\n{note}"),
        None => subtitle,
    }
}

fn window(view: &MachineView) -> Option<MachinesWindow> {
    view.root().and_downcast()
}

pub fn info_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .subtitle_selectable(true)
        .css_classes(["property"])
        .build()
}

fn overview(
    view: &MachineView,
    info: &MachineInfo,
    config: &MachineConfig,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Overview"))
        .build();
    if let Some(os) = config.os_id.as_deref().and_then(os_name) {
        group.add(&info_row(&gettext("Operating System"), &os));
    }
    group.add(&firmware_row(view, info, config));
    let hypervisor = match config.virt_type.as_str() {
        "kvm" => "KVM".to_owned(),
        "qemu" => gettext("QEMU (emulated)"),
        other => other.to_owned(),
    };
    group.add(&info_row(
        &gettext("Machine"),
        &format!("{hypervisor} · {} · {}", config.machine, config.arch),
    ));
    if info.persistent {
        let autostart = adw::SwitchRow::builder()
            .title(gettext("Start With the Host"))
            .subtitle(gettext(
                "Start when libvirt starts, at boot for the system connection",
            ))
            .active(info.autostart)
            .build();
        autostart.connect_active_notify(glib::clone!(
            #[weak]
            view,
            move |row| {
                let on = row.is_active();
                view.run(move |hv, uuid| hv.set_autostart(uuid, on));
            }
        ));
        group.add(&autostart);
        let boot = adw::ActionRow::builder()
            .title(gettext("Boot Order"))
            .subtitle(boot_summary(config))
            .activatable(true)
            .build();
        boot.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let running = info.state.is_active();
        boot.connect_activated(glib::clone!(
            #[weak]
            view,
            #[strong]
            config,
            move |_| dialogs::machine::boot_order(&view, &config, running)
        ));
        group.add(&boot);
    }
    group
}

fn firmware_row(view: &MachineView, info: &MachineInfo, config: &MachineConfig) -> adw::ComboRow {
    let mut choices = vec![(Firmware::Bios, "BIOS".to_owned())];
    if info.capabilities.efi || config.firmware != Firmware::Bios {
        choices.push((Firmware::Uefi, "UEFI".to_owned()));
        choices.push((Firmware::UefiSecureBoot, gettext("UEFI with Secure Boot")));
    }
    let labels: Vec<&str> = choices.iter().map(|(_, l)| l.as_str()).collect();
    let row = adw::ComboRow::builder()
        .title(gettext("Firmware"))
        .model(&gtk::StringList::new(&labels))
        .selected(
            choices
                .iter()
                .position(|(f, _)| *f == config.firmware)
                .unwrap_or(0) as u32,
        )
        .build();
    if info.state.is_active() {
        row.set_subtitle(&gettext("Changes take effect at the next start"));
    }
    row.connect_selected_notify(glib::clone!(
        #[weak]
        view,
        move |row| {
            if let Some((firmware, _)) = choices.get(row.selected() as usize) {
                let firmware = *firmware;
                view.run(move |hv, uuid| hv.set_firmware(uuid, firmware));
            }
        }
    ));
    row
}

fn boot_summary(config: &MachineConfig) -> String {
    if config.boot.is_empty() {
        return gettext("Nothing to boot from");
    }
    config
        .boot
        .iter()
        .map(|d| dialogs::machine::boot_label(config, *d).0)
        .collect::<Vec<_>>()
        .join(", ")
}

/// "http://fedoraproject.org/fedora/41" as "fedora 41".
fn os_name(id: &str) -> Option<String> {
    let path = id.split("://").nth(1)?.split_once('/')?.1;
    Some(path.replace('/', " "))
}

fn resources(
    view: &MachineView,
    info: &MachineInfo,
    config: &MachineConfig,
) -> adw::PreferencesGroup {
    let host = window(view).map(|w| w.host()).unwrap_or_default();
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Processor and Memory"))
        .build();
    if info.state.is_active() {
        group.set_description(Some(&gettext(
            "Changes take effect the next time the virtual machine starts.",
        )));
    }
    let vcpus = adw::SpinRow::builder()
        .title(gettext("Processors"))
        .adjustment(&gtk::Adjustment::new(
            f64::from(config.vcpus),
            1.0,
            f64::from(host.cpus.max(config.vcpus)),
            1.0,
            4.0,
            0.0,
        ))
        .build();
    on_settled(
        &vcpus,
        glib::clone!(
            #[weak]
            view,
            move |row| {
                let n = row.value() as u32;
                view.run(move |hv, uuid| hv.set_vcpus(uuid, n));
            }
        ),
    );
    group.add(&vcpus);
    group.add(&cpu_model_row(view, info, config));
    if let Some(row) = topology_row(view, config) {
        group.add(&row);
    }
    if let Some(row) = pins_row(view, config) {
        group.add(&row);
    }

    let gib = |mib: u64| mib as f64 / 1024.0;
    let memory = adw::SpinRow::builder()
        .title(gettext("Memory"))
        .subtitle(gettext("GiB"))
        .digits(2)
        .adjustment(&gtk::Adjustment::new(
            gib(config.memory_mib),
            gib(config.memory_mib).min(0.5),
            gib(host.memory_mib.max(config.memory_mib)),
            0.5,
            2.0,
            0.0,
        ))
        .build();
    on_settled(
        &memory,
        glib::clone!(
            #[weak]
            view,
            move |row| {
                let mib = (row.value() * 1024.0).round() as u64;
                view.run(move |hv, uuid| hv.set_memory(uuid, mib));
            }
        ),
    );
    group.add(&memory);

    let hugepages = adw::SwitchRow::builder()
        .title(gettext("Huge Pages"))
        .subtitle(gettext(
            "Memory from the huge pages the host has set aside, which it needs to start",
        ))
        .active(config.hugepages)
        .build();
    hugepages.connect_active_notify(glib::clone!(
        #[weak]
        view,
        move |row| {
            let on = row.is_active();
            view.run(move |hv, uuid| hv.set_hugepages(uuid, on));
        }
    ));
    group.add(&hugepages);
    group
}

/// Save the machine's processors as `cpu`.
fn set_cpu(view: &MachineView, cpu: Cpu) {
    view.run(move |hv, uuid| hv.set_cpu(uuid, &cpu));
}

fn cpu_model_row(view: &MachineView, info: &MachineInfo, config: &MachineConfig) -> adw::ComboRow {
    let caps = &info.capabilities;
    let current = &config.cpu.model;
    let mut choices: Vec<(CpuModel, String)> = Vec::new();
    if caps.host_passthrough || *current == CpuModel::HostPassthrough {
        choices.push((CpuModel::HostPassthrough, gettext("Same as the Host")));
    }
    if caps.host_model || *current == CpuModel::HostModel {
        choices.push((CpuModel::HostModel, gettext("Like the Host, Portable")));
    }
    choices.push((CpuModel::Default, gettext("QEMU’s Default")));
    for model in &caps.cpu_models {
        choices.push((CpuModel::Named(model.clone()), model.clone()));
    }
    match current {
        CpuModel::Named(model) if !caps.cpu_models.contains(model) => {
            choices.push((current.clone(), model.clone()));
        }
        CpuModel::Other(mode) => choices.push((current.clone(), mode.clone())),
        _ => {}
    }
    let labels: Vec<&str> = choices.iter().map(|(_, l)| l.as_str()).collect();
    let row = adw::ComboRow::builder()
        .title(gettext("Processor Model"))
        .model(&gtk::StringList::new(&labels))
        .enable_search(choices.len() > 8)
        .expression(gtk::PropertyExpression::new(
            gtk::StringObject::static_type(),
            gtk::Expression::NONE,
            "string",
        ))
        .selected(choices.iter().position(|(m, _)| m == current).unwrap_or(0) as u32)
        .build();
    let cpu = config.cpu.clone();
    row.connect_selected_notify(glib::clone!(
        #[weak]
        view,
        move |row| {
            if let Some((model, _)) = choices.get(row.selected() as usize) {
                set_cpu(
                    &view,
                    Cpu {
                        model: model.clone(),
                        ..cpu.clone()
                    },
                );
            }
        }
    ));
    row
}

/// How the processors are laid out, where there is more than one way for their count.
fn topology_row(view: &MachineView, config: &MachineConfig) -> Option<adw::ComboRow> {
    let n = config.cpu.count;
    let current = config.cpu.topology;
    if n < 2 && !matches!(current, Topology::Other { .. }) {
        return None;
    }
    let mut choices = vec![
        (
            Topology::Sockets,
            ngettext("{n} socket", "{n} sockets", n).replace("{n}", &n.to_string()),
        ),
        (
            Topology::Cores,
            ngettext("1 socket, {n} core", "1 socket, {n} cores", n).replace("{n}", &n.to_string()),
        ),
    ];
    if n.is_multiple_of(2) {
        choices.push((
            Topology::Threads,
            ngettext(
                "1 socket, {n} core, 2 threads",
                "1 socket, {n} cores, 2 threads",
                n / 2,
            )
            .replace("{n}", &(n / 2).to_string()),
        ));
    }
    if let Topology::Other {
        sockets,
        cores,
        threads,
    } = current
    {
        choices.push((
            current,
            gettext("{sockets} sockets, {cores} cores, {threads} threads")
                .replace("{sockets}", &sockets.to_string())
                .replace("{cores}", &cores.to_string())
                .replace("{threads}", &threads.to_string()),
        ));
    }
    let labels: Vec<&str> = choices.iter().map(|(_, l)| l.as_str()).collect();
    let row = adw::ComboRow::builder()
        .title(gettext("Topology"))
        .subtitle(gettext("Windows uses no more than two sockets"))
        .model(&gtk::StringList::new(&labels))
        .selected(choices.iter().position(|(t, _)| *t == current).unwrap_or(0) as u32)
        .build();
    let cpu = config.cpu.clone();
    row.connect_selected_notify(glib::clone!(
        #[weak]
        view,
        move |row| {
            if let Some((topology, _)) = choices.get(row.selected() as usize) {
                set_cpu(
                    &view,
                    Cpu {
                        topology: *topology,
                        ..cpu.clone()
                    },
                );
            }
        }
    ));
    Some(row)
}

/// The host processors the machine's run on, one each; none where they are pinned some
/// other way.
fn pins_row(view: &MachineView, config: &MachineConfig) -> Option<adw::EntryRow> {
    let pins = config.cpu.pins.as_ref()?;
    let row = adw::EntryRow::builder()
        .title(gettext("Pinned Host Processors, Like “2-5”"))
        .text(cpu_list(pins))
        .show_apply_button(true)
        .build();
    let cpu = config.cpu.clone();
    row.connect_apply(glib::clone!(
        #[weak]
        view,
        move |row| match parse_cpu_list(&row.text()) {
            Some(pins) => set_cpu(
                &view,
                Cpu {
                    pins: Some(pins),
                    ..cpu.clone()
                },
            ),
            None => {
                if let Some(win) = window(&view) {
                    win.toast(
                        &gettext("“{text}” is not a list of processors, like “2-5,8”")
                            .replace("{text}", &row.text()),
                    );
                }
            }
        }
    ));
    Some(row)
}

/// Call `f` once the row's value has stopped changing for [`SETTLE`], so dragging or
/// holding a button does not redefine the machine at every step.
fn on_settled(row: &adw::SpinRow, f: impl Fn(&adw::SpinRow) + 'static) {
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let f = Rc::new(f);
    row.connect_value_notify(move |row| {
        if let Some(source) = pending.take() {
            source.remove();
        }
        let source = glib::timeout_add_local_once(
            SETTLE,
            glib::clone!(
                #[weak]
                row,
                #[strong]
                pending,
                #[strong]
                f,
                move || {
                    pending.take();
                    f(&row);
                }
            ),
        );
        pending.replace(Some(source));
    });
}

fn storage(
    view: &MachineView,
    config: &MachineConfig,
    live: Option<&MachineConfig>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Storage"))
        .build();
    let add = add_button(&gettext("Add Storage"));
    add.connect_clicked(glib::clone!(
        #[weak]
        view,
        move |_| hardware::add_storage(&view)
    ));
    group.set_header_suffix(Some(&add));
    let live = live.map(|l| l.disks.as_slice());
    for (disk, pending) in with_pending(&config.disks, live, |a, b| a.target == b.target) {
        group.add(&disk_row(view, disk, pending));
    }
    if config.disks.is_empty() {
        group.set_description(Some(&gettext("No disks")));
    }
    group
}

fn disk_row(view: &MachineView, disk: &Disk, pending: Pending) -> adw::ActionRow {
    let title = match disk.device {
        DiskDevice::Cdrom => gettext("CD/DVD Drive"),
        DiskDevice::Floppy => gettext("Floppy Drive"),
        DiskDevice::Disk | DiskDevice::Lun if disk.kind == "block" => gettext("Host Disk"),
        DiskDevice::Disk | DiskDevice::Lun => gettext("Disk"),
    };
    let mut subtitle = disk.source.clone().unwrap_or_else(|| gettext("Empty"));
    let bus = [
        Some(disk.target.as_str()),
        Some(disk.bus.as_str()),
        disk.format.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|s| !s.is_empty())
    .collect::<Vec<_>>()
    .join(" · ");
    if !bus.is_empty() {
        subtitle = format!("{subtitle}\n{bus}");
    }
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(noted(subtitle, pending))
        .subtitle_selectable(true)
        .css_classes(["property"])
        .build();
    let remove = remove_button(&gettext("Remove"));
    remove.connect_clicked(glib::clone!(
        #[weak]
        view,
        #[strong]
        disk,
        move |_| {
            let disk = disk.clone();
            glib::spawn_future_local(glib::clone!(
                #[weak]
                view,
                async move {
                    if disk.device == DiskDevice::Cdrom
                        || hardware::confirm_remove_disk(&view, &disk).await
                    {
                        view.change(move |hv, uuid| hv.detach(uuid, &disk.xml));
                    }
                }
            ));
        }
    ));
    if pending == Pending::Removed {
        return row;
    }
    if disk.device != DiskDevice::Cdrom || pending == Pending::Added {
        row.add_suffix(&remove);
        return row;
    }
    let choose = gtk::Button::builder()
        .icon_name("document-open-symbolic")
        .tooltip_text(gettext("Insert Disc Image"))
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    choose.connect_clicked(glib::clone!(
        #[weak]
        view,
        #[strong]
        disk,
        move |_| {
            let disk = disk.clone();
            glib::spawn_future_local(glib::clone!(
                #[weak]
                view,
                async move {
                    if let Some(path) = choose_iso(&view).await {
                        view.run(move |hv, uuid| hv.change_media(uuid, &disk, Some(&path)));
                    }
                }
            ));
        }
    ));
    row.add_suffix(&choose);
    if disk.source.is_some() {
        let eject = gtk::Button::builder()
            .icon_name("media-eject-symbolic")
            .tooltip_text(gettext("Eject"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        eject.connect_clicked(glib::clone!(
            #[weak]
            view,
            #[strong]
            disk,
            move |_| {
                let disk = disk.clone();
                view.run(move |hv, uuid| hv.change_media(uuid, &disk, None));
            }
        ));
        row.add_suffix(&eject);
    }
    row.add_suffix(&remove);
    row
}

pub async fn choose_iso(parent: &impl IsA<gtk::Widget>) -> Option<String> {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(&gettext("Disc Images")));
    filter.add_suffix("iso");
    filter.add_mime_type("application/x-cd-image");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder()
        .title(gettext("Insert Disc Image"))
        .filters(&filters)
        .build();
    let window = parent.root().and_downcast::<gtk::Window>();
    let file = dialog.open_future(window.as_ref()).await.ok()?;
    file.path().map(|p| p.to_string_lossy().into_owned())
}

fn network(
    view: &MachineView,
    info: &MachineInfo,
    config: &MachineConfig,
    live: Option<&MachineConfig>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Network"))
        .build();
    let add = add_button(&gettext("Add Network Interface"));
    add.connect_clicked(glib::clone!(
        #[weak]
        view,
        #[strong]
        config,
        move |_| hardware::add_interface(&view, &config)
    ));
    group.set_header_suffix(Some(&add));
    let mut rows = Vec::new();
    let live = live.map(|l| l.nics.as_slice());
    for (nic, pending) in with_pending(&config.nics, live, |a, b| a.mac == b.mac) {
        let row = nic_row(view, nic, pending);
        group.add(&row);
        rows.push((nic.mac.clone(), row));
    }
    if config.nics.is_empty() {
        group.set_description(Some(&gettext("No network interfaces")));
    }
    if info.state.is_active() && !rows.is_empty() {
        let (Some(win), uuid) = (window(view), info.uuid.clone()) else {
            return group;
        };
        glib::spawn_future_local(async move {
            let Some(Ok(found)) = win.call(move |hv| Ok(hv.addresses(&uuid))).await else {
                return;
            };
            for (mac, row) in rows {
                let addresses: Vec<String> = found
                    .iter()
                    .filter(|a| {
                        mac.as_deref()
                            .is_none_or(|m| m.eq_ignore_ascii_case(&a.mac))
                    })
                    .flat_map(|a| a.addresses.clone())
                    .collect();
                if !addresses.is_empty() {
                    row.set_subtitle(&format!(
                        "{}\n{}",
                        row.subtitle().unwrap_or_default(),
                        addresses.join(", ")
                    ));
                }
            }
        });
    }
    group
}

fn nic_row(view: &MachineView, nic: &Nic, pending: Pending) -> adw::ActionRow {
    let source = nic.source.clone().unwrap_or_default();
    let title = match nic.kind.as_str() {
        "network" => gettext("Virtual Network “{name}”").replace("{name}", &source),
        "bridge" => gettext("Bridge {name}").replace("{name}", &source),
        "direct" => gettext("Host Device {name}").replace("{name}", &source),
        "user" => gettext("User Networking"),
        other => other.to_owned(),
    };
    let subtitle = [nic.model.as_deref(), nic.mac.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(noted(subtitle, pending))
        .subtitle_selectable(true)
        .css_classes(["property"])
        .build();
    if pending != Pending::Removed {
        row.add_suffix(&detach_button(view, &nic.xml));
    }
    row
}

/// A button that takes the device `xml` from the machine.
fn detach_button(view: &MachineView, xml: &str) -> gtk::Button {
    let button = remove_button(&gettext("Remove"));
    let xml = xml.to_owned();
    button.connect_clicked(glib::clone!(
        #[weak]
        view,
        move |_| {
            let xml = xml.clone();
            view.change(move |hv, uuid| hv.detach(uuid, &xml));
        }
    ));
    button
}

fn host_devices(
    view: &MachineView,
    config: &MachineConfig,
    live: Option<&MachineConfig>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Host Devices"))
        .build();
    let add = add_button(&gettext("Add Host Device"));
    add.connect_clicked(glib::clone!(
        #[weak]
        view,
        #[strong]
        config,
        move |_| hardware::add_host_device(&view, &config)
    ));
    group.set_header_suffix(Some(&add));
    let live = live.map(|l| l.host_devices.as_slice());
    let devices = with_pending(&config.host_devices, live, |a, b| a.id.matches(&b.id));
    if devices.is_empty() {
        group.set_description(Some(&gettext(
            "USB and PCI devices of the host passed through to the virtual machine",
        )));
        return group;
    }
    let rows: Vec<(HostDev, Pending, adw::ActionRow)> = devices
        .into_iter()
        .map(|(dev, pending)| {
            let title = match dev.id {
                HostDeviceId::Usb { .. } => gettext("USB Device"),
                HostDeviceId::Pci(_) => gettext("PCI Device"),
            };
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(noted(dev.id.to_string(), pending))
                .subtitle_selectable(true)
                .build();
            if pending != Pending::Removed {
                row.add_suffix(&detach_button(view, &dev.xml));
            }
            group.add(&row);
            (dev.clone(), pending, row)
        })
        .collect();
    // Names come from the host's own list, where it still has the device.
    if let Some(win) = window(view) {
        glib::spawn_future_local(async move {
            let Some(Ok(host)) = win.call(|hv| hv.host_devices()).await else {
                return;
            };
            for (dev, pending, row) in rows {
                if let Some(found) = host.iter().find(|h| dev.id.matches(&h.id)) {
                    row.set_title(&hardware::device_title(found));
                    row.set_subtitle(&noted(hardware::device_subtitle(found), pending));
                }
            }
        });
    }
    group
}

fn gadgets(
    view: &MachineView,
    config: &MachineConfig,
    live: Option<&MachineConfig>,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Devices"))
        .build();
    let add = add_button(&gettext("Add Device"));
    add.connect_clicked(glib::clone!(
        #[weak]
        view,
        #[strong]
        config,
        move |_| hardware::add_gadget(&view, &config)
    ));
    group.set_header_suffix(Some(&add));
    let live = live.map(|l| l.gadgets.as_slice());
    let gadgets = with_pending(&config.gadgets, live, GadgetDevice::same);
    if gadgets.is_empty() {
        group.set_description(Some(&gettext(
            "A TPM, random number generator, sound card, or folders shared with the guest",
        )));
    }
    for (device, pending) in gadgets {
        let (title, subtitle) = match &device.gadget {
            Gadget::Tpm { emulated: true } => (gettext("TPM"), gettext("Emulated, version 2.0")),
            Gadget::Tpm { emulated: false } => (gettext("TPM"), gettext("The host’s own")),
            Gadget::Rng { source } => (
                gettext("Random Number Generator"),
                source.clone().unwrap_or_default(),
            ),
            Gadget::Sound { model } => (
                gettext("Sound Card"),
                match model.as_str() {
                    "ich6" | "ich7" | "ich9" => gettext("Intel HD Audio"),
                    "ac97" => "AC’97".to_owned(),
                    "usb" => gettext("USB Audio"),
                    other => other.to_owned(),
                },
            ),
            Gadget::SharedFolder { source, tag } => (
                gettext("Shared Folder"),
                gettext("{path}\nIn the guest: mount -t virtiofs {tag} /mnt")
                    .replace("{path}", source)
                    .replace("{tag}", tag),
            ),
        };
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(noted(subtitle, pending))
            .subtitle_selectable(true)
            .css_classes(["property"])
            .build();
        if pending != Pending::Removed {
            row.add_suffix(&detach_button(view, &device.xml));
        }
        group.add(&row);
    }
    group
}

fn snapshots(view: &MachineView, info: &MachineInfo) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Snapshots"))
        .build();
    let add = add_button(&gettext("Take Snapshot"));
    add.connect_clicked(glib::clone!(
        #[weak]
        view,
        move |_| dialogs::machine::take_snapshot(&view)
    ));
    group.set_header_suffix(Some(&add));
    if info.snapshots.is_empty() {
        group.set_description(Some(&gettext(
            "Saved states of the disks, and of the memory while it runs, to go back to",
        )));
    }
    // Newest first.
    for snapshot in info.snapshots.iter().rev() {
        group.add(&snapshot_row(view, snapshot));
    }
    group
}

fn snapshot_row(view: &MachineView, snapshot: &Snapshot) -> adw::ActionRow {
    let when = glib::DateTime::from_unix_local(snapshot.created)
        .and_then(|t| t.format("%x %R"))
        .map(|t| t.to_string())
        .unwrap_or_default();
    let kind = if snapshot.running {
        gettext("With memory")
    } else {
        gettext("Disks only")
    };
    let mut subtitle = format!("{when} · {kind}");
    if let Some(description) = &snapshot.description {
        subtitle = format!("{subtitle}\n{description}");
    }
    let row = adw::ActionRow::builder()
        .title(&snapshot.name)
        .subtitle(subtitle)
        .subtitle_selectable(true)
        .build();
    if snapshot.current {
        row.add_suffix(
            &gtk::Label::builder()
                .label(gettext("Current"))
                .valign(gtk::Align::Center)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }
    let revert = gtk::Button::builder()
        .icon_name("edit-undo-symbolic")
        .tooltip_text(gettext("Revert to Snapshot"))
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let name = snapshot.name.clone();
    revert.connect_clicked(glib::clone!(
        #[weak]
        view,
        #[strong]
        name,
        move |_| dialogs::machine::revert_snapshot(&view, &name)
    ));
    row.add_suffix(&revert);
    let delete = remove_button(&gettext("Delete Snapshot"));
    delete.connect_clicked(glib::clone!(
        #[weak]
        view,
        move |_| dialogs::machine::delete_snapshot(&view, &name)
    ));
    row.add_suffix(&delete);
    row
}

fn display(
    view: &MachineView,
    info: &MachineInfo,
    config: &MachineConfig,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(gettext("Display"))
        .build();
    if info.state.is_active() {
        group.set_description(Some(&gettext(
            "Changes take effect the next time the virtual machine starts.",
        )));
    }
    let current = config.display();
    let options = &info.capabilities;
    let set = move |view: &MachineView, display: Display| {
        view.run(move |hv, uuid| hv.set_display(uuid, &display));
    };

    let mut protocols: Vec<(Protocol, String)> = Vec::new();
    for (protocol, kind, label) in [
        (Protocol::Spice, "spice", "SPICE".to_owned()),
        (Protocol::Vnc, "vnc", "VNC".to_owned()),
    ] {
        if options.graphics.iter().any(|g| g == kind) || current.protocol == protocol {
            protocols.push((protocol, label));
        }
    }
    protocols.push((Protocol::None, gettext("None")));
    let labels: Vec<&str> = protocols.iter().map(|(_, l)| l.as_str()).collect();
    let protocol = adw::ComboRow::builder()
        .title(gettext("Protocol"))
        .model(&gtk::StringList::new(&labels))
        .selected(
            protocols
                .iter()
                .position(|(p, _)| *p == current.protocol)
                .unwrap_or(0) as u32,
        )
        .build();
    protocol.set_subtitle(&match current.protocol {
        Protocol::Spice => {
            gettext("With sound, and the screen sized to the window where the guest runs its agent")
        }
        Protocol::Vnc => gettext("The screen alone, without sound"),
        Protocol::None => {
            gettext("No console, for a passed-through graphics card with its own screen")
        }
    });
    protocol.connect_selected_notify(glib::clone!(
        #[weak]
        view,
        #[strong]
        current,
        move |row| {
            if let Some((protocol, _)) = protocols.get(row.selected() as usize) {
                set(
                    &view,
                    Display {
                        protocol: *protocol,
                        ..current.clone()
                    },
                );
            }
        }
    ));
    group.add(&protocol);

    let mut models = options.video.clone();
    // The ones worth choosing first, the rest as QEMU lists them, and none last.
    let rank = |m: &String| {
        let first = ["virtio", "qxl", "vga", "bochs"]
            .iter()
            .position(|p| p == m);
        (m == "none", first.unwrap_or(usize::MAX))
    };
    models.sort_by_key(rank);
    if !models.contains(&current.video) {
        models.push(current.video.clone());
    }
    let model_labels: Vec<String> = models.iter().map(|m| video_label(m)).collect();
    let model_labels: Vec<&str> = model_labels.iter().map(String::as_str).collect();
    let video = adw::ComboRow::builder()
        .title(gettext("Video Card"))
        .model(&gtk::StringList::new(&model_labels))
        .selected(models.iter().position(|m| *m == current.video).unwrap_or(0) as u32)
        .build();
    video.connect_selected_notify(glib::clone!(
        #[weak]
        view,
        #[strong]
        current,
        move |row| {
            if let Some(model) = models.get(row.selected() as usize) {
                set(
                    &view,
                    Display {
                        video: model.clone(),
                        ..current.clone()
                    },
                );
            }
        }
    ));
    group.add(&video);

    let accel = adw::SwitchRow::builder()
        .title(gettext("3D Acceleration"))
        .subtitle(gettext(
            "Renders on the host’s graphics card; needs a virtio video card",
        ))
        .active(current.accel3d)
        .sensitive(
            current.accel3d || current.video == "virtio" && options.has_accel3d(current.protocol),
        )
        .build();
    accel.connect_active_notify(glib::clone!(
        #[weak]
        view,
        move |row| {
            set(
                &view,
                Display {
                    accel3d: row.is_active(),
                    ..current.clone()
                },
            );
        }
    ));
    group.add(&accel);
    group
}

/// "qxl" as "QXL", with the ones worth a word said what they are for.
fn video_label(model: &str) -> String {
    match model {
        "virtio" => gettext("Virtio (Linux guests)"),
        "qxl" => gettext("QXL (older SPICE guests)"),
        "vga" => gettext("VGA (any guest)"),
        "bochs" => "Bochs".to_owned(),
        "ramfb" => "Ramfb".to_owned(),
        "cirrus" => "Cirrus".to_owned(),
        "vmvga" => "VMware SVGA".to_owned(),
        "none" => gettext("None"),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_between_the_run_and_the_next_start() {
        let next = [1, 2, 3];
        let live = [2, 3, 4];
        let marked = with_pending(&next, Some(&live), |a, b| a == b);
        let marked: Vec<(i32, Pending)> = marked.into_iter().map(|(d, p)| (*d, p)).collect();
        assert_eq!(
            marked,
            [
                (1, Pending::Added),
                (2, Pending::No),
                (3, Pending::No),
                (4, Pending::Removed)
            ]
        );
        assert!(
            with_pending(&next, None, |a, b| a == b)
                .iter()
                .all(|(_, p)| *p == Pending::No)
        );
    }

    #[test]
    fn os_names_come_from_the_id_path() {
        assert_eq!(
            os_name("http://fedoraproject.org/fedora/41").as_deref(),
            Some("fedora 41")
        );
        assert_eq!(
            os_name("http://microsoft.com/win/11").as_deref(),
            Some("win 11")
        );
        assert_eq!(os_name("nonsense"), None);
    }
}
