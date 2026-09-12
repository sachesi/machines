//! Domain XML: what the details page reads from it, the XML of a new machine, and the
//! edits libvirt has no call for.

use std::fmt::Write;

use crate::host_xml::HostDeviceId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firmware {
    Bios,
    Uefi,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineConfig {
    pub virt_type: String,
    pub arch: String,
    pub machine: String,
    pub firmware: Firmware,
    pub memory_mib: u64,
    pub vcpus: u32,
    /// The libosinfo id virt-manager and virt-install record, e.g.
    /// `http://fedoraproject.org/fedora/41`.
    pub os_id: Option<String>,
    pub disks: Vec<Disk>,
    pub nics: Vec<Nic>,
    pub host_devices: Vec<HostDev>,
    /// The `type` of each graphics device, in order: `vnc`, `spice`, `dbus`…
    pub graphics: Vec<String>,
    pub video: Option<String>,
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
        let firmware =
            if os.and_then(|os| os.attribute("firmware")) == Some("efi") || loader_is_pflash {
                Firmware::Uefi
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
        let mut graphics = Vec::new();
        let mut video = None;
        for dev in devices {
            let xml = xml[dev.range()].to_owned();
            let sub = |name: &str| dev.children().find(|n| n.has_tag_name(name));
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
                "interface" => nics.push(Nic {
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
                }),
                "hostdev" if dev.attribute("mode") == Some("subsystem") => {
                    if let Some(id) = HostDeviceId::from_hostdev(dev) {
                        host_devices.push(HostDev { id, xml });
                    }
                }
                "graphics" => {
                    graphics.push(dev.attribute("type").unwrap_or_default().to_owned());
                }
                "video" if video.is_none() => {
                    video = sub("model")
                        .and_then(|m| m.attribute("type"))
                        .map(str::to_owned);
                }
                _ => {}
            }
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
            vcpus,
            os_id,
            disks,
            nics,
            host_devices,
            graphics,
            video,
        })
    }

    /// Image files this machine's disks (not its CD-ROMs) are read from.
    pub fn disk_files(&self) -> Vec<String> {
        self.disks
            .iter()
            .filter(|d| d.device == DiskDevice::Disk && d.kind == "file")
            .filter_map(|d| d.source.clone())
            .collect()
    }
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
    pub uefi: bool,
    pub tpm: bool,
    pub memory_mib: u64,
    pub vcpus: u32,
    /// Path and format (`qcow2`, `raw`) of the system disk.
    pub disk: Option<(String, String)>,
    pub cdrom: Option<String>,
    pub network: NetworkSource,
    /// A model from [`video_model`].
    pub video: String,
}

/// The XML of a machine that installs from `cdrom` onto `disk`, or boots `disk` as it is.
///
/// The display is VNC with no listening socket: the console reaches it through libvirt,
/// and nothing else on the network can.
pub fn new_machine_xml(m: &NewMachine) -> String {
    let windows = m.os == GuestOs::Windows;
    let kvm = m.virt_type == "kvm";
    let mut x = String::new();
    let _ = writeln!(x, "<domain type='{}'>", escape(&m.virt_type));
    let _ = writeln!(x, "  <name>{}</name>", escape(&m.name));
    let _ = writeln!(x, "  <memory unit='MiB'>{}</memory>", m.memory_mib);
    let _ = writeln!(
        x,
        "  <currentMemory unit='MiB'>{}</currentMemory>",
        m.memory_mib
    );
    let _ = writeln!(x, "  <vcpu>{}</vcpu>", m.vcpus);
    x.push_str(if m.uefi {
        "  <os firmware='efi'>\n"
    } else {
        "  <os>\n"
    });
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
    if m.uefi {
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
         <graphics type='vnc'>\n      <listen type='none'/>\n    </graphics>\n",
    );
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
    let models: Vec<String> = roxmltree::Document::parse(caps)
        .ok()
        .and_then(|doc| {
            let video = doc.descendants().find(|n| n.has_tag_name("video"))?;
            let list = video
                .descendants()
                .find(|n| n.has_tag_name("enum") && n.attribute("name") == Some("modelType"))?;
            Some(
                list.children()
                    .filter(|n| n.has_tag_name("value"))
                    .filter_map(|n| n.text().map(str::to_owned))
                    .collect(),
            )
        })
        .unwrap_or_default();
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

/// `xml` with its SPICE display replaced by one the built-in console can show: VNC with no
/// listening socket. What only works with SPICE goes too: its agent channel, USB
/// redirection and smartcard, and its audio backend, which becomes none.
pub fn spice_to_vnc(xml: &str) -> Result<String, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
    let devices = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("devices"))
        .ok_or("the domain has no devices")?;
    let has_vnc = devices
        .children()
        .any(|n| n.has_tag_name("graphics") && n.attribute("type") == Some("vnc"));
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut replaced = has_vnc;
    for dev in devices.children().filter(|n| n.is_element()) {
        let kind = dev.attribute("type");
        let spicevmc = || {
            kind == Some("spicevmc")
                || dev
                    .children()
                    .any(|c| c.is_element() && c.attribute("type") == Some("spicevmc"))
        };
        match dev.tag_name().name() {
            "graphics" if kind == Some("spice") => {
                let replacement = if replaced {
                    String::new()
                } else {
                    replaced = true;
                    "<graphics type='vnc'>\n      <listen type='none'/>\n    </graphics>".to_owned()
                };
                edits.push((dev.range(), replacement));
            }
            "channel" | "redirdev" | "smartcard" if spicevmc() => {
                edits.push((dev.range(), String::new()));
            }
            "audio" if kind == Some("spice") => {
                let id = dev.attribute("id").unwrap_or("1");
                edits.push((
                    dev.range(),
                    format!("<audio id='{}' type='none'/>", escape(id)),
                ));
            }
            _ => {}
        }
    }
    let mut out = xml.to_owned();
    // Back to front, so the earlier ranges still point at the same text.
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    for (range, replacement) in edits {
        out.replace_range(range, &replacement);
    }
    Ok(out)
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
    fn current_vcpus_win_over_the_maximum() {
        let c = MachineConfig::parse(
            "<domain type='qemu'><vcpu current='2'>8</vcpu><memory unit='GiB'>2</memory></domain>",
        )
        .unwrap();
        assert_eq!(c.vcpus, 2);
        assert_eq!(c.memory_mib, 2048);
    }

    #[test]
    fn spice_becomes_vnc_and_its_devices_go() {
        let xml = spice_to_vnc(VIRT_MANAGER).unwrap();
        let c = MachineConfig::parse(&xml).unwrap();
        assert_eq!(c.graphics, ["vnc"]);
        assert!(!xml.contains("spicevmc"), "{xml}");
        assert!(xml.contains("<audio id='1' type='none'/>"), "{xml}");
        // The sound card still points at an audio backend that exists.
        assert!(xml.contains("<audio id='1'/>"), "{xml}");
    }

    #[test]
    fn a_machine_with_vnc_already_only_loses_spice() {
        let xml = VIRT_MANAGER.replace(
            "<video>",
            "<graphics type='vnc'><listen type='none'/></graphics>\n    <video>",
        );
        let c = MachineConfig::parse(&spice_to_vnc(&xml).unwrap()).unwrap();
        assert_eq!(c.graphics, ["vnc"]);
    }

    fn new_machine(os: GuestOs) -> NewMachine {
        NewMachine {
            name: "Tom & Jerry's <box>".into(),
            virt_type: "kvm".into(),
            os,
            uefi: true,
            tpm: false,
            memory_mib: 4096,
            vcpus: 2,
            disk: Some(("/images/a'b.qcow2".into(), "qcow2".into())),
            cdrom: Some("/isos/install.iso".into()),
            network: NetworkSource::Network("default".into()),
            video: video_model("", os),
        }
    }

    #[test]
    fn new_machine_reads_back() {
        let xml = new_machine_xml(&new_machine(GuestOs::Linux));
        let doc = roxmltree::Document::parse(&xml).unwrap();
        let name = doc.descendants().find(|n| n.has_tag_name("name")).unwrap();
        assert_eq!(name.text(), Some("Tom & Jerry's <box>"));
        let c = MachineConfig::parse(&xml).unwrap();
        assert_eq!(c.firmware, Firmware::Uefi);
        assert_eq!(c.memory_mib, 4096);
        assert_eq!(c.vcpus, 2);
        assert_eq!(c.disk_files(), ["/images/a'b.qcow2"]);
        assert_eq!(c.disks[0].bus, "virtio");
        assert_eq!(c.disks[1].source.as_deref(), Some("/isos/install.iso"));
        assert_eq!(c.graphics, ["vnc"]);
        assert_eq!(c.nics[0].model.as_deref(), Some("virtio"));
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
}
