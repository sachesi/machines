//! Devices given to a machine, or taken from it, after it was made.

use gettextrs::gettext;
use virt::domain::Domain;
use virt::error::ErrorNumber;
use virt::nodedev::NodeDevice;
use virt::storage_pool::StoragePool;
use virt::storage_vol::StorageVol;
use virt::sys;

use super::{Hypervisor, Result, image_format, message};
use crate::domain_xml::{self, DiskDevice, Gadget, HostDev, MachineConfig};
use crate::host_xml::{self, HostDevice, HostDeviceId};

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

#[derive(Debug, Clone)]
pub enum NewGadget {
    Tpm,
    Rng,
    Sound,
    /// This directory of the host.
    SharedFolder(String),
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

    /// Plug the device `xml` into the running machine, or pull it out, leaving the
    /// definition as it is.
    pub fn plug(&self, uuid: &str, xml: &str, plugged: bool) -> Result<()> {
        let dom = self.domain(uuid)?;
        let live = sys::VIR_DOMAIN_AFFECT_LIVE;
        if plugged {
            dom.attach_device_flags(xml, live)
        } else {
            dom.detach_device_flags(xml, live)
        }
        .map(drop)
        .map_err(message)
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
        let at_next_start = if active {
            Change::AtNextStart
        } else {
            Change::Done
        };
        let result = match apply(flags) {
            Ok(_) => return Ok(Change::Done),
            // Where the running machine cannot take the device at all, e.g. a SATA disk,
            // which cannot be hotplugged, the definition alone still takes the change. Any
            // other failure, such as an image that is not there, is the user's to see.
            Err(e)
                if active
                    && persistent
                    && matches!(
                        e.code(),
                        ErrorNumber::OperationUnsupported | ErrorNumber::ConfigUnsupported
                    ) =>
            {
                apply(config).map(|_| Change::AtNextStart)
            }
            Err(e) => Err(e),
        };
        match result {
            // Some devices, such as a TPM, libvirt adds and removes in no way at all; the
            // definition is edited instead.
            Err(e) if persistent && e.code() == ErrorNumber::OperationUnsupported => {
                let definition = dom
                    .get_xml_desc(sys::VIR_DOMAIN_XML_INACTIVE | sys::VIR_DOMAIN_XML_SECURE)
                    .map_err(message)?;
                let edited = if attach {
                    domain_xml::with_device(&definition, xml)?
                } else {
                    domain_xml::without_device(&definition, xml)?
                };
                Domain::define_xml(&self.conn, &edited).map_err(message)?;
                Ok(at_next_start)
            }
            result => result.map_err(message),
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

    /// Add a TPM, random number generator, sound card or shared folder. A shared folder
    /// needs memory shared with virtiofsd, which the definition gets first, and so a
    /// running machine without it only from its next start.
    pub fn add_gadget(&self, uuid: &str, gadget: &NewGadget) -> Result<Change> {
        let dom = self.domain(uuid)?;
        let xml = dom
            .get_xml_desc(sys::VIR_DOMAIN_XML_INACTIVE | sys::VIR_DOMAIN_XML_SECURE)
            .map_err(message)?;
        let config = MachineConfig::parse(&xml)?;
        let device = match gadget {
            NewGadget::Tpm => domain_xml::tpm_xml().to_owned(),
            NewGadget::Rng => domain_xml::rng_xml().to_owned(),
            NewGadget::Sound => domain_xml::sound_xml(&config.machine),
            NewGadget::SharedFolder(path) => {
                if let Some(shared) = domain_xml::with_shared_memory(&xml)? {
                    Domain::define_xml(&self.conn, &shared).map_err(message)?;
                }
                let taken: Vec<&str> = config
                    .gadgets
                    .iter()
                    .filter_map(|g| match &g.gadget {
                        Gadget::SharedFolder { tag, .. } => Some(tag.as_str()),
                        _ => None,
                    })
                    .collect();
                domain_xml::shared_folder_xml(path, &domain_xml::folder_tag(path, &taken))
            }
        };
        self.attach(uuid, &device)
    }

    /// Give the USB device `name`, a node device just plugged into the host, back to the
    /// running machine that had it before it was pulled out, and say which one that is.
    ///
    /// libvirt holds on to a USB device by the bus and device number it had, and the
    /// device gets another as it comes back, so the host would keep it otherwise.
    pub fn replug_usb(&self, name: &str) -> Result<Option<(HostDevice, String)>> {
        let Some(added) = NodeDevice::lookup_by_name(&self.conn, name)
            .and_then(|d| d.get_xml_desc(0))
            .ok()
            .and_then(|xml| HostDevice::parse(&xml))
            .filter(|d| matches!(d.id, HostDeviceId::Usb { .. }))
        else {
            return Ok(None);
        };
        let present: Vec<HostDeviceId> = self.host_devices()?.into_iter().map(|d| d.id).collect();
        for dom in self
            .conn
            .list_all_domains(sys::VIR_CONNECT_LIST_DOMAINS_ACTIVE)
            .map_err(message)?
        {
            let Some(live) = dom
                .get_xml_desc(0)
                .ok()
                .and_then(|xml| MachineConfig::parse(&xml).ok())
            else {
                continue;
            };
            let Some(stale) = lost_usb_device(&live.host_devices, &added.id, &present) else {
                continue;
            };
            let live_only = sys::VIR_DOMAIN_AFFECT_LIVE;
            dom.detach_device_flags(&stale.xml, live_only)
                .map_err(message)?;
            dom.attach_device_flags(&added.id.hostdev_xml(), live_only)
                .map_err(message)?;
            return Ok(Some((added, dom.get_name().map_err(message)?)));
        }
        Ok(None)
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

/// Of a running machine's host devices, `live`, the USB device of the same kind as
/// `added` that it lost, as `present`, the host's devices now, no longer has it where the
/// machine had it; `None` where the machine lost none, or has `added` already.
fn lost_usb_device<'a>(
    live: &'a [HostDev],
    added: &HostDeviceId,
    present: &[HostDeviceId],
) -> Option<&'a HostDev> {
    let HostDeviceId::Usb {
        vendor, product, ..
    } = added
    else {
        return None;
    };
    let alike = |d: &&HostDev| {
        matches!(&d.id, HostDeviceId::Usb { vendor: v, product: p, .. }
            if v == vendor && p == product)
    };
    if live.iter().filter(alike).any(|d| d.id == *added) {
        return None;
    }
    live.iter().filter(alike).find(|d| {
        matches!(
            d.id,
            HostDeviceId::Usb {
                address: Some(_),
                ..
            }
        ) && !present.contains(&d.id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb(product: u32, address: (u32, u32)) -> HostDeviceId {
        HostDeviceId::Usb {
            vendor: 0x046d,
            product,
            address: Some(address),
        }
    }

    fn hostdev(id: HostDeviceId) -> HostDev {
        HostDev {
            xml: id.hostdev_xml(),
            id,
        }
    }

    #[test]
    fn a_usb_device_plugged_in_again_goes_back_to_the_machine_that_lost_it() {
        let live = [hostdev(usb(0xc52b, (1, 5))), hostdev(usb(0xc077, (1, 6)))];
        let back = usb(0xc52b, (1, 9));
        // Its old place is gone from the host: it is the one the machine lost.
        let present = [back.clone(), usb(0xc077, (1, 6))];
        assert_eq!(lost_usb_device(&live, &back, &present), Some(&live[0]));
        // Its old place is still there: this is a second one like it, which stays.
        let twin = [back.clone(), usb(0xc52b, (1, 5)), usb(0xc077, (1, 6))];
        assert_eq!(lost_usb_device(&live, &back, &twin), None);
        // Another kind of device, or one the machine has already.
        assert_eq!(lost_usb_device(&live, &usb(0xaaaa, (1, 9)), &present), None);
        assert_eq!(lost_usb_device(&live, &usb(0xc077, (1, 6)), &present), None);
    }
}
