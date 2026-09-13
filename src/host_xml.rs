//! The XML of what the host lends its machines: its USB and PCI devices, storage pools and
//! volumes, and virtual networks.

use std::fmt;
use std::net::Ipv4Addr;

use crate::domain_xml::escape;

/// A number as libvirt writes it, decimal or `0x` hexadecimal.
fn number(text: &str) -> Option<u32> {
    let text = text.trim();
    match text.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciAddress {
    pub domain: u32,
    pub bus: u32,
    pub slot: u32,
    pub function: u32,
}

impl fmt::Display for PciAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{:x}",
            self.domain, self.bus, self.slot, self.function
        )
    }
}

/// A host device as a `<hostdev>` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostDeviceId {
    Usb {
        vendor: u32,
        product: u32,
        /// Bus and device number, for telling identical devices apart.
        address: Option<(u32, u32)>,
    },
    Pci(PciAddress),
}

impl HostDeviceId {
    /// Whether both name the same device, where one leaves out the USB address.
    pub fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Usb {
                    vendor,
                    product,
                    address,
                },
                Self::Usb {
                    vendor: v,
                    product: p,
                    address: a,
                },
            ) => vendor == v && product == p && (address.is_none() || a.is_none() || address == a),
            (Self::Pci(a), Self::Pci(b)) => a == b,
            _ => false,
        }
    }

    /// The `<hostdev>` that passes this device through. libvirt takes a PCI device from its
    /// host driver while the machine runs, and gives it back after.
    pub fn hostdev_xml(&self) -> String {
        match self {
            Self::Usb {
                vendor,
                product,
                address,
            } => {
                let address = address
                    .map(|(bus, device)| format!("<address bus='{bus}' device='{device}'/>"))
                    .unwrap_or_default();
                format!(
                    "<hostdev mode='subsystem' type='usb' managed='yes'><source>\
                     <vendor id='{vendor:#06x}'/><product id='{product:#06x}'/>{address}\
                     </source></hostdev>"
                )
            }
            Self::Pci(a) => format!(
                "<hostdev mode='subsystem' type='pci' managed='yes'><source>\
                 <address domain='{:#06x}' bus='{:#04x}' slot='{:#04x}' function='{:#x}'/>\
                 </source></hostdev>",
                a.domain, a.bus, a.slot, a.function
            ),
        }
    }

    /// Read the `<source>` of a `<hostdev type='usb|pci'>`.
    pub fn from_hostdev(dev: roxmltree::Node) -> Option<Self> {
        let source = dev.children().find(|n| n.has_tag_name("source"))?;
        let sub = |name: &str| source.children().find(|n| n.has_tag_name(name));
        let address = sub("address");
        let attr = |name: &str| address.and_then(|a| a.attribute(name)).and_then(number);
        match dev.attribute("type")? {
            "usb" => Some(Self::Usb {
                vendor: sub("vendor")?.attribute("id").and_then(number)?,
                product: sub("product")?.attribute("id").and_then(number)?,
                address: attr("bus").zip(attr("device")),
            }),
            "pci" => Some(Self::Pci(PciAddress {
                domain: attr("domain").unwrap_or(0),
                bus: attr("bus")?,
                slot: attr("slot")?,
                function: attr("function").unwrap_or(0),
            })),
            _ => None,
        }
    }
}

impl fmt::Display for HostDeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usb {
                vendor, product, ..
            } => write!(f, "{vendor:04x}:{product:04x}"),
            Self::Pci(address) => address.fmt(f),
        }
    }
}

/// A USB or PCI device of the host, from its node device XML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDevice {
    pub id: HostDeviceId,
    pub product: Option<String>,
    pub vendor: Option<String>,
    /// The PCI class code, e.g. `0x030000` for a display controller.
    pub class: Option<u32>,
}

impl HostDevice {
    pub fn parse(xml: &str) -> Option<Self> {
        let doc = roxmltree::Document::parse(xml).ok()?;
        let cap = doc.descendants().find(|n| {
            n.has_tag_name("capability")
                && matches!(n.attribute("type"), Some("usb_device" | "pci"))
        })?;
        let sub = |name: &str| cap.children().find(|n| n.has_tag_name(name));
        let text = |name: &str| sub(name).and_then(|n| n.text()).and_then(number);
        let label = |name: &str| {
            sub(name)
                .and_then(|n| n.text())
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
        };
        let id_of = |name: &str| sub(name).and_then(|n| n.attribute("id")).and_then(number);
        let id = if cap.attribute("type") == Some("usb_device") {
            HostDeviceId::Usb {
                vendor: id_of("vendor")?,
                product: id_of("product")?,
                address: text("bus").zip(text("device")),
            }
        } else {
            HostDeviceId::Pci(PciAddress {
                domain: text("domain").unwrap_or(0),
                bus: text("bus")?,
                slot: text("slot")?,
                function: text("function").unwrap_or(0),
            })
        };
        Some(Self {
            id,
            product: label("product"),
            vendor: label("vendor"),
            class: text("class"),
        })
    }

    /// The id to pass this device through by. A USB device goes by its vendor and product
    /// alone, so that it is found again after it moves to another port, unless `all` has
    /// another one like it.
    pub fn passthrough_id(&self, all: &[HostDevice]) -> HostDeviceId {
        match &self.id {
            HostDeviceId::Usb {
                vendor, product, ..
            } => {
                let alike = all
                    .iter()
                    .filter(|d| {
                        matches!(&d.id, HostDeviceId::Usb { vendor: v, product: p, .. }
                            if v == vendor && p == product)
                    })
                    .count();
                if alike > 1 {
                    self.id.clone()
                } else {
                    HostDeviceId::Usb {
                        vendor: *vendor,
                        product: *product,
                        address: None,
                    }
                }
            }
            HostDeviceId::Pci(_) => self.id.clone(),
        }
    }

    /// Whether a machine could sensibly be given this device: not a USB root hub, nor a
    /// PCI bridge the host's other devices hang off, nor one of the chipset's own
    /// peripherals such as its IOMMU.
    pub fn can_pass_through(&self) -> bool {
        const LINUX_FOUNDATION: u32 = 0x1d6b;
        const BRIDGE_CLASS: u32 = 0x06;
        const SYSTEM_PERIPHERAL_CLASS: u32 = 0x08;
        match &self.id {
            HostDeviceId::Usb { vendor, .. } => *vendor != LINUX_FOUNDATION,
            HostDeviceId::Pci(_) => self
                .class
                .is_none_or(|c| !matches!(c >> 16, BRIDGE_CLASS | SYSTEM_PERIPHERAL_CLASS)),
        }
    }
}

/// A whole disk of the host, from its node device XML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostDisk {
    /// The device node, e.g. `/dev/sda`.
    pub block: String,
    /// Where to reach it from a machine: its `/dev/disk/by-id` link where udev made one,
    /// which stays the same when the kernel names the disks in another order.
    pub path: String,
    pub model: Option<String>,
    pub vendor: Option<String>,
    /// Bytes.
    pub size: u64,
}

impl HostDisk {
    /// A disk of the host; CD drives, card readers and other removable media are not.
    pub fn parse(xml: &str) -> Option<Self> {
        let doc = roxmltree::Document::parse(xml).ok()?;
        let cap = doc
            .descendants()
            .find(|n| n.has_tag_name("capability") && n.attribute("type") == Some("storage"))?;
        let text = |name: &str| {
            cap.children()
                .find(|n| n.has_tag_name(name))
                .and_then(|n| n.text())
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
        };
        if text("drive_type").is_some_and(|t| t != "disk")
            || cap
                .children()
                .any(|n| n.has_tag_name("capability") && n.attribute("type") == Some("removable"))
        {
            return None;
        }
        let block = text("block")?;
        let path = doc
            .descendants()
            .filter(|n| n.has_tag_name("devnode") && n.attribute("type") == Some("link"))
            .filter_map(|n| n.text())
            .filter(|l| l.starts_with("/dev/disk/by-id/"))
            // A wwn- link is a bare number; the model-and-serial one says what the disk is.
            .min_by_key(|l| (l.starts_with("/dev/disk/by-id/wwn-"), l.len()))
            .map_or_else(|| block.clone(), str::to_owned);
        Some(Self {
            block,
            path,
            model: text("model"),
            vendor: text("vendor"),
            size: text("size").and_then(|s| s.parse().ok()).unwrap_or(0),
        })
    }

    pub fn name(&self) -> String {
        match (&self.vendor, &self.model) {
            (Some(vendor), Some(model)) if !model.starts_with(vendor.as_str()) => {
                format!("{vendor} {model}")
            }
            (_, Some(model)) => model.clone(),
            (Some(vendor), None) => vendor.clone(),
            (None, None) => self.block.clone(),
        }
    }
}

fn child<'a, 'i>(parent: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    parent.children().find(|n| n.has_tag_name(name))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolConfig {
    /// `dir`, `fs`, `netfs`, `logical`, `disk`, `iscsi`…
    pub kind: String,
    pub path: Option<String>,
    /// Where the pool's storage comes from, as people write it: `host:/export` for NFS,
    /// the volume group for LVM, the target for iSCSI.
    pub source: Option<String>,
}

impl PoolConfig {
    pub fn parse(xml: &str) -> Result<Self, String> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
        let root = doc.root_element();
        let path = child(root, "target")
            .and_then(|t| child(t, "path"))
            .and_then(|p| p.text())
            .map(|p| p.trim().to_owned());
        let source = child(root, "source").and_then(|source| {
            let host = child(source, "host").and_then(|h| h.attribute("name"));
            let dir = child(source, "dir").and_then(|d| d.attribute("path"));
            let device = child(source, "device").and_then(|d| d.attribute("path"));
            let name = child(source, "name").and_then(|n| n.text());
            match (host, dir.or(device), name) {
                (Some(host), Some(path), _) => Some(format!("{host}:{path}")),
                (None, Some(path), _) => Some(path.to_owned()),
                (_, None, Some(name)) => Some(name.to_owned()),
                (Some(host), None, None) => Some(host.to_owned()),
                (None, None, None) => None,
            }
        });
        Ok(Self {
            kind: root.attribute("type").unwrap_or_default().to_owned(),
            path,
            source,
        })
    }
}

/// What a new pool is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolSource {
    /// The image files in a directory of the host.
    Dir(String),
    /// The image files in an NFS export, mounted on `mount`.
    Nfs {
        host: String,
        export: String,
        mount: String,
    },
    /// The logical volumes of an LVM volume group the host already has.
    Lvm(String),
    /// The LUNs of an iSCSI target.
    Iscsi { host: String, target: String },
}

impl PoolSource {
    /// Whether libvirt makes the pool's directory before it starts it; for the others,
    /// building would format disks.
    pub fn needs_build(&self) -> bool {
        matches!(self, Self::Dir(_) | Self::Nfs { .. })
    }
}

pub fn pool_xml(name: &str, source: &PoolSource) -> String {
    let (kind, source, target) = match source {
        PoolSource::Dir(path) => ("dir", String::new(), path.clone()),
        PoolSource::Nfs {
            host,
            export,
            mount,
        } => (
            "netfs",
            format!(
                "<host name='{}'/><dir path='{}'/><format type='auto'/>",
                escape(host),
                escape(export)
            ),
            mount.clone(),
        ),
        PoolSource::Lvm(group) => (
            "logical",
            format!("<name>{}</name><format type='lvm2'/>", escape(group)),
            format!("/dev/{group}"),
        ),
        PoolSource::Iscsi { host, target } => (
            "iscsi",
            format!(
                "<host name='{}'/><device path='{}'/>",
                escape(host),
                escape(target)
            ),
            "/dev/disk/by-path".to_owned(),
        ),
    };
    format!(
        "<pool type='{kind}'><name>{}</name><source>{source}</source>\
         <target><path>{}</path></target></pool>",
        escape(name),
        escape(&target)
    )
}

/// A volume of `gib` GiB; qcow2 ones only take up what is written to them. Logical
/// volumes have no format.
pub fn volume_xml(name: &str, gib: u64, format: Option<&str>) -> String {
    let target = format
        .map(|f| format!("<target><format type='{}'/></target>", escape(f)))
        .unwrap_or_default();
    format!(
        "<volume><name>{}</name><capacity unit='GiB'>{gib}</capacity>{target}</volume>",
        escape(name)
    )
}

/// A raw volume of exactly `bytes`, for a file to be uploaded into, which fills it with
/// whatever format it is in.
pub fn upload_volume_xml(name: &str, bytes: u64) -> String {
    format!(
        "<volume><name>{}</name><capacity unit='bytes'>{bytes}</capacity>\
         <allocation>0</allocation><target><format type='raw'/></target></volume>",
        escape(name)
    )
}

/// The image format a volume's XML gives, e.g. `qcow2`.
pub fn volume_format(xml: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let target = doc
        .root_element()
        .children()
        .find(|n| n.has_tag_name("target"))?;
    target
        .children()
        .find(|n| n.has_tag_name("format"))?
        .attribute("type")
        .map(str::to_owned)
}

/// An IPv4 address with the length of its network prefix, as in `192.168.122.1/24`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ipv4Subnet {
    pub address: Ipv4Addr,
    pub prefix: u8,
}

impl Ipv4Subnet {
    /// `192.168.100.0/24`, for a network of its own: from /8 to /30, so that there are
    /// addresses for the host and its guests. Host bits set in the address are dropped.
    pub fn parse_network(text: &str) -> Option<Self> {
        let (address, prefix) = text.trim().split_once('/')?;
        let address: Ipv4Addr = address.trim().parse().ok()?;
        let prefix: u8 = prefix.trim().parse().ok()?;
        if !(8..=30).contains(&prefix) {
            return None;
        }
        Some(Self { address, prefix }.network())
    }

    fn mask(self) -> u32 {
        u32::MAX << (32 - u32::from(self.prefix))
    }

    pub fn network(self) -> Self {
        Self {
            address: Ipv4Addr::from(u32::from(self.address) & self.mask()),
            prefix: self.prefix,
        }
    }

    fn netmask(self) -> Ipv4Addr {
        Ipv4Addr::from(self.mask())
    }

    fn nth(self, n: u32) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.network().address) + n)
    }

    fn broadcast(self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.address) | !self.mask())
    }

    pub fn overlaps(self, other: Self) -> bool {
        let shorter = self.prefix.min(other.prefix);
        let mask = u32::MAX << (32 - u32::from(shorter));
        u32::from(self.address) & mask == u32::from(other.address) & mask
    }

    /// The first 192.168.x.0/24 from 192.168.100.0 up that overlaps none of `taken`.
    pub fn unused(taken: &[Self]) -> Self {
        (100..=254)
            .map(|x| Self {
                address: Ipv4Addr::new(192, 168, x, 0),
                prefix: 24,
            })
            .find(|s| !taken.iter().any(|t| t.overlaps(*s)))
            .unwrap_or(Self {
                address: Ipv4Addr::new(10, 100, 0, 0),
                prefix: 24,
            })
    }
}

impl fmt::Display for Ipv4Subnet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkConfig {
    /// `nat`, `route`, `open`, `bridge`…; None for an isolated network.
    pub forward: Option<String>,
    /// For a `bridge` network, the host's bridge; otherwise the one libvirt makes.
    pub bridge: Option<String>,
    /// The host's address on the network.
    pub ipv4: Option<Ipv4Subnet>,
    pub dhcp: Option<(String, String)>,
}

impl NetworkConfig {
    pub fn parse(xml: &str) -> Result<Self, String> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
        let root = doc.root_element();
        let child = |name: &str| root.children().find(|n| n.has_tag_name(name));
        let forward = child("forward").map(|f| f.attribute("mode").unwrap_or("nat").to_owned());
        let ip = root
            .children()
            .find(|n| n.has_tag_name("ip") && n.attribute("family").is_none_or(|f| f == "ipv4"));
        let ipv4 = ip.and_then(|ip| {
            let address: Ipv4Addr = ip.attribute("address")?.parse().ok()?;
            let prefix = match ip.attribute("prefix") {
                Some(p) => p.parse().ok()?,
                None => {
                    let mask: Ipv4Addr = ip.attribute("netmask")?.parse().ok()?;
                    u8::try_from(u32::from(mask).leading_ones()).ok()?
                }
            };
            Some(Ipv4Subnet { address, prefix })
        });
        let dhcp = ip
            .and_then(|ip| ip.children().find(|n| n.has_tag_name("dhcp")))
            .and_then(|d| d.children().find(|n| n.has_tag_name("range")))
            .and_then(|r| {
                Some((
                    r.attribute("start")?.to_owned(),
                    r.attribute("end")?.to_owned(),
                ))
            });
        Ok(Self {
            forward,
            bridge: child("bridge")
                .and_then(|b| b.attribute("name"))
                .map(str::to_owned),
            ipv4,
            dhcp,
        })
    }
}

#[derive(Debug, Clone)]
pub struct NewNetwork {
    pub name: String,
    /// Whether guests reach outside through the host's NAT, or only each other and the host.
    pub nat: bool,
    pub subnet: Ipv4Subnet,
    pub dhcp: bool,
}

/// A network on a bridge of libvirt's own, with the host at the subnet's first address.
pub fn new_network_xml(n: &NewNetwork) -> String {
    let forward = if n.nat { "<forward mode='nat'/>" } else { "" };
    let dhcp = if n.dhcp {
        format!(
            "<dhcp><range start='{}' end='{}'/></dhcp>",
            n.subnet.nth(2),
            Ipv4Addr::from(u32::from(n.subnet.broadcast()) - 1)
        )
    } else {
        String::new()
    };
    format!(
        "<network><name>{}</name>{forward}<bridge stp='on' delay='0'/>\
         <ip address='{}' netmask='{}'>{dhcp}</ip></network>",
        escape(&n.name),
        n.subnet.nth(1),
        n.subnet.netmask()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_devices_read_back() {
        let dev = HostDevice::parse(
            "<device><name>usb_1_5</name><capability type='usb_device'>\
             <bus>1</bus><device>5</device>\
             <product id='0xc52b'>Unifying Receiver</product>\
             <vendor id='0x046d'>Logitech, Inc.</vendor></capability></device>",
        )
        .unwrap();
        let id = HostDeviceId::Usb {
            vendor: 0x046d,
            product: 0xc52b,
            address: Some((1, 5)),
        };
        assert_eq!(dev.id, id);
        assert_eq!(dev.product.as_deref(), Some("Unifying Receiver"));
        assert!(dev.can_pass_through());
        assert_eq!(id.to_string(), "046d:c52b");

        let xml = id.hostdev_xml();
        let doc = roxmltree::Document::parse(&xml).unwrap();
        assert_eq!(
            HostDeviceId::from_hostdev(doc.root_element()),
            Some(id.clone())
        );
        let unaddressed = HostDeviceId::Usb {
            vendor: 0x046d,
            product: 0xc52b,
            address: None,
        };
        assert!(unaddressed.matches(&id));
        let other_port = HostDeviceId::Usb {
            vendor: 0x046d,
            product: 0xc52b,
            address: Some((1, 6)),
        };
        assert!(!other_port.matches(&id));

        assert_eq!(dev.passthrough_id(std::slice::from_ref(&dev)), unaddressed);
        let twin = HostDevice {
            id: other_port,
            ..dev.clone()
        };
        assert_eq!(dev.passthrough_id(&[dev.clone(), twin]), id);
    }

    #[test]
    fn root_hubs_and_bridges_stay_with_the_host() {
        let hub = HostDevice::parse(
            "<device><capability type='usb_device'><bus>1</bus><device>1</device>\
             <product id='0x0002'/><vendor id='0x1d6b'>Linux Foundation</vendor>\
             </capability></device>",
        )
        .unwrap();
        assert!(!hub.can_pass_through());
        assert_eq!(hub.product, None);
        let pci = |class: &str| {
            HostDevice::parse(&format!(
                "<device><capability type='pci'><class>{class}</class><domain>0</domain>\
                 <bus>1</bus><slot>0</slot><function>1</function>\
                 <product id='0x10f9'>Audio</product><vendor id='0x10de'>NVIDIA</vendor>\
                 </capability></device>"
            ))
            .unwrap()
        };
        assert!(!pci("0x060400").can_pass_through());
        assert!(!pci("0x080600").can_pass_through());
        let audio = pci("0x040300");
        assert!(audio.can_pass_through());
        assert_eq!(audio.id.to_string(), "0000:01:00.1");
        let xml = audio.id.hostdev_xml();
        let doc = roxmltree::Document::parse(&xml).unwrap();
        assert_eq!(
            HostDeviceId::from_hostdev(doc.root_element()),
            Some(audio.id)
        );
    }

    #[test]
    fn host_disks_go_by_id() {
        let disk = HostDisk::parse(
            "<device><name>block_sda</name>\
             <devnode type='dev'>/dev/sda</devnode>\
             <devnode type='link'>/dev/disk/by-id/wwn-0x5002538e40a1b2c3</devnode>\
             <devnode type='link'>/dev/disk/by-id/ata-Samsung_SSD_860_EVO_S3Z9NB0K</devnode>\
             <devnode type='link'>/dev/disk/by-path/pci-0000:00:17.0-ata-1</devnode>\
             <capability type='storage'><block>/dev/sda</block><bus>ata</bus>\
             <drive_type>disk</drive_type><model>Samsung SSD 860</model><vendor>ATA</vendor>\
             <size>250059350016</size></capability></device>",
        )
        .unwrap();
        assert_eq!(disk.block, "/dev/sda");
        assert_eq!(
            disk.path,
            "/dev/disk/by-id/ata-Samsung_SSD_860_EVO_S3Z9NB0K"
        );
        assert_eq!(disk.size, 250_059_350_016);
        assert_eq!(disk.name(), "ATA Samsung SSD 860");

        let nvme = HostDisk::parse(
            "<device><capability type='storage'><block>/dev/nvme0n1</block>\
             <drive_type>disk</drive_type><model>Micron MTFDKCD256TFK</model>\
             <size>256060514304</size></capability></device>",
        )
        .unwrap();
        assert_eq!(nvme.path, "/dev/nvme0n1");
        assert_eq!(nvme.name(), "Micron MTFDKCD256TFK");

        let card_reader = "<device><capability type='storage'><block>/dev/sdd</block>\
             <drive_type>disk</drive_type><capability type='removable'>\
             <media_available>0</media_available></capability></capability></device>";
        assert_eq!(HostDisk::parse(card_reader), None);
        let dvd = "<device><capability type='storage'><block>/dev/sr0</block>\
             <drive_type>cdrom</drive_type></capability></device>";
        assert_eq!(HostDisk::parse(dvd), None);
    }

    #[test]
    fn pools_and_volumes() {
        let pool =
            PoolConfig::parse(&pool_xml("isos", &PoolSource::Dir("/srv/i&so".into()))).unwrap();
        assert_eq!(pool.kind, "dir");
        assert_eq!(pool.path.as_deref(), Some("/srv/i&so"));
        assert_eq!(pool.source, None);
        let nfs = PoolSource::Nfs {
            host: "nas".into(),
            export: "/vol/vms".into(),
            mount: "/var/lib/libvirt/images/nas".into(),
        };
        let pool = PoolConfig::parse(&pool_xml("nas", &nfs)).unwrap();
        assert_eq!(pool.kind, "netfs");
        assert_eq!(pool.source.as_deref(), Some("nas:/vol/vms"));
        let pool = PoolConfig::parse(&pool_xml("vg", &PoolSource::Lvm("vg0".into()))).unwrap();
        assert_eq!(pool.kind, "logical");
        assert_eq!(pool.path.as_deref(), Some("/dev/vg0"));
        assert_eq!(pool.source.as_deref(), Some("vg0"));
        let iscsi = PoolSource::Iscsi {
            host: "san".into(),
            target: "iqn.2004-04.com.example:disks".into(),
        };
        let pool = PoolConfig::parse(&pool_xml("san", &iscsi)).unwrap();
        assert_eq!(
            pool.source.as_deref(),
            Some("san:iqn.2004-04.com.example:disks")
        );
        assert!(!iscsi.needs_build());

        assert_eq!(
            volume_format(&volume_xml("a.qcow2", 8, Some("qcow2"))).as_deref(),
            Some("qcow2")
        );
        assert_eq!(volume_format(&volume_xml("lv", 8, None)), None);
        assert_eq!(
            volume_format(&upload_volume_xml("a.iso", 1024)).as_deref(),
            Some("raw")
        );
    }

    #[test]
    fn subnets() {
        let s = Ipv4Subnet::parse_network("10.0.5.7/16").unwrap();
        assert_eq!(s.to_string(), "10.0.0.0/16");
        assert_eq!(Ipv4Subnet::parse_network("10.0.0.0/31"), None);
        assert_eq!(Ipv4Subnet::parse_network("10.0.0.0"), None);
        let default = Ipv4Subnet::parse_network("192.168.100.0/24").unwrap();
        let wide = Ipv4Subnet::parse_network("192.168.0.0/16").unwrap();
        assert!(default.overlaps(wide));
        assert_eq!(
            Ipv4Subnet::unused(&[default]).to_string(),
            "192.168.101.0/24"
        );
        assert_eq!(Ipv4Subnet::unused(&[wide]).to_string(), "10.100.0.0/24");
    }

    #[test]
    fn new_networks_read_back() {
        let n = NewNetwork {
            name: "lab".into(),
            nat: true,
            subnet: Ipv4Subnet::parse_network("192.168.100.0/24").unwrap(),
            dhcp: true,
        };
        let c = NetworkConfig::parse(&new_network_xml(&n)).unwrap();
        assert_eq!(c.forward.as_deref(), Some("nat"));
        assert_eq!(c.ipv4.unwrap().to_string(), "192.168.100.1/24");
        assert_eq!(
            c.dhcp,
            Some(("192.168.100.2".into(), "192.168.100.254".into()))
        );
        let isolated = NetworkConfig::parse(&new_network_xml(&NewNetwork {
            nat: false,
            dhcp: false,
            ..n
        }))
        .unwrap();
        assert_eq!(isolated.forward, None);
        assert_eq!(isolated.dhcp, None);
    }

    #[test]
    fn libvirts_default_network() {
        let c = NetworkConfig::parse(
            "<network><name>default</name><forward mode='nat'><nat><port start='1024' end='65535'/></nat></forward>\
             <bridge name='virbr0' stp='on' delay='0'/><ip address='192.168.122.1' netmask='255.255.255.0'>\
             <dhcp><range start='192.168.122.2' end='192.168.122.254'/></dhcp></ip>\
             <ip family='ipv6' address='fd00::1' prefix='64'/></network>",
        )
        .unwrap();
        assert_eq!(c.bridge.as_deref(), Some("virbr0"));
        assert_eq!(c.ipv4.unwrap().to_string(), "192.168.122.1/24");
    }
}
