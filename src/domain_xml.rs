//! Domain XML: what the details page reads from it, the XML of a new machine, and the
//! edits libvirt has no call for.

use std::fmt::Write;

use crate::host_xml::HostDeviceId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firmware {
    Bios,
    Uefi,
    UefiSecureBoot,
}

/// The processor the machine sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CpuModel {
    /// QEMU's own, as with no `<cpu>` mode or model at all.
    Default,
    /// The host's processor as it is, the fastest.
    HostPassthrough,
    /// A named model with what the host's processor adds to it.
    HostModel,
    Named(String),
    /// Another `<cpu mode>`, such as `maximum`, which is left as it is.
    Other(String),
}

/// How the machine's processors are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topology {
    /// A socket for each, which is what libvirt does without a topology.
    Sockets,
    /// One socket with a core for each.
    Cores,
    /// One socket with two threads to a core, where the count is even.
    Threads,
    /// Anything else, which the machine keeps until its count changes.
    Other {
        sockets: u32,
        cores: u32,
        threads: u32,
    },
}

impl Topology {
    /// Sockets, cores and threads of `count` processors laid out this way; `None` leaves
    /// the layout to libvirt.
    fn layout(self, count: u32) -> Option<(u32, u32, u32)> {
        match self {
            Self::Sockets => None,
            Self::Threads if count.is_multiple_of(2) => Some((1, count / 2, 2)),
            Self::Other {
                sockets,
                cores,
                threads,
            } if sockets * cores * threads == count => Some((sockets, cores, threads)),
            _ => Some((1, count, 1)),
        }
    }
}

/// How many host processors libvirt's CPU masks can name, 0 to 8191.
const HOST_CPU_LIMIT: u32 = 8192;

/// "2-5,8" as `[2, 3, 4, 5, 8]`, in the order given; `None` if it is no such list, or
/// names a processor past what libvirt can.
pub fn parse_cpu_list(text: &str) -> Option<Vec<u32>> {
    let mut cpus = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (first, last): (u32, u32) = match part.split_once('-') {
            Some((first, last)) => (first.trim().parse().ok()?, last.trim().parse().ok()?),
            None => {
                let cpu = part.parse().ok()?;
                (cpu, cpu)
            }
        };
        if first > last || last >= HOST_CPU_LIMIT || cpus.len() >= HOST_CPU_LIMIT as usize {
            return None;
        }
        cpus.extend(first..=last);
    }
    Some(cpus)
}

/// `[2, 3, 4, 5, 8]` as "2-5,8".
pub fn cpu_list(cpus: &[u32]) -> String {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &cpu in cpus {
        match runs.last_mut() {
            Some((_, last)) if last.checked_add(1) == Some(cpu) => *last = cpu,
            _ => runs.push((cpu, cpu)),
        }
    }
    runs.iter()
        .map(|&(first, last)| match last - first {
            0 => first.to_string(),
            1 => format!("{first},{last}"),
            _ => format!("{first}-{last}"),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// What the processors of a machine are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cpu {
    pub count: u32,
    pub model: CpuModel,
    pub topology: Topology,
    /// The host processor each of the machine's runs on, first to last, or `None` where
    /// the pinning is more than one to one, and not this app's to change.
    pub pins: Option<Vec<u32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskDevice {
    Disk,
    Cdrom,
    Floppy,
    Lun,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    pub device: DiskDevice,
    /// `file`, `block`, `volume`, `network`…
    pub kind: String,
    pub source: Option<String>,
    pub target: String,
    pub bus: String,
    pub format: Option<String>,
    /// The `<disk>` element as the definition has it, which names it to libvirt.
    pub xml: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nic {
    /// `network`, `bridge`, `user`, `direct`…
    pub kind: String,
    pub source: Option<String>,
    pub model: Option<String>,
    pub mac: Option<String>,
    /// The `<interface>` element as the definition has it.
    pub xml: String,
}

/// A USB or PCI device of the host passed through to the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDev {
    pub id: HostDeviceId,
    /// The `<hostdev>` element as the definition has it.
    pub xml: String,
}

/// A device of the kinds the details page lists together, after disks, interfaces and the
/// host's devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gadget {
    /// A TPM, emulated by swtpm, or the host's own passed through.
    Tpm {
        emulated: bool,
    },
    /// A random number generator fed from the host, from this device.
    Rng {
        source: Option<String>,
    },
    Sound {
        model: String,
    },
    /// A directory of the host the guest mounts by `tag`.
    SharedFolder {
        source: String,
        tag: String,
    },
    /// A serial port, which the serial console shows.
    Serial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GadgetDevice {
    pub gadget: Gadget,
    /// The element as the definition has it.
    pub xml: String,
}

impl GadgetDevice {
    /// Whether `other` is this device, in the definition or in the running machine.
    pub fn same(&self, other: &Self) -> bool {
        match (&self.gadget, &other.gadget) {
            (Gadget::SharedFolder { tag: a, .. }, Gadget::SharedFolder { tag: b, .. }) => a == b,
            (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b),
        }
    }
}

/// A device the firmware can boot from, by its place among the machine's devices of its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootDevice {
    Disk(usize),
    Nic(usize),
    HostDev(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineConfig {
    pub virt_type: String,
    pub arch: String,
    pub machine: String,
    pub firmware: Firmware,
    pub memory_mib: u64,
    /// Whether the memory comes in huge pages the host has set aside.
    pub hugepages: bool,
    pub vcpus: u32,
    pub cpu: Cpu,
    /// The libosinfo id virt-manager and virt-install record, e.g.
    /// `http://fedoraproject.org/fedora/41`.
    pub os_id: Option<String>,
    pub disks: Vec<Disk>,
    pub nics: Vec<Nic>,
    pub host_devices: Vec<HostDev>,
    pub gadgets: Vec<GadgetDevice>,
    /// The `type` of each graphics device, in order: `vnc`, `spice`, `dbus`…
    pub graphics: Vec<String>,
    pub video: Option<String>,
    /// Whether the video card renders 3D on the host's GPU.
    pub accel3d: bool,
    /// What the firmware tries to boot from, first to last.
    pub boot: Vec<BootDevice>,
    /// Whether it has a serial port or console to show as text.
    pub serial: bool,
}

const LIBOSINFO_NS: &str = "http://libosinfo.org/xmlns/libvirt/domain/1.0";

impl MachineConfig {
    pub fn parse(xml: &str) -> Result<Self, String> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
        let root = doc.root_element();
        let child = |name: &str| root.children().find(|n| n.has_tag_name(name));
        let os = child("os");
        let os_type = os.and_then(|os| os.children().find(|n| n.has_tag_name("type")));
        let loader_is_pflash = os
            .and_then(|os| os.children().find(|n| n.has_tag_name("loader")))
            .is_some_and(|l| l.attribute("type") == Some("pflash"));
        let secure_boot = os
            .and_then(|os| self::child(os, "loader"))
            .is_some_and(|l| l.attribute("secure") == Some("yes"))
            || os
                .and_then(|os| self::child(os, "firmware"))
                .iter()
                .flat_map(|f| f.children())
                .any(|f| {
                    f.attribute("name") == Some("secure-boot")
                        && f.attribute("enabled") == Some("yes")
                });
        let firmware =
            if os.and_then(|os| os.attribute("firmware")) == Some("efi") || loader_is_pflash {
                if secure_boot {
                    Firmware::UefiSecureBoot
                } else {
                    Firmware::Uefi
                }
            } else {
                Firmware::Bios
            };
        let memory_mib = child("memory")
            .and_then(|m| to_mib(m.text()?, m.attribute("unit").unwrap_or("KiB")))
            .unwrap_or(0);
        let vcpus = child("vcpu")
            .and_then(|v| {
                v.attribute("current")
                    .or(v.text())
                    .and_then(|n| n.trim().parse().ok())
            })
            .unwrap_or(1);
        let cpu = parse_cpu(root, vcpus);
        let hugepages = child("memoryBacking")
            .is_some_and(|m| m.children().any(|n| n.has_tag_name("hugepages")));
        let os_id = root
            .descendants()
            .find(|n| n.tag_name().namespace() == Some(LIBOSINFO_NS) && n.has_tag_name("os"))
            .and_then(|n| n.attribute("id"))
            .map(str::to_owned);

        let devices = child("devices");
        let devices = devices
            .iter()
            .flat_map(|d| d.children())
            .filter(|n| n.is_element());
        let mut disks = Vec::new();
        let mut nics = Vec::new();
        let mut host_devices = Vec::new();
        let mut gadgets = Vec::new();
        let mut graphics = Vec::new();
        let mut video = None;
        let mut accel3d = false;
        let mut boot = Vec::new();
        let mut serial = false;
        for dev in devices {
            let xml = xml[dev.range()].to_owned();
            let sub = |name: &str| dev.children().find(|n| n.has_tag_name(name));
            let order = sub("boot")
                .and_then(|b| b.attribute("order"))
                .and_then(|o| o.parse::<u32>().ok());
            let mut boots = |device| {
                if let Some(order) = order {
                    boot.push((order, device));
                }
            };
            match dev.tag_name().name() {
                "disk" => {
                    let device = match dev.attribute("device") {
                        Some("cdrom") => DiskDevice::Cdrom,
                        Some("floppy") => DiskDevice::Floppy,
                        Some("lun") => DiskDevice::Lun,
                        _ => DiskDevice::Disk,
                    };
                    let source = sub("source").and_then(|s| {
                        s.attribute("file")
                            .or(s.attribute("dev"))
                            .or(s.attribute("volume"))
                            .or(s.attribute("name"))
                            .map(str::to_owned)
                    });
                    let target = sub("target");
                    boots(BootDevice::Disk(disks.len()));
                    disks.push(Disk {
                        device,
                        kind: dev.attribute("type").unwrap_or("file").to_owned(),
                        source,
                        target: target
                            .and_then(|t| t.attribute("dev"))
                            .unwrap_or_default()
                            .to_owned(),
                        bus: target
                            .and_then(|t| t.attribute("bus"))
                            .unwrap_or_default()
                            .to_owned(),
                        format: sub("driver")
                            .and_then(|d| d.attribute("type"))
                            .map(str::to_owned),
                        xml,
                    });
                }
                "interface" => {
                    boots(BootDevice::Nic(nics.len()));
                    nics.push(Nic {
                        kind: dev.attribute("type").unwrap_or_default().to_owned(),
                        source: sub("source").and_then(|s| {
                            s.attribute("network")
                                .or(s.attribute("bridge"))
                                .or(s.attribute("dev"))
                                .map(str::to_owned)
                        }),
                        model: sub("model")
                            .and_then(|m| m.attribute("type"))
                            .map(str::to_owned),
                        mac: sub("mac")
                            .and_then(|m| m.attribute("address"))
                            .map(str::to_owned),
                        xml,
                    });
                }
                "hostdev" if dev.attribute("mode") == Some("subsystem") => {
                    if let Some(id) = HostDeviceId::from_hostdev(dev) {
                        boots(BootDevice::HostDev(host_devices.len()));
                        host_devices.push(HostDev { id, xml });
                    }
                }
                "graphics" => {
                    graphics.push(dev.attribute("type").unwrap_or_default().to_owned());
                }
                "serial" => {
                    serial = true;
                    gadgets.push(GadgetDevice {
                        gadget: Gadget::Serial,
                        xml,
                    });
                }
                "console" => serial = true,
                "tpm" => gadgets.push(GadgetDevice {
                    gadget: Gadget::Tpm {
                        emulated: sub("backend").and_then(|b| b.attribute("type"))
                            == Some("emulator"),
                    },
                    xml,
                }),
                "rng" => gadgets.push(GadgetDevice {
                    gadget: Gadget::Rng {
                        source: sub("backend")
                            .and_then(|b| b.text())
                            .map(|t| t.trim().to_owned()),
                    },
                    xml,
                }),
                "sound" => gadgets.push(GadgetDevice {
                    gadget: Gadget::Sound {
                        model: dev.attribute("model").unwrap_or_default().to_owned(),
                    },
                    xml,
                }),
                "filesystem" => {
                    let dir = |name: &str| sub(name).and_then(|n| n.attribute("dir"));
                    if let (Some(source), Some(tag)) = (dir("source"), dir("target")) {
                        gadgets.push(GadgetDevice {
                            gadget: Gadget::SharedFolder {
                                source: source.to_owned(),
                                tag: tag.to_owned(),
                            },
                            xml,
                        });
                    }
                }
                "video" if video.is_none() => {
                    let model = sub("model");
                    video = model.and_then(|m| m.attribute("type")).map(str::to_owned);
                    accel3d = model
                        .and_then(|m| m.children().find(|n| n.has_tag_name("acceleration")))
                        .is_some_and(|a| a.attribute("accel3d") == Some("yes"));
                }
                _ => {}
            }
        }

        boot.sort_by_key(|(order, _)| *order);
        let mut boot: Vec<BootDevice> = boot.into_iter().map(|(_, device)| device).collect();
        if boot.is_empty() {
            boot = legacy_boot(os, &disks, nics.len());
        }

        Ok(Self {
            virt_type: root.attribute("type").unwrap_or_default().to_owned(),
            arch: os_type
                .and_then(|t| t.attribute("arch"))
                .unwrap_or_default()
                .to_owned(),
            machine: os_type
                .and_then(|t| t.attribute("machine"))
                .unwrap_or_default()
                .to_owned(),
            firmware,
            memory_mib,
            hugepages,
            vcpus,
            cpu,
            os_id,
            disks,
            nics,
            host_devices,
            gadgets,
            graphics,
            video,
            accel3d,
            boot,
            serial,
        })
    }

    /// Image files this machine's disks (not its CD-ROMs) write to, leaving out the ones it
    /// only reads or shares with others by design.
    pub fn disk_files(&self) -> Vec<String> {
        self.disks
            .iter()
            .filter(|d| d.writable() && d.kind == "file")
            .filter_map(|d| d.source.clone())
            .collect()
    }
}

impl Disk {
    /// Whether it is a disk the machine writes to as its own: not a CD-ROM, nor marked
    /// read-only or shareable.
    pub fn writable(&self) -> bool {
        self.device == DiskDevice::Disk
            && !self.xml.contains("<readonly")
            && !self.xml.contains("<shareable")
    }
}

fn parse_cpu(root: roxmltree::Node, count: u32) -> Cpu {
    let cpu = child(root, "cpu");
    let named = cpu
        .and_then(|c| child(c, "model"))
        .and_then(|m| m.text())
        .map(|m| CpuModel::Named(m.trim().to_owned()));
    let model = match cpu.and_then(|c| c.attribute("mode")) {
        Some("host-passthrough") => CpuModel::HostPassthrough,
        Some("host-model") => CpuModel::HostModel,
        None | Some("custom") => named.unwrap_or(CpuModel::Default),
        Some(other) => CpuModel::Other(other.to_owned()),
    };
    let topology = match cpu.and_then(|c| child(c, "topology")) {
        None => Topology::Sockets,
        Some(t) => {
            let n = |name: &str| {
                t.attribute(name)
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(1)
            };
            match (
                n("sockets"),
                n("dies") * n("clusters"),
                n("cores"),
                n("threads"),
            ) {
                (s, 1, 1, 1) if s == count => Topology::Sockets,
                (1, 1, _, 1) => Topology::Cores,
                (1, 1, _, 2) => Topology::Threads,
                (sockets, _, cores, threads) => Topology::Other {
                    sockets,
                    cores,
                    threads,
                },
            }
        }
    };
    let mut pinned: Vec<(u32, Option<u32>)> = child(root, "cputune")
        .iter()
        .flat_map(|t| t.children())
        .filter(|n| n.has_tag_name("vcpupin"))
        .filter_map(|p| {
            let vcpu = p.attribute("vcpu")?.parse().ok()?;
            Some((vcpu, p.attribute("cpuset")?.parse().ok()))
        })
        .collect();
    pinned.sort_by_key(|(vcpu, _)| *vcpu);
    let pins = pinned
        .iter()
        .enumerate()
        .map(|(i, (vcpu, host))| (*vcpu == i as u32).then_some(*host).flatten())
        .collect();
    Cpu {
        count,
        model,
        topology,
        pins,
    }
}

/// The boot order `<os><boot dev=…/>` gives: the first device of each kind it names.
fn legacy_boot(os: Option<roxmltree::Node>, disks: &[Disk], nics: usize) -> Vec<BootDevice> {
    let first = |device: DiskDevice| {
        disks
            .iter()
            .position(|d| d.device == device)
            .map(BootDevice::Disk)
    };
    let mut boot = Vec::new();
    for dev in os
        .iter()
        .flat_map(|os| os.children())
        .filter(|n| n.has_tag_name("boot"))
    {
        let device = match dev.attribute("dev") {
            Some("hd") => first(DiskDevice::Disk),
            Some("cdrom") => first(DiskDevice::Cdrom),
            Some("fd") => first(DiskDevice::Floppy),
            Some("network") => (nics > 0).then_some(BootDevice::Nic(0)),
            _ => None,
        };
        if let Some(device) = device.filter(|d| !boot.contains(d)) {
            boot.push(device);
        }
    }
    boot
}

fn to_mib(value: &str, unit: &str) -> Option<u64> {
    let value: u64 = value.trim().parse().ok()?;
    let bytes: u128 = match unit {
        "b" | "bytes" => 1,
        "KB" => 1000,
        "k" | "KiB" => 1 << 10,
        "MB" => 1_000_000,
        "M" | "MiB" => 1 << 20,
        "GB" => 1_000_000_000,
        "G" | "GiB" => 1 << 30,
        "TB" => 1_000_000_000_000,
        "T" | "TiB" => 1 << 40,
        _ => return None,
    };
    u64::try_from((u128::from(value) * bytes) >> 20).ok()
}

/// Escape text for an attribute value or element content.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestOs {
    Linux,
    Windows,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkSource {
    /// A libvirt virtual network, by name.
    Network(String),
    /// A bridge the host has set up, by name.
    Bridge(String),
    /// QEMU's own user-mode networking, for sessions without a virtual network.
    User,
}

/// An `<interface>` on `source` with a network card of `model`, e.g. `virtio`.
pub fn interface_xml(source: &NetworkSource, model: &str) -> String {
    let (kind, source) = match source {
        NetworkSource::Network(name) => {
            ("network", format!("<source network='{}'/>", escape(name)))
        }
        NetworkSource::Bridge(name) => ("bridge", format!("<source bridge='{}'/>", escape(name))),
        NetworkSource::User => ("user", String::new()),
    };
    format!(
        "<interface type='{kind}'>{source}<model type='{}'/></interface>",
        escape(model)
    )
}

/// A `<disk>` reading the image `file`, or an empty CD-ROM drive.
pub fn disk_xml(
    device: DiskDevice,
    file: Option<&str>,
    format: &str,
    target: &str,
    bus: &str,
) -> String {
    let device = match device {
        DiskDevice::Cdrom => "cdrom",
        DiskDevice::Floppy => "floppy",
        DiskDevice::Lun => "lun",
        DiskDevice::Disk => "disk",
    };
    let source = file
        .map(|f| format!("<source file='{}'/>", escape(f)))
        .unwrap_or_default();
    let (discard, readonly) = if device == "cdrom" {
        ("", "<readonly/>")
    } else {
        (" discard='unmap'", "")
    };
    format!(
        "<disk type='file' device='{device}'><driver name='qemu' type='{}'{discard}/>{source}\
         <target dev='{}' bus='{}'/>{readonly}</disk>",
        escape(format),
        escape(target),
        escape(bus)
    )
}

/// A `<disk>` on the host's block device `dev`, which the guest reads and writes directly,
/// with no cache of the host's in between.
pub fn block_disk_xml(dev: &str, target: &str, bus: &str) -> String {
    format!(
        "<disk type='block' device='disk'>\
         <driver name='qemu' type='raw' cache='none' io='native' discard='unmap'/>\
         <source dev='{}'/><target dev='{}' bus='{}'/></disk>",
        escape(dev),
        escape(target),
        escape(bus)
    )
}

/// `xml` with `device` added to its devices, for the devices libvirt cannot attach.
pub fn with_device(xml: &str, device: &str) -> Result<String, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let devices = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("devices"))
        .ok_or("the domain has no devices")?;
    let mut out = xml.to_owned();
    out.insert_str(devices.range().end - "</devices>".len(), device);
    Ok(out)
}

/// `xml` without the device element `device`, as it is written there.
pub fn without_device(xml: &str, device: &str) -> Result<String, String> {
    let at = xml
        .find(device)
        .ok_or("the device is not in the definition")?;
    let mut out = xml.to_owned();
    out.replace_range(at..at + device.len(), "");
    Ok(out)
}

/// An emulated TPM 2.0, which Windows 11 wants.
pub fn tpm_xml() -> &'static str {
    "<tpm model='tpm-crb'><backend type='emulator' version='2.0'/></tpm>"
}

/// A serial port the console shows as text, which libvirt also makes the guest's console.
pub const SERIAL_XML: &str = "<serial type='pty'><target port='0'/></serial>";

/// A slot for a USB device a SPICE client redirects to the guest.
pub const REDIRDEV_XML: &str = "<redirdev bus='usb' type='spicevmc'/>";
/// How many USB devices a SPICE machine takes from the client at once, as virt-manager has it.
pub const REDIRDEV_SLOTS: usize = 2;

/// A virtio random number generator the host's `/dev/urandom` feeds.
pub fn rng_xml() -> &'static str {
    "<rng model='virtio'><backend model='random'>/dev/urandom</backend></rng>"
}

/// A sound card for `machine`: the q35 chipset's own, else the older i440fx one's.
pub fn sound_xml(machine: &str) -> String {
    let model = if machine.contains("q35") {
        "ich9"
    } else {
        "ich6"
    };
    format!("<sound model='{model}'/>")
}

/// The directory `source` of the host, shared with the guest over virtiofs by `tag`.
pub fn shared_folder_xml(source: &str, tag: &str) -> String {
    format!(
        "<filesystem type='mount' accessmode='passthrough'><driver type='virtiofs'/>\
         <source dir='{}'/><target dir='{}'/></filesystem>",
        escape(source),
        escape(tag)
    )
}

/// A mount tag for the directory `path` that none of `taken` has: its name, in letters,
/// digits, `-` and `_`.
pub fn folder_tag(path: &str, taken: &[&str]) -> String {
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let stem: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = if stem.is_empty() {
        "share".to_owned()
    } else {
        stem
    };
    (0..)
        .map(|i| match i {
            0 => stem.clone(),
            i => format!("{stem}{i}"),
        })
        .find(|t| !taken.contains(&t.as_str()))
        .expect("an unused tag")
}

/// `xml` with the memory QEMU shares with virtiofsd, which virtiofs needs, or `None` where it
/// has it already.
pub fn with_shared_memory(xml: &str) -> Result<Option<String>, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    let backing = root.children().find(|n| n.has_tag_name("memoryBacking"));
    let child = |name: &str| backing.and_then(|b| b.children().find(|n| n.has_tag_name(name)));
    if child("access").is_some_and(|a| a.attribute("mode") == Some("shared")) {
        return Ok(None);
    }
    let mut out = xml.to_owned();
    match backing {
        None => {
            let end = root.range().end - "</domain>".len();
            out.insert_str(
                end,
                "<memoryBacking><source type='memfd'/><access mode='shared'/></memoryBacking>",
            );
        }
        Some(backing) => {
            let mut added = String::new();
            if child("source").is_none() {
                added.push_str("<source type='memfd'/>");
            }
            added.push_str("<access mode='shared'/>");
            let range = backing.range();
            if xml[range.clone()].ends_with("/>") {
                out.replace_range(range, &format!("<memoryBacking>{added}</memoryBacking>"));
            } else {
                // The end first, so the access element's range still holds.
                out.insert_str(range.end - "</memoryBacking>".len(), &added);
                if let Some(access) = child("access") {
                    out.replace_range(access.range(), "");
                }
            }
        }
    }
    Ok(Some(out))
}

/// `xml` booting from `order`, first to last, and from nothing else.
///
/// The order goes on the devices themselves, as `<boot order=…/>`, which libvirt allows only
/// once `<os>` names none by kind.
pub fn set_boot_order(xml: &str, order: &[BootDevice]) -> Result<String, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    if let Some(os) = child(root, "os") {
        for boot in os.children().filter(|n| n.has_tag_name("boot")) {
            edits.push((boot.range(), String::new()));
        }
    }
    let devices = child(root, "devices").ok_or("the domain has no devices")?;
    let elements = |name: &'static str| devices.children().filter(move |n| n.has_tag_name(name));
    let disks: Vec<_> = elements("disk").collect();
    let nics: Vec<_> = elements("interface").collect();
    let host_devices: Vec<_> = elements("hostdev")
        .filter(|n| {
            n.attribute("mode") == Some("subsystem") && HostDeviceId::from_hostdev(*n).is_some()
        })
        .collect();
    for dev in disks.iter().chain(&nics).chain(&host_devices) {
        if let Some(boot) = child(*dev, "boot") {
            edits.push((boot.range(), String::new()));
        }
    }
    for (i, device) in order.iter().enumerate() {
        let dev = match *device {
            BootDevice::Disk(n) => disks.get(n),
            BootDevice::Nic(n) => nics.get(n),
            BootDevice::HostDev(n) => host_devices.get(n),
        }
        .ok_or("the device to boot from is not in the definition")?;
        let boot = format!("<boot order='{}'/>", i + 1);
        let range = dev.range();
        if xml[range.clone()].ends_with("/>") {
            let open = xml[range.start..range.end - 2].trim_end();
            let name = dev.tag_name().name();
            edits.push((range, format!("{open}>{boot}</{name}>")));
        } else {
            let end = range.end - format!("</{}>", dev.tag_name().name()).len();
            edits.push((end..end, boot));
        }
    }
    Ok(apply_edits(xml, edits))
}

/// `xml` with its processors as `cpu` says. Features, caches and NUMA cells its `<cpu>`
/// has stay, and pinning more than one to one stays where `cpu` leaves it alone.
pub fn set_cpu(xml: &str, cpu: &Cpu) -> Result<String, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    let end = root.range().end - "</domain>".len();
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut put = |node: Option<roxmltree::Node>, text: String| match node {
        Some(node) => edits.push((node.range(), text)),
        None => edits.push((end..end, text)),
    };

    put(
        child(root, "vcpu"),
        format!("<vcpu placement='static'>{}</vcpu>", cpu.count),
    );

    let old = child(root, "cpu");
    let open = match &cpu.model {
        CpuModel::Default => "<cpu>".to_owned(),
        CpuModel::HostPassthrough => {
            "<cpu mode='host-passthrough' check='none' migratable='on'>".to_owned()
        }
        CpuModel::HostModel => "<cpu mode='host-model' check='partial'>".to_owned(),
        CpuModel::Named(model) => format!(
            "<cpu mode='custom' match='exact' check='none'><model fallback='allow'>{}</model>",
            escape(model)
        ),
        CpuModel::Other(mode) => format!("<cpu mode='{}'>", escape(mode)),
    };
    let topology = cpu
        .topology
        .layout(cpu.count)
        .map(|(s, c, t)| format!("<topology sockets='{s}' cores='{c}' threads='{t}'/>"))
        .unwrap_or_default();
    let kept: String = old
        .iter()
        .flat_map(|c| c.children())
        .filter(|n| {
            n.is_element() && !["model", "topology", "vendor"].contains(&n.tag_name().name())
        })
        .map(|n| &xml[n.range()])
        .collect();
    let element = if cpu.model == CpuModel::Default && topology.is_empty() && kept.is_empty() {
        String::new()
    } else {
        format!("{open}{topology}{kept}</cpu>")
    };
    if old.is_some() || !element.is_empty() {
        put(old, element);
    }

    if let Some(pins) = &cpu.pins {
        let pins: String = pins
            .iter()
            .take(cpu.count as usize)
            .enumerate()
            .map(|(vcpu, host)| format!("<vcpupin vcpu='{vcpu}' cpuset='{host}'/>"))
            .collect();
        match child(root, "cputune") {
            None if pins.is_empty() => {}
            None => put(None, format!("<cputune>{pins}</cputune>")),
            Some(tune) => {
                let (old_pins, others): (Vec<_>, Vec<_>) = tune
                    .children()
                    .filter(|n| n.is_element())
                    .partition(|n| n.has_tag_name("vcpupin"));
                if others.is_empty() {
                    let tune_xml = if pins.is_empty() {
                        String::new()
                    } else {
                        format!("<cputune>{pins}</cputune>")
                    };
                    put(Some(tune), tune_xml);
                } else {
                    for pin in old_pins {
                        edits.push((pin.range(), String::new()));
                    }
                    let at = tune.range().end - "</cputune>".len();
                    edits.push((at..at, pins));
                }
            }
        }
    }
    Ok(apply_edits(xml, edits))
}

/// `xml` with its memory in huge pages, or out of them.
pub fn set_hugepages(xml: &str, on: bool) -> Result<String, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    let backing = child(root, "memoryBacking");
    let pages = backing.and_then(|b| child(b, "hugepages"));
    let mut edits = Vec::new();
    match (on, backing, pages) {
        (true, None, _) => {
            let end = root.range().end - "</domain>".len();
            edits.push((
                end..end,
                "<memoryBacking><hugepages/></memoryBacking>".to_owned(),
            ));
        }
        (true, Some(backing), None) => {
            let range = backing.range();
            let text = &xml[range.clone()];
            if text.ends_with("/>") {
                edits.push((
                    range,
                    "<memoryBacking><hugepages/></memoryBacking>".to_owned(),
                ));
            } else {
                let at = range.start + text.find('>').ok_or("a broken memoryBacking")? + 1;
                edits.push((at..at, "<hugepages/>".to_owned()));
            }
        }
        (false, Some(backing), Some(pages)) => {
            if backing.children().filter(|n| n.is_element()).count() == 1 {
                edits.push((backing.range(), String::new()));
            } else {
                edits.push((pages.range(), String::new()));
            }
        }
        _ => {}
    }
    Ok(apply_edits(xml, edits))
}

/// `xml` booting with `firmware`, which libvirt picks the files for.
pub fn set_firmware(xml: &str, firmware: Firmware) -> Result<String, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let os = child(doc.root_element(), "os").ok_or("the domain has no os")?;
    let range = os.range();
    let open_end = range.start + xml[range.clone()].find('>').ok_or("a broken os")? + 1;
    let mut open: String = "<os".to_owned();
    for attribute in os.attributes().filter(|a| a.name() != "firmware") {
        let _ = write!(
            open,
            " {}='{}'",
            attribute.name(),
            escape(attribute.value())
        );
    }
    open.push_str(&os_firmware(firmware));
    let mut edits = vec![(range.start..open_end, open)];
    for gone in os
        .children()
        .filter(|n| ["loader", "nvram", "firmware"].contains(&n.tag_name().name()))
    {
        edits.push((gone.range(), String::new()));
    }
    Ok(apply_edits(xml, edits))
}

/// What follows `<os` for `firmware`, up to the end of its opening tag and the features
/// libvirt picks the firmware by.
fn os_firmware(firmware: Firmware) -> String {
    let features = |secure: &str| {
        format!(
            " firmware='efi'><firmware><feature enabled='{secure}' name='enrolled-keys'/>\
             <feature enabled='{secure}' name='secure-boot'/></firmware>"
        )
    };
    match firmware {
        Firmware::Bios => ">".to_owned(),
        Firmware::Uefi => features("no"),
        Firmware::UefiSecureBoot => features("yes"),
    }
}

fn child<'a, 'i>(parent: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    parent.children().find(|n| n.has_tag_name(name))
}

/// `xml` with each range replaced, where no two ranges overlap.
fn apply_edits(xml: &str, mut edits: Vec<(std::ops::Range<usize>, String)>) -> String {
    let mut out = xml.to_owned();
    // Back to front, so the earlier ranges still point at the same text.
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    for (range, replacement) in edits {
        out.replace_range(range, &replacement);
    }
    out
}

/// A copy of the machine `xml` named `name`, with a UUID and MAC addresses of its own, and
/// firmware variables libvirt makes afresh.
///
/// `disks` pairs the element of each disk the copy changes with the image the copy reads
/// instead, or with nothing for a disk the copy goes without.
pub fn clone_xml(
    xml: &str,
    name: &str,
    disks: &[(String, Option<String>)],
) -> Result<String, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let name_node = child(root, "name").ok_or("the domain has no name")?;
    edits.push((name_node.range(), format!("<name>{}</name>", escape(name))));
    if let Some(uuid) = child(root, "uuid") {
        edits.push((uuid.range(), String::new()));
    }
    if let Some(nvram) = child(root, "os").and_then(|os| child(os, "nvram")) {
        let template = nvram
            .attribute("template")
            .map(|t| format!("<nvram template='{}'/>", escape(t)))
            .unwrap_or_default();
        edits.push((nvram.range(), template));
    }
    let devices = child(root, "devices").ok_or("the domain has no devices")?;
    for dev in devices.children().filter(|n| n.is_element()) {
        match dev.tag_name().name() {
            "interface" => {
                if let Some(mac) = child(dev, "mac") {
                    edits.push((mac.range(), String::new()));
                }
            }
            // A socket libvirt named after the machine; it makes the copy one of its own.
            "channel" if dev.attribute("type") == Some("unix") => {
                if let Some(source) = child(dev, "source") {
                    edits.push((source.range(), String::new()));
                }
            }
            "disk" => {
                let Some((_, copy)) = disks.iter().find(|(d, _)| *d == xml[dev.range()]) else {
                    continue;
                };
                let source = copy.as_ref().and_then(|copy| {
                    let attribute = child(dev, "source")?
                        .attributes()
                        .find(|a| ["file", "dev", "volume", "name"].contains(&a.name()))?;
                    Some((attribute.range_value(), escape(copy)))
                });
                match (copy, source) {
                    (Some(_), Some(source)) => edits.push(source),
                    (Some(_), None) => return Err("a disk to copy has no source".to_owned()),
                    (None, _) => edits.push((dev.range(), String::new())),
                }
            }
            _ => {}
        }
    }
    Ok(apply_edits(xml, edits))
}

/// The first device name for `bus` that none of `taken` has: `vda`, `vdb`… on virtio,
/// `sda`… on SATA and SCSI, `hda`… on IDE, and past `z`, `aa`.
pub fn next_target(bus: &str, taken: &[&str]) -> String {
    let prefix = match bus {
        "virtio" => "vd",
        "ide" => "hd",
        "xen" => "xvd",
        "fdc" => "fd",
        _ => "sd",
    };
    (0..)
        .map(|mut i: u32| {
            let mut letters = Vec::new();
            loop {
                letters.push(char::from(b'a' + (i % 26) as u8));
                if i < 26 {
                    break;
                }
                i = i / 26 - 1;
            }
            letters.reverse();
            format!("{prefix}{}", letters.into_iter().collect::<String>())
        })
        .find(|name| !taken.contains(&name.as_str()))
        .expect("an unused name")
}

#[derive(Debug, Clone)]
pub struct NewMachine {
    pub name: String,
    /// `kvm`, or `qemu` where the host has no KVM.
    pub virt_type: String,
    pub os: GuestOs,
    /// The osinfo id of the system, such as `http://fedoraproject.org/fedora/42`.
    pub osinfo: Option<String>,
    pub firmware: Firmware,
    pub tpm: bool,
    pub memory_mib: u64,
    pub vcpus: u32,
    /// Path and format (`qcow2`, `raw`) of the system disk.
    pub disk: Option<(String, String)>,
    pub cdrom: Option<String>,
    pub network: NetworkSource,
    /// A model from [`video_model`].
    pub video: String,
    /// SPICE rather than VNC, with sound and the agent channel that goes with it.
    pub spice: bool,
}

/// The XML of a machine that installs from `cdrom` onto `disk`, or boots `disk` as it is.
///
/// The display listens on no socket: the console reaches it through libvirt, and nothing
/// else on the network can.
pub fn new_machine_xml(m: &NewMachine) -> String {
    let windows = m.os == GuestOs::Windows;
    let kvm = m.virt_type == "kvm";
    let mut x = String::new();
    let _ = writeln!(x, "<domain type='{}'>", escape(&m.virt_type));
    let _ = writeln!(x, "  <name>{}</name>", escape(&m.name));
    // Where virt-manager and GNOME Boxes look for what the machine runs.
    if let Some(id) = &m.osinfo {
        let _ = writeln!(
            x,
            "  <metadata>\n    <libosinfo:libosinfo \
             xmlns:libosinfo='http://libosinfo.org/xmlns/libvirt/domain/1.0'>\n      \
             <libosinfo:os id='{}'/>\n    </libosinfo:libosinfo>\n  </metadata>",
            escape(id)
        );
    }
    let _ = writeln!(x, "  <memory unit='MiB'>{}</memory>", m.memory_mib);
    let _ = writeln!(
        x,
        "  <currentMemory unit='MiB'>{}</currentMemory>",
        m.memory_mib
    );
    let _ = writeln!(x, "  <vcpu>{}</vcpu>", m.vcpus);
    let _ = writeln!(x, "  <os{}", os_firmware(m.firmware));
    x.push_str("    <type arch='x86_64' machine='q35'>hvm</type>\n");
    if m.disk.is_some() {
        x.push_str("    <boot dev='hd'/>\n");
    }
    if m.cdrom.is_some() {
        x.push_str("    <boot dev='cdrom'/>\n");
    }
    x.push_str("  </os>\n  <features>\n    <acpi/>\n    <apic/>\n");
    if windows && kvm {
        x.push_str(
            "    <hyperv mode='custom'>\n      <relaxed state='on'/>\n      <vapic state='on'/>\n      \
             <spinlocks state='on' retries='8191'/>\n      <vpindex state='on'/>\n      \
             <synic state='on'/>\n      <stimer state='on'/>\n    </hyperv>\n",
        );
    }
    if m.firmware != Firmware::Bios {
        x.push_str("    <smm state='on'/>\n");
    }
    x.push_str("  </features>\n");
    if kvm {
        x.push_str("  <cpu mode='host-passthrough' check='none' migratable='on'/>\n");
    }
    let _ = writeln!(
        x,
        "  <clock offset='{}'>",
        if windows { "localtime" } else { "utc" }
    );
    x.push_str(
        "    <timer name='rtc' tickpolicy='catchup'/>\n    <timer name='pit' tickpolicy='delay'/>\n    \
         <timer name='hpet' present='no'/>\n",
    );
    if windows && kvm {
        x.push_str("    <timer name='hypervclock' present='yes'/>\n");
    }
    x.push_str("  </clock>\n");
    x.push_str(
        "  <on_poweroff>destroy</on_poweroff>\n  <on_reboot>restart</on_reboot>\n  \
         <on_crash>destroy</on_crash>\n  <pm>\n    <suspend-to-mem enabled='no'/>\n    \
         <suspend-to-disk enabled='no'/>\n  </pm>\n  <devices>\n",
    );
    // Windows has no virtio drivers until they are installed, so it gets SATA and e1000e.
    let (disk_dev, disk_bus) = if windows {
        ("sda", "sata")
    } else {
        ("vda", "virtio")
    };
    if let Some((path, format)) = &m.disk {
        let _ = write!(
            x,
            "    <disk type='file' device='disk'>\n      <driver name='qemu' type='{}' discard='unmap'/>\n      \
             <source file='{}'/>\n      <target dev='{disk_dev}' bus='{disk_bus}'/>\n    </disk>\n",
            escape(format),
            escape(path)
        );
    }
    if let Some(iso) = &m.cdrom {
        let _ = write!(
            x,
            "    <disk type='file' device='cdrom'>\n      <driver name='qemu' type='raw'/>\n      \
             <source file='{}'/>\n      <target dev='sdb' bus='sata'/>\n      <readonly/>\n    </disk>\n",
            escape(iso)
        );
    }
    let nic_model = if windows { "e1000e" } else { "virtio" };
    let _ = writeln!(x, "    {}", interface_xml(&m.network, nic_model));
    x.push_str(
        "    <controller type='usb' model='qemu-xhci' ports='15'/>\n    \
         <input type='tablet' bus='usb'/>\n    \
         <serial type='pty'>\n      <target port='0'/>\n    </serial>\n    \
         <console type='pty'>\n      <target type='serial' port='0'/>\n    </console>\n",
    );
    x.push_str(if m.spice {
        "    <graphics type='spice'>\n      <listen type='none'/>\n    </graphics>\n    \
         <channel type='spicevmc'>\n      <target type='virtio' name='com.redhat.spice.0'/>\n    </channel>\n    \
         <sound model='ich9'>\n      <audio id='1'/>\n    </sound>\n    \
         <audio id='1' type='spice'/>\n    \
         <redirdev bus='usb' type='spicevmc'/>\n    <redirdev bus='usb' type='spicevmc'/>\n"
    } else {
        "    <graphics type='vnc'>\n      <listen type='none'/>\n    </graphics>\n"
    });
    let _ = writeln!(
        x,
        "    <video>\n      <model type='{}'/>\n    </video>",
        escape(&m.video)
    );
    x.push_str(
        "    <channel type='unix'>\n      <target type='virtio' name='org.qemu.guest_agent.0'/>\n    </channel>\n    \
         <rng model='virtio'>\n      <backend model='random'>/dev/urandom</backend>\n    </rng>\n    \
         <memballoon model='virtio'/>\n",
    );
    if m.tpm {
        x.push_str(
            "    <tpm model='tpm-crb'>\n      <backend type='emulator' version='2.0'/>\n    </tpm>\n",
        );
    }
    x.push_str("  </devices>\n</domain>\n");
    x
}

/// The display adapter for a new machine, from the models the domain capabilities
/// `caps` list: virtio where the guest will have a driver for it, else plain VGA, else
/// whatever QEMU has.
pub fn video_model(caps: &str, os: GuestOs) -> String {
    let models = Capabilities::parse(caps).video;
    let preferred: &[&str] = match os {
        GuestOs::Windows => &["vga", "bochs"],
        _ => &["virtio", "vga", "bochs"],
    };
    preferred
        .iter()
        .find(|p| models.iter().any(|m| m == *p))
        .map(|p| (*p).to_owned())
        .or_else(|| models.into_iter().find(|m| m != "none"))
        .unwrap_or_else(|| "vga".to_owned())
}

/// The XML for `update_device` that puts `source` in the CD-ROM drive `disk`, or empties it.
pub fn cdrom_xml(disk: &Disk, source: Option<&str>) -> String {
    let source = source
        .map(|s| format!("<source file='{}'/>", escape(s)))
        .unwrap_or_default();
    format!(
        "<disk type='file' device='cdrom'><driver name='qemu' type='raw'/>{source}\
         <target dev='{}' bus='{}'/><readonly/></disk>",
        escape(&disk.target),
        escape(&disk.bus)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Vnc,
    Spice,
    /// No display at all, for a machine whose screen is a passed-through graphics card's.
    None,
}

/// How a machine shows its screen: the remote display protocol, the video card, and
/// whether the card renders 3D on the host's GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    pub protocol: Protocol,
    pub video: String,
    /// Only a virtio card has it.
    pub accel3d: bool,
}

impl MachineConfig {
    pub fn display(&self) -> Display {
        Display {
            protocol: if self.graphics.iter().any(|g| g == "spice") {
                Protocol::Spice
            } else if self.graphics.is_empty() {
                Protocol::None
            } else {
                Protocol::Vnc
            },
            video: self.video.clone().unwrap_or_else(|| "vga".to_owned()),
            accel3d: self.accel3d,
        }
    }
}

/// What QEMU can give a machine, from its domain capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Graphics types: `vnc`, `spice`, `egl-headless`…
    pub graphics: Vec<String>,
    /// Video card models: `virtio`, `qxl`, `vga`…
    pub video: Vec<String>,
    /// Whether there is UEFI firmware to boot with.
    pub efi: bool,
    pub host_passthrough: bool,
    pub host_model: bool,
    /// The CPU models QEMU can give the machine on this host.
    pub cpu_models: Vec<String>,
}

impl Capabilities {
    pub fn parse(caps: &str) -> Self {
        let Ok(doc) = roxmltree::Document::parse(caps) else {
            return Self::default();
        };
        let values = |device: &str, name: &str| -> Vec<String> {
            doc.descendants()
                .filter(|n| n.has_tag_name(device))
                .flat_map(|d| d.children())
                .find(|n| n.has_tag_name("enum") && n.attribute("name") == Some(name))
                .map(|list| {
                    list.children()
                        .filter(|n| n.has_tag_name("value"))
                        .filter_map(|n| n.text().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let cpu_mode = |name: &str| {
            doc.descendants()
                .filter(|n| n.has_tag_name("mode") && n.attribute("name") == Some(name))
                .find(|n| n.parent().is_some_and(|p| p.has_tag_name("cpu")))
        };
        let supported =
            |name: &str| cpu_mode(name).is_some_and(|m| m.attribute("supported") == Some("yes"));
        let efi = doc
            .descendants()
            .find(|n| n.has_tag_name("os"))
            .and_then(|os| {
                os.children()
                    .find(|n| n.has_tag_name("enum") && n.attribute("name") == Some("firmware"))
            })
            .is_some_and(|list| list.children().any(|v| v.text() == Some("efi")));
        let cpu_models = cpu_mode("custom")
            .iter()
            .flat_map(|m| m.children())
            .filter(|n| n.has_tag_name("model") && n.attribute("usable") == Some("yes"))
            .filter_map(|n| n.text().map(str::to_owned))
            .collect();
        Self {
            graphics: values("graphics", "type"),
            video: values("video", "modelType"),
            efi,
            host_passthrough: supported("host-passthrough"),
            host_model: supported("host-model"),
            cpu_models,
        }
    }

    /// Whether 3D acceleration can go with `protocol`: SPICE takes it in its own stream,
    /// VNC needs QEMU to read the frames back from the GPU.
    pub fn has_accel3d(&self, protocol: Protocol) -> bool {
        self.video.iter().any(|v| v == "virtio")
            && match protocol {
                Protocol::Spice => self.graphics.iter().any(|g| g == "spice"),
                Protocol::Vnc => self.graphics.iter().any(|g| g == "egl-headless"),
                Protocol::None => false,
            }
    }
}

/// `xml` with its displays and first video card replaced by what `display` says.
///
/// Both protocols listen on no socket: the console reaches them through libvirt. What only
/// works with SPICE goes with it when it goes: its agent channel, USB redirection,
/// smartcard, and audio, which becomes none; SPICE brings its agent channel and audio back.
/// With no protocol, the machine has no display at all.
pub fn set_display(xml: &str, display: &Display) -> Result<String, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let devices = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("devices"))
        .ok_or("the domain has no devices")?;
    let spice = display.protocol == Protocol::Spice;
    let accel3d =
        display.accel3d && display.video == "virtio" && display.protocol != Protocol::None;
    let gl = if accel3d { "<gl enable='yes'/>" } else { "" };
    let mut graphics = match display.protocol {
        Protocol::Spice => format!("<graphics type='spice'><listen type='none'/>{gl}</graphics>"),
        Protocol::Vnc => "<graphics type='vnc'><listen type='none'/></graphics>".to_owned(),
        Protocol::None => String::new(),
    };
    if accel3d && !spice {
        graphics.push_str("<graphics type='egl-headless'/>");
    }
    let video = format!(
        "<video><model type='{}'{}</model></video>",
        escape(&display.video),
        if accel3d {
            "><acceleration accel3d='yes'/>"
        } else {
            ">"
        }
    );

    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut graphics = Some(graphics);
    let mut video = Some(video);
    let mut has_agent = false;
    let mut redirdevs = 0;
    for dev in devices.children().filter(|n| n.is_element()) {
        let kind = dev.attribute("type");
        let spicevmc = kind == Some("spicevmc")
            || dev
                .children()
                .any(|c| c.is_element() && c.attribute("type") == Some("spicevmc"));
        match dev.tag_name().name() {
            "graphics" => edits.push((dev.range(), graphics.take().unwrap_or_default())),
            "video" if video.is_some() => {
                edits.push((dev.range(), video.take().unwrap_or_default()))
            }
            "channel" if spicevmc && spice => has_agent = true,
            "redirdev" if spicevmc && spice => redirdevs += 1,
            "channel" | "redirdev" | "smartcard" if spicevmc && !spice => {
                edits.push((dev.range(), String::new()));
            }
            "audio" if !spice && kind == Some("spice") || spice && kind == Some("none") => {
                let id = dev.attribute("id").unwrap_or("1");
                let backend = if spice { "spice" } else { "none" };
                edits.push((
                    dev.range(),
                    format!("<audio id='{}' type='{backend}'/>", escape(id)),
                ));
            }
            _ => {}
        }
    }
    let mut added: String = [graphics, video].into_iter().flatten().collect();
    if spice && !has_agent {
        added.push_str(
            "<channel type='spicevmc'><target type='virtio' name='com.redhat.spice.0'/></channel>",
        );
    }
    if spice {
        for _ in redirdevs..REDIRDEV_SLOTS {
            added.push_str(REDIRDEV_XML);
        }
    }
    let end = devices.range().end - "</devices>".len();
    edits.push((end..end, added));
    Ok(apply_edits(xml, edits))
}

/// A saved state of a machine's disks, and of its memory if it was running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub name: String,
    pub description: Option<String>,
    /// Seconds since the epoch.
    pub created: i64,
    /// Whether it has the machine's memory, so that reverting to it resumes the machine.
    pub running: bool,
    /// Whether the machine's disks go on from this snapshot.
    pub current: bool,
}

impl Snapshot {
    /// From a `<domainsnapshot>`; `current` is left for libvirt to tell.
    pub fn parse(xml: &str) -> Result<Self, String> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
        let root = doc.root_element();
        let text = |name: &str| child(root, name).and_then(|n| n.text()).map(str::trim);
        let memory = child(root, "memory").and_then(|m| m.attribute("snapshot"));
        Ok(Self {
            name: text("name").ok_or("the snapshot has no name")?.to_owned(),
            description: text("description")
                .filter(|d| !d.is_empty())
                .map(str::to_owned),
            created: text("creationTime")
                .and_then(|t| t.parse().ok())
                .unwrap_or(0),
            running: matches!(text("state"), Some("running" | "paused"))
                && memory.is_none_or(|m| m != "no"),
            current: false,
        })
    }
}

/// The XML that takes a snapshot named `name`.
pub fn snapshot_xml(name: &str, description: &str) -> String {
    let description = if description.is_empty() {
        String::new()
    } else {
        format!("<description>{}</description>", escape(description))
    };
    format!(
        "<domainsnapshot><name>{}</name>{description}</domainsnapshot>",
        escape(name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a machine virt-manager 5 created.
    const VIRT_MANAGER: &str = r#"<domain type='kvm'>
  <name>fedora41</name>
  <uuid>4f5b36fc-9b0b-4e0e-8c1b-9b2f2a7d8f01</uuid>
  <metadata>
    <libosinfo:libosinfo xmlns:libosinfo="http://libosinfo.org/xmlns/libvirt/domain/1.0">
      <libosinfo:os id="http://fedoraproject.org/fedora/41"/>
    </libosinfo:libosinfo>
  </metadata>
  <memory unit='KiB'>4194304</memory>
  <currentMemory unit='KiB'>4194304</currentMemory>
  <vcpu placement='static'>4</vcpu>
  <os firmware='efi'>
    <type arch='x86_64' machine='pc-q35-9.1'>hvm</type>
    <boot dev='hd'/>
  </os>
  <devices>
    <emulator>/usr/bin/qemu-system-x86_64</emulator>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2' discard='unmap'/>
      <source file='/var/lib/libvirt/images/fedora41.qcow2'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    <disk type='file' device='cdrom'>
      <driver name='qemu' type='raw'/>
      <target dev='sda' bus='sata'/>
      <readonly/>
    </disk>
    <interface type='network'>
      <mac address='52:54:00:12:34:56'/>
      <source network='default'/>
      <model type='virtio'/>
    </interface>
    <channel type='spicevmc'>
      <target type='virtio' name='com.redhat.spice.0'/>
    </channel>
    <graphics type='spice' autoport='yes'>
      <listen type='address'/>
      <image compression='off'/>
    </graphics>
    <sound model='ich9'>
      <audio id='1'/>
    </sound>
    <audio id='1' type='spice'/>
    <video>
      <model type='virtio' heads='1' primary='yes'/>
    </video>
    <redirdev bus='usb' type='spicevmc'/>
    <redirdev bus='usb' type='spicevmc'/>
  </devices>
</domain>
"#;

    #[test]
    fn reads_what_virt_manager_writes() {
        let c = MachineConfig::parse(VIRT_MANAGER).unwrap();
        assert_eq!(c.virt_type, "kvm");
        assert_eq!(c.machine, "pc-q35-9.1");
        assert_eq!(c.firmware, Firmware::Uefi);
        assert_eq!(c.memory_mib, 4096);
        assert_eq!(c.vcpus, 4);
        assert_eq!(
            c.os_id.as_deref(),
            Some("http://fedoraproject.org/fedora/41")
        );
        assert_eq!(c.disks.len(), 2);
        assert_eq!(c.disks[1].device, DiskDevice::Cdrom);
        assert_eq!(c.disks[1].source, None);
        assert_eq!(c.disk_files(), ["/var/lib/libvirt/images/fedora41.qcow2"]);
        assert_eq!(c.nics[0].source.as_deref(), Some("default"));
        assert_eq!(c.nics[0].mac.as_deref(), Some("52:54:00:12:34:56"));
        assert_eq!(c.graphics, ["spice"]);
        assert_eq!(c.video.as_deref(), Some("virtio"));
    }

    #[test]
    fn shared_and_read_only_images_are_not_the_machines_own() {
        let xml = VIRT_MANAGER.replace(
            "<interface",
            "<disk type='file' device='disk'><source file='/srv/base.img'/>\
             <target dev='vdb' bus='virtio'/><readonly/></disk>\
             <disk type='file' device='disk'><source file='/srv/cluster.img'/>\
             <target dev='vdc' bus='virtio'/><shareable/></disk><interface",
        );
        let c = MachineConfig::parse(&xml).unwrap();
        assert_eq!(c.disks.len(), 4);
        assert_eq!(c.disk_files(), ["/var/lib/libvirt/images/fedora41.qcow2"]);
    }

    #[test]
    fn current_vcpus_win_over_the_maximum() {
        let c = MachineConfig::parse(
            "<domain type='qemu'><vcpu current='2'>8</vcpu><memory unit='GiB'>2</memory></domain>",
        )
        .unwrap();
        assert_eq!(c.vcpus, 2);
        assert_eq!(c.memory_mib, 2048);
    }

    fn vnc(video: &str, accel3d: bool) -> Display {
        Display {
            protocol: Protocol::Vnc,
            video: video.into(),
            accel3d,
        }
    }

    #[test]
    fn spice_becomes_vnc_and_its_devices_go() {
        let c = MachineConfig::parse(VIRT_MANAGER).unwrap();
        let kept = set_display(VIRT_MANAGER, &c.display()).unwrap();
        assert_eq!(kept.matches("<redirdev").count(), REDIRDEV_SLOTS, "{kept}");
        let xml = set_display(VIRT_MANAGER, &vnc("virtio", false)).unwrap();
        let back = MachineConfig::parse(&xml).unwrap();
        assert_eq!(back.graphics, ["vnc"]);
        assert!(!xml.contains("spicevmc"), "{xml}");
        assert!(xml.contains("<audio id='1' type='none'/>"), "{xml}");
        // The sound card still points at an audio backend that exists.
        assert!(xml.contains("<audio id='1'/>"), "{xml}");
        assert_eq!(back.disks, c.disks);

        let spice = Display {
            protocol: Protocol::Spice,
            ..back.display()
        };
        let again = set_display(&xml, &spice).unwrap();
        let back = MachineConfig::parse(&again).unwrap();
        assert_eq!(back.display(), spice);
        assert!(again.contains("<audio id='1' type='spice'/>"), "{again}");
        assert_eq!(again.matches("com.redhat.spice.0").count(), 1, "{again}");
        assert_eq!(
            again.matches("<redirdev").count(),
            REDIRDEV_SLOTS,
            "{again}"
        );
    }

    #[test]
    fn accel3d_takes_a_virtio_card() {
        let xml = VIRT_MANAGER.replace(
            "<video>",
            "<graphics type='vnc'><listen type='none'/></graphics>\n    <video>",
        );
        let accel = set_display(&xml, &vnc("virtio", true)).unwrap();
        let c = MachineConfig::parse(&accel).unwrap();
        assert_eq!(c.graphics, ["vnc", "egl-headless"]);
        assert_eq!(c.display(), vnc("virtio", true));

        let spice = Display {
            protocol: Protocol::Spice,
            ..c.display()
        };
        let c = MachineConfig::parse(&set_display(&accel, &spice).unwrap()).unwrap();
        assert_eq!(c.graphics, ["spice"]);
        assert!(c.accel3d);

        let qxl = MachineConfig::parse(&set_display(&accel, &vnc("qxl", true)).unwrap()).unwrap();
        assert_eq!(qxl.display(), vnc("qxl", false));
        assert_eq!(qxl.graphics, ["vnc"]);
    }

    #[test]
    fn a_machine_can_go_without_a_display() {
        let none = Display {
            protocol: Protocol::None,
            video: "none".into(),
            accel3d: true,
        };
        let xml = set_display(VIRT_MANAGER, &none).unwrap();
        let c = MachineConfig::parse(&xml).unwrap();
        assert!(c.graphics.is_empty(), "{xml}");
        assert!(!xml.contains("spicevmc"), "{xml}");
        assert_eq!(
            c.display(),
            Display {
                accel3d: false,
                ..none
            }
        );
        let back = set_display(&xml, &vnc("vga", false)).unwrap();
        assert_eq!(
            MachineConfig::parse(&back).unwrap().display(),
            vnc("vga", false)
        );
    }

    #[test]
    fn capabilities_come_from_the_capabilities() {
        let caps = "<domainCapabilities><devices>\
            <graphics supported='yes'><enum name='type'><value>vnc</value>\
            <value>egl-headless</value></enum></graphics>\
            <video supported='yes'><enum name='modelType'><value>vga</value>\
            <value>virtio</value></enum></video></devices></domainCapabilities>";
        let options = Capabilities::parse(caps);
        assert_eq!(options.graphics, ["vnc", "egl-headless"]);
        assert_eq!(options.video, ["vga", "virtio"]);
        assert!(options.has_accel3d(Protocol::Vnc));
        assert!(!options.has_accel3d(Protocol::Spice));
    }

    fn new_machine(os: GuestOs) -> NewMachine {
        NewMachine {
            name: "Tom & Jerry's <box>".into(),
            virt_type: "kvm".into(),
            os,
            osinfo: Some("http://fedoraproject.org/fedora/42".into()),
            firmware: Firmware::Uefi,
            tpm: false,
            memory_mib: 4096,
            vcpus: 2,
            disk: Some(("/images/a'b.qcow2".into(), "qcow2".into())),
            cdrom: Some("/isos/install.iso".into()),
            network: NetworkSource::Network("default".into()),
            video: video_model("", os),
            spice: false,
        }
    }

    #[test]
    fn new_machine_reads_back() {
        let xml = new_machine_xml(&new_machine(GuestOs::Linux));
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let name = doc.descendants().find(|n| n.has_tag_name("name")).unwrap();
        assert_eq!(name.text(), Some("Tom & Jerry's <box>"));
        let os = doc
            .descendants()
            .find(|n| n.has_tag_name(("http://libosinfo.org/xmlns/libvirt/domain/1.0", "os")))
            .unwrap();
        assert_eq!(
            os.attribute("id"),
            Some("http://fedoraproject.org/fedora/42")
        );
        let c = MachineConfig::parse(&xml).unwrap();
        assert_eq!(c.firmware, Firmware::Uefi);
        assert_eq!(c.memory_mib, 4096);
        assert_eq!(c.vcpus, 2);
        assert_eq!(c.disk_files(), ["/images/a'b.qcow2"]);
        assert_eq!(c.disks[0].bus, "virtio");
        assert_eq!(c.disks[1].source.as_deref(), Some("/isos/install.iso"));
        assert_eq!(c.graphics, ["vnc"]);
        assert_eq!(c.nics[0].model.as_deref(), Some("virtio"));
        assert!(c.serial);
        assert!(!MachineConfig::parse(VIRT_MANAGER).unwrap().serial);

        let spice = NewMachine {
            spice: true,
            ..new_machine(GuestOs::Linux)
        };
        let xml = new_machine_xml(&spice);
        let c = MachineConfig::parse(&xml).unwrap();
        assert_eq!(c.display().protocol, Protocol::Spice);
        // What switching to SPICE would add, it has already.
        assert_eq!(
            set_display(&xml, &c.display())
                .unwrap()
                .matches("spicevmc")
                .count(),
            1 + REDIRDEV_SLOTS
        );
    }

    #[test]
    fn new_machines_boot_the_firmware_chosen() {
        for firmware in [Firmware::Bios, Firmware::Uefi, Firmware::UefiSecureBoot] {
            let xml = new_machine_xml(&NewMachine {
                firmware,
                ..new_machine(GuestOs::Linux)
            });
            assert_eq!(
                MachineConfig::parse(&xml).unwrap().firmware,
                firmware,
                "{xml}"
            );
            assert_eq!(xml.contains("<smm"), firmware != Firmware::Bios, "{xml}");
        }
    }

    #[test]
    fn windows_gets_devices_it_has_drivers_for() {
        let c = MachineConfig::parse(&new_machine_xml(&new_machine(GuestOs::Windows))).unwrap();
        assert_eq!(c.disks[0].bus, "sata");
        assert_eq!(c.nics[0].model.as_deref(), Some("e1000e"));
        assert_eq!(c.video.as_deref(), Some("vga"));
    }

    #[test]
    fn video_follows_what_qemu_has() {
        let caps = |models: &str| {
            format!(
                "<domainCapabilities><devices><video supported='yes'><enum name='modelType'>{models}</enum></video></devices></domainCapabilities>"
            )
        };
        let full = caps("<value>vga</value><value>virtio</value><value>bochs</value>");
        assert_eq!(video_model(&full, GuestOs::Linux), "virtio");
        assert_eq!(video_model(&full, GuestOs::Windows), "vga");
        let core = caps("<value>vga</value><value>cirrus</value><value>none</value>");
        assert_eq!(video_model(&core, GuestOs::Linux), "vga");
        assert_eq!(
            video_model(
                &caps("<value>none</value><value>ramfb</value>"),
                GuestOs::Other
            ),
            "ramfb"
        );
    }

    #[test]
    fn targets_take_the_next_free_letter() {
        assert_eq!(next_target("virtio", &["vda", "sda"]), "vdb");
        assert_eq!(next_target("sata", &["vda", "sda"]), "sdb");
        assert_eq!(next_target("ide", &[]), "hda");
        let full: Vec<String> = (b'a'..=b'z').map(|c| format!("vd{}", c as char)).collect();
        let full: Vec<&str> = full.iter().map(String::as_str).collect();
        assert_eq!(next_target("virtio", &full), "vdaa");
    }

    #[test]
    fn added_devices_read_back() {
        let disk = disk_xml(
            DiskDevice::Disk,
            Some("/i/b.qcow2"),
            "qcow2",
            "vdb",
            "virtio",
        );
        let cdrom = disk_xml(DiskDevice::Cdrom, None, "raw", "sdc", "sata");
        let block = block_disk_xml("/dev/disk/by-id/ata-X", "vdc", "virtio");
        let nic = interface_xml(&NetworkSource::Bridge("br0".into()), "e1000e");
        let usb = crate::host_xml::HostDeviceId::Usb {
            vendor: 0x046d,
            product: 0xc52b,
            address: None,
        };
        let xml = format!(
            "<domain><devices>{disk}{cdrom}{block}{nic}{}</devices></domain>",
            usb.hostdev_xml()
        );
        let c = MachineConfig::parse(&xml).unwrap();
        assert_eq!(c.disks[0].source.as_deref(), Some("/i/b.qcow2"));
        assert_eq!(c.disks[0].xml, disk);
        assert_eq!(c.disks[1].device, DiskDevice::Cdrom);
        assert_eq!(c.disks[1].source, None);
        assert_eq!(c.disks[2].kind, "block");
        assert_eq!(c.disks[2].source.as_deref(), Some("/dev/disk/by-id/ata-X"));
        assert_eq!(c.disks[2].format.as_deref(), Some("raw"));
        assert_eq!(c.nics[0].kind, "bridge");
        assert_eq!(c.nics[0].source.as_deref(), Some("br0"));
        assert_eq!(c.nics[0].xml, nic);
        assert_eq!(c.host_devices[0].id, usb);
    }

    #[test]
    fn gadgets_read_back() {
        let folder = shared_folder_xml("/home/me/Shared Stuff", "Shared_Stuff");
        let xml = format!(
            "<domain><devices>{}{}{}{folder}{SERIAL_XML}</devices></domain>",
            tpm_xml(),
            rng_xml(),
            sound_xml("pc-q35-9.1")
        );
        let c = MachineConfig::parse(&xml).unwrap();
        let gadgets: Vec<&Gadget> = c.gadgets.iter().map(|g| &g.gadget).collect();
        assert_eq!(
            gadgets,
            [
                &Gadget::Tpm { emulated: true },
                &Gadget::Rng {
                    source: Some("/dev/urandom".into())
                },
                &Gadget::Sound {
                    model: "ich9".into()
                },
                &Gadget::SharedFolder {
                    source: "/home/me/Shared Stuff".into(),
                    tag: "Shared_Stuff".into()
                },
                &Gadget::Serial,
            ]
        );
        assert_eq!(c.gadgets[3].xml, folder);
        assert_eq!(folder_tag("/home/me/Shared Stuff/", &[]), "Shared_Stuff");
        assert_eq!(folder_tag("/srv/iso", &["iso"]), "iso1");
        assert_eq!(folder_tag("/", &[]), "share");
    }

    #[test]
    fn virtiofs_gets_shared_memory() {
        let shared = "<memoryBacking><source type='memfd'/><access mode='shared'/></memoryBacking>";
        let bare = "<domain><name>a</name></domain>";
        let added = with_shared_memory(bare).unwrap().unwrap();
        assert!(added.contains(shared), "{added}");
        assert_eq!(with_shared_memory(&added).unwrap(), None);
        let hugepages =
            "<domain><memoryBacking><hugepages/><access mode='private'/></memoryBacking></domain>";
        let changed = with_shared_memory(hugepages).unwrap().unwrap();
        assert_eq!(
            changed,
            "<domain><memoryBacking><hugepages/><source type='memfd'/><access mode='shared'/></memoryBacking></domain>"
        );
        let empty = with_shared_memory("<domain><memoryBacking/></domain>")
            .unwrap()
            .unwrap();
        assert_eq!(empty, format!("<domain>{shared}</domain>"));
    }

    #[test]
    fn devices_libvirt_cannot_attach_go_into_the_definition() {
        let bare = "<domain><devices><rng/></devices></domain>";
        let tpm = with_device(bare, tpm_xml()).unwrap();
        let c = MachineConfig::parse(&tpm).unwrap();
        assert_eq!(c.gadgets[1].gadget, Gadget::Tpm { emulated: true });
        assert_eq!(without_device(&tpm, &c.gadgets[1].xml).unwrap(), bare);
    }

    #[test]
    fn cdrom_xml_keeps_the_drive() {
        let c = MachineConfig::parse(VIRT_MANAGER).unwrap();
        let xml = cdrom_xml(&c.disks[1], Some("/isos/a&b.iso"));
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let target = doc
            .descendants()
            .find(|n| n.has_tag_name("target"))
            .unwrap();
        assert_eq!(target.attribute("dev"), Some("sda"));
        let source = doc
            .descendants()
            .find(|n| n.has_tag_name("source"))
            .unwrap();
        assert_eq!(source.attribute("file"), Some("/isos/a&b.iso"));
        assert!(!cdrom_xml(&c.disks[1], None).contains("source"));
    }

    #[test]
    fn boot_order_moves_onto_the_devices() {
        let c = MachineConfig::parse(VIRT_MANAGER).unwrap();
        assert_eq!(c.boot, [BootDevice::Disk(0)]);
        let order = [BootDevice::Disk(1), BootDevice::Nic(0), BootDevice::Disk(0)];
        let xml = set_boot_order(VIRT_MANAGER, &order).unwrap();
        assert!(!xml.contains("<boot dev="), "{xml}");
        assert_eq!(MachineConfig::parse(&xml).unwrap().boot, order);
        let again = set_boot_order(&xml, &[BootDevice::Nic(0)]).unwrap();
        assert_eq!(again.matches("<boot order=").count(), 1, "{again}");
        assert_eq!(
            MachineConfig::parse(&again).unwrap().boot,
            [BootDevice::Nic(0)]
        );
        let empty = "<domain><devices><interface type='user'/></devices></domain>";
        let xml = set_boot_order(empty, &[BootDevice::Nic(0)]).unwrap();
        assert!(
            xml.contains("<interface type='user'><boot order='1'/></interface>"),
            "{xml}"
        );
    }

    #[test]
    fn a_clone_gets_its_own_identity_and_disks() {
        let with_nvram = VIRT_MANAGER.replace(
            "<boot dev='hd'/>",
            "<nvram template='/usr/share/edk2/ovmf/OVMF_VARS.fd'>/var/lib/libvirt/qemu/nvram/fedora41_VARS.fd</nvram>",
        );
        let c = MachineConfig::parse(&with_nvram).unwrap();
        let disks = [(
            c.disks[0].xml.clone(),
            Some("/var/lib/libvirt/images/copy.qcow2".to_owned()),
        )];
        let xml = clone_xml(&with_nvram, "copy", &disks).unwrap();
        assert!(xml.contains("<name>copy</name>"), "{xml}");
        assert!(!xml.contains("<uuid>"), "{xml}");
        assert!(!xml.contains("52:54:00:12:34:56"), "{xml}");
        assert!(!xml.contains("fedora41_VARS"), "{xml}");
        assert!(
            xml.contains("<nvram template='/usr/share/edk2/ovmf/OVMF_VARS.fd'/>"),
            "{xml}"
        );
        let copy = MachineConfig::parse(&xml).unwrap();
        assert_eq!(copy.disk_files(), ["/var/lib/libvirt/images/copy.qcow2"]);
        assert_eq!(copy.disks[1], c.disks[1]);

        let without = clone_xml(VIRT_MANAGER, "copy", &[(c.disks[0].xml.clone(), None)]).unwrap();
        assert_eq!(MachineConfig::parse(&without).unwrap().disks.len(), 1);
    }

    #[test]
    fn processors_change_with_what_their_cpu_has() {
        let c = MachineConfig::parse(VIRT_MANAGER).unwrap();
        assert_eq!(c.cpu.model, CpuModel::Default);
        assert_eq!(c.cpu.topology, Topology::Sockets);
        assert_eq!(c.cpu.pins, Some(vec![]));
        let cpu = Cpu {
            count: 6,
            model: CpuModel::HostPassthrough,
            topology: Topology::Threads,
            pins: Some(vec![2, 3, 4, 5, 6, 7, 8]),
        };
        let xml = set_cpu(VIRT_MANAGER, &cpu).unwrap();
        let back = MachineConfig::parse(&xml).unwrap();
        assert_eq!(back.vcpus, 6);
        assert_eq!(
            back.cpu,
            Cpu {
                pins: Some(vec![2, 3, 4, 5, 6, 7]),
                ..cpu.clone()
            }
        );
        assert!(
            xml.contains("<topology sockets='1' cores='3' threads='2'/>"),
            "{xml}"
        );

        // An odd count cannot have two threads to a core; features and NUMA cells stay.
        let numa = xml.replace(
            "</cpu>",
            "<feature policy='require' name='topoext'/><numa><cell id='0' cpus='0-4' memory='4' unit='GiB'/></numa></cpu>",
        );
        let odd = set_cpu(
            &numa,
            &Cpu {
                count: 5,
                model: CpuModel::Named("EPYC".into()),
                pins: Some(vec![]),
                ..cpu.clone()
            },
        )
        .unwrap();
        let back = MachineConfig::parse(&odd).unwrap();
        assert_eq!(back.cpu.model, CpuModel::Named("EPYC".into()));
        assert_eq!(back.cpu.topology, Topology::Cores);
        assert_eq!(back.cpu.pins, Some(vec![]));
        assert!(!odd.contains("cputune"), "{odd}");
        assert!(odd.contains("<numa><cell id='0'"), "{odd}");
        assert!(odd.contains("name='topoext'"), "{odd}");

        let plain = set_cpu(
            &xml,
            &Cpu {
                model: CpuModel::Default,
                topology: Topology::Sockets,
                pins: Some(vec![]),
                ..cpu.clone()
            },
        )
        .unwrap();
        assert!(!plain.contains("<cpu"), "{plain}");

        // Pins to more than one host processor each are not this app's to touch.
        let wide = xml
            .replace("<cputune>", "<cputune><emulatorpin cpuset='0-1'/>")
            .replace("cpuset='2'", "cpuset='2-3'");
        let c = MachineConfig::parse(&wide).unwrap();
        assert_eq!(c.cpu.pins, None);
        let kept = set_cpu(
            &wide,
            &Cpu {
                count: 8,
                ..c.cpu.clone()
            },
        )
        .unwrap();
        assert!(
            kept.contains("cpuset='2-3'") && kept.contains("emulatorpin"),
            "{kept}"
        );
        let unpinned = set_cpu(
            &wide,
            &Cpu {
                pins: Some(vec![]),
                ..c.cpu
            },
        )
        .unwrap();
        assert!(
            !unpinned.contains("vcpupin") && unpinned.contains("emulatorpin"),
            "{unpinned}"
        );
    }

    #[test]
    fn cpu_lists_read_and_write() {
        assert_eq!(parse_cpu_list(" 2-5, 8"), Some(vec![2, 3, 4, 5, 8]));
        assert_eq!(parse_cpu_list(""), Some(vec![]));
        assert_eq!(parse_cpu_list("5-2"), None);
        assert_eq!(parse_cpu_list("two"), None);
        assert_eq!(parse_cpu_list("0-4294967295"), None);
        assert_eq!(parse_cpu_list("8192"), None);
        assert_eq!(parse_cpu_list("8191").map(|c| c.len()), Some(1));
        assert_eq!(parse_cpu_list("0-8191,0-8191"), None);
        assert_eq!(cpu_list(&[2, 3, 4, 5, 8, 9, 11]), "2-5,8,9,11");
        assert_eq!(cpu_list(&[]), "");
    }

    #[test]
    fn hugepages_come_and_go() {
        let on = set_hugepages(VIRT_MANAGER, true).unwrap();
        assert!(MachineConfig::parse(&on).unwrap().hugepages);
        assert!(!set_hugepages(&on, false).unwrap().contains("memoryBacking"));
        let shared = with_shared_memory(VIRT_MANAGER).unwrap().unwrap();
        let both = set_hugepages(&shared, true).unwrap();
        assert!(
            both.contains("<memoryBacking><hugepages/><source type='memfd'/>"),
            "{both}"
        );
        let back = set_hugepages(&both, false).unwrap();
        assert_eq!(back, shared);
    }

    #[test]
    fn firmware_is_left_for_libvirt_to_pick() {
        let defined = VIRT_MANAGER.replace(
            "<boot dev='hd'/>",
            "<firmware><feature enabled='yes' name='secure-boot'/></firmware>\
             <loader readonly='yes' secure='yes' type='pflash'>/usr/share/OVMF_CODE.secboot.fd</loader>\
             <nvram>/var/lib/libvirt/qemu/nvram/f_VARS.fd</nvram><boot dev='hd'/>",
        );
        let c = MachineConfig::parse(&defined).unwrap();
        assert_eq!(c.firmware, Firmware::UefiSecureBoot);
        let uefi = set_firmware(&defined, Firmware::Uefi).unwrap();
        assert!(
            !uefi.contains("<loader") && !uefi.contains("<nvram"),
            "{uefi}"
        );
        assert_eq!(
            MachineConfig::parse(&uefi).unwrap().firmware,
            Firmware::Uefi
        );
        let bios = set_firmware(&uefi, Firmware::Bios).unwrap();
        assert!(
            bios.contains("<os>") && !bios.contains("<firmware>"),
            "{bios}"
        );
        assert_eq!(
            MachineConfig::parse(&bios).unwrap().firmware,
            Firmware::Bios
        );
        let secure = set_firmware(&bios, Firmware::UefiSecureBoot).unwrap();
        assert_eq!(
            MachineConfig::parse(&secure).unwrap().firmware,
            Firmware::UefiSecureBoot
        );
    }

    #[test]
    fn snapshots_read_back() {
        let taken = snapshot_xml("Before <update>", "a & b");
        let s = Snapshot::parse(&taken).unwrap();
        assert_eq!(s.name, "Before <update>");
        assert_eq!(s.description.as_deref(), Some("a & b"));
        assert!(snapshot_xml("x", "").contains("<name>x</name></domainsnapshot>"));

        let libvirt = format!(
            "<domainsnapshot><name>s1</name><state>running</state>\
             <creationTime>1790231332</creationTime><memory snapshot='internal'/>\
             <disks><disk name='vda' snapshot='internal'/></disks>{VIRT_MANAGER}</domainsnapshot>"
        );
        let s = Snapshot::parse(&libvirt).unwrap();
        assert_eq!(
            (s.name.as_str(), s.created, s.running),
            ("s1", 1790231332, true)
        );
        assert_eq!(s.description, None);
        let disks_only = libvirt.replace("'internal'/><disks>", "'no'/><disks>");
        assert!(!Snapshot::parse(&disks_only).unwrap().running);
        let off = libvirt.replace("running", "shutoff");
        assert!(!Snapshot::parse(&off).unwrap().running);
    }
}
