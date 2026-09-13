//! Devices given to a machine, or taken from it, after it was made.

use gettextrs::gettext;
use virt::storage_pool::StoragePool;
use virt::storage_vol::StorageVol;
use virt::sys;

use super::{Hypervisor, Result, image_format, message};
use crate::domain_xml::{self, DiskDevice, MachineConfig};
use crate::host_xml::{self, HostDevice};

/// Whether a change reached the running machine as well as its definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Done,
    /// Only the definition changed; the machine has it from its next start.
    AtNextStart,
}

#[derive(Debug, Clone)]
pub enum NewStorage {
    /// A new, empty volume of this many GiB in the pool of this name.
    Volume { pool: String, gib: u64 },
    /// A volume already in a pool, by its path.
    PoolVolume(String),
    /// An image file that is already there.
    Image(String),
    /// A disk of the host, by the path of its device node.
    HostDisk(String),
    /// A CD/DVD drive, empty or with this disc image in it.
    Cdrom(Option<String>),
}

impl Hypervisor {
    /// Add the device `xml` to the definition, and to the running machine if it can take
    /// it there.
    pub fn attach(&self, uuid: &str, xml: &str) -> Result<Change> {
        self.change_device(uuid, xml, true)
    }

    /// Remove the device `xml`, as [`Self::attach`] adds one.
    pub fn detach(&self, uuid: &str, xml: &str) -> Result<Change> {
        self.change_device(uuid, xml, false)
    }

    fn change_device(&self, uuid: &str, xml: &str, attach: bool) -> Result<Change> {
        let dom = self.domain(uuid)?;
        let apply = |flags| {
            if attach {
                dom.attach_device_flags(xml, flags)
            } else {
                dom.detach_device_flags(xml, flags)
            }
        };
        let active = dom.is_active().map_err(message)?;
        let persistent = dom.is_persistent().map_err(message)?;
        let config = sys::VIR_DOMAIN_AFFECT_CONFIG;
        let mut flags = 0;
        if persistent {
            flags |= config;
        }
        if active {
            flags |= sys::VIR_DOMAIN_AFFECT_LIVE;
        }
        // Where the running machine refuses, e.g. a SATA disk, which cannot be hotplugged,
        // the definition alone still takes the change.
        match apply(flags) {
            Ok(_) => Ok(Change::Done),
            Err(_) if active && persistent => {
                apply(config).map(|_| Change::AtNextStart).map_err(message)
            }
            Err(e) => Err(message(e)),
        }
    }

    /// Add a disk or CD/DVD drive, on the bus the machine's others of its kind are on.
    pub fn add_storage(&self, uuid: &str, storage: &NewStorage) -> Result<Change> {
        let dom = self.domain(uuid)?;
        let xml = dom
            .get_xml_desc(sys::VIR_DOMAIN_XML_INACTIVE)
            .map_err(message)?;
        let config = MachineConfig::parse(&xml)?;
        let bus_of = |device| {
            config
                .disks
                .iter()
                .find(|d| d.device == device && !d.bus.is_empty())
                .map(|d| d.bus.clone())
        };
        let taken: Vec<&str> = config.disks.iter().map(|d| d.target.as_str()).collect();
        match storage {
            NewStorage::Volume { pool, gib } => {
                let bus = bus_of(DiskDevice::Disk).unwrap_or_else(|| "virtio".to_owned());
                let pool = StoragePool::lookup_by_name(&self.conn, pool).map_err(message)?;
                if !pool.is_active().map_err(message)? {
                    pool.create(0).map_err(message)?;
                }
                let name = dom.get_name().map_err(message)?;
                let vol = self.new_disk(&pool, &name, *gib)?;
                let attached = volume_disk_xml(&vol, &domain_xml::next_target(&bus, &taken), &bus)
                    .and_then(|disk| self.attach(uuid, &disk));
                if attached.is_err() {
                    let _ = vol.delete(0);
                }
                attached
            }
            NewStorage::PoolVolume(path) => {
                let bus = bus_of(DiskDevice::Disk).unwrap_or_else(|| "virtio".to_owned());
                let vol = StorageVol::lookup_by_path(&self.conn, path).map_err(message)?;
                let disk = volume_disk_xml(&vol, &domain_xml::next_target(&bus, &taken), &bus)?;
                self.attach(uuid, &disk)
            }
            NewStorage::Image(path) => {
                let bus = bus_of(DiskDevice::Disk).unwrap_or_else(|| "virtio".to_owned());
                let format = image_format(std::path::Path::new(path));
                let disk = domain_xml::disk_xml(
                    DiskDevice::Disk,
                    Some(path),
                    &format,
                    &domain_xml::next_target(&bus, &taken),
                    &bus,
                );
                self.attach(uuid, &disk)
            }
            NewStorage::HostDisk(dev) => {
                let bus = bus_of(DiskDevice::Disk).unwrap_or_else(|| "virtio".to_owned());
                let target = domain_xml::next_target(&bus, &taken);
                self.attach(uuid, &domain_xml::block_disk_xml(dev, &target, &bus))
            }
            NewStorage::Cdrom(iso) => {
                let bus = bus_of(DiskDevice::Cdrom).unwrap_or_else(|| {
                    if config.machine.contains("q35") {
                        "sata".to_owned()
                    } else if config.machine == "pc" || config.machine.contains("i440fx") {
                        "ide".to_owned()
                    } else {
                        "scsi".to_owned()
                    }
                });
                let drive = domain_xml::disk_xml(
                    DiskDevice::Cdrom,
                    iso.as_deref(),
                    "raw",
                    &domain_xml::next_target(&bus, &taken),
                    &bus,
                );
                self.attach(uuid, &drive)
            }
        }
    }

    /// The host's USB and PCI devices, USB first.
    pub fn host_devices(&self) -> Result<Vec<HostDevice>> {
        let devices = self.node_devices(
            sys::VIR_CONNECT_LIST_NODE_DEVICES_CAP_USB_DEV
                | sys::VIR_CONNECT_LIST_NODE_DEVICES_CAP_PCI_DEV,
        )?;
        let mut found: Vec<HostDevice> = devices
            .iter()
            .filter_map(|d| d.get_xml_desc(0).ok())
            .filter_map(|xml| HostDevice::parse(&xml))
            .collect();
        found.sort_by_key(|d| {
            (
                matches!(d.id, crate::host_xml::HostDeviceId::Pci(_)),
                d.id.to_string(),
            )
        });
        Ok(found)
    }
}

/// A `<disk>` on the volume `vol`: an image file in its format, or a block device such as a
/// logical volume or an iSCSI LUN.
fn volume_disk_xml(vol: &StorageVol, target: &str, bus: &str) -> Result<String> {
    let path = vol.get_path().map_err(message)?;
    match vol.get_info().map_err(message)?.kind {
        sys::VIR_STORAGE_VOL_FILE => {
            let format = vol
                .get_xml_desc(0)
                .ok()
                .and_then(|xml| host_xml::volume_format(&xml))
                .unwrap_or_else(|| image_format(std::path::Path::new(&path)));
            Ok(domain_xml::disk_xml(
                DiskDevice::Disk,
                Some(&path),
                &format,
                target,
                bus,
            ))
        }
        sys::VIR_STORAGE_VOL_BLOCK => Ok(domain_xml::block_disk_xml(&path, target, bus)),
        _ => Err(gettext("{path} is neither a file nor a block device").replace("{path}", &path)),
    }
}
