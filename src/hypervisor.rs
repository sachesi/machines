//! The libvirt connection. Every call here blocks, some for as long as a polkit prompt is
//! up, so the window makes them from `gio::spawn_blocking`.

mod devices;
mod events;
mod networks;
mod serial;
mod snapshots;
mod storage;
mod usage;

pub use devices::{Change, NewGadget, NewStorage};
pub use events::Event;
pub use networks::VirtualNetwork;
pub use serial::{SerialStream, start_event_loop};
pub use storage::{HostUse, Pool, Volume};
pub use usage::Usage;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use gettextrs::gettext;
use virt::connect::Connect;
use virt::domain::Domain;
use virt::error::{ErrorDomain, ErrorNumber};
use virt::network::Network;
use virt::nodedev::NodeDevice;
use virt::storage_pool::StoragePool;
use virt::storage_vol::StorageVol;
use virt::stream::Stream;
use virt::sys;

use crate::domain_xml::{
    self, BootDevice, Capabilities, Cpu, Disk, Display, Firmware, GuestOs, MachineConfig,
    NetworkSource, NewMachine, Snapshot,
};
use crate::{glib, host_xml};

pub type Result<T> = std::result::Result<T, String>;

/// The namespace of what the app keeps in a machine's `<metadata>`.
const METADATA_NS: &str = "https://github.com/sachesi/machines/metadata/1";
/// Marks a machine whose firmware variables are to be made afresh at its next start.
const RESET_NVRAM: &str = "reset-nvram";

fn message(e: virt::error::Error) -> String {
    e.message().to_owned()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "MachinesMachineState")]
pub enum MachineState {
    #[default]
    ShutOff,
    Running,
    Paused,
    ShuttingDown,
    Crashed,
    Suspended,
}

impl MachineState {
    fn from_raw(state: sys::virDomainState) -> Self {
        match state {
            sys::VIR_DOMAIN_RUNNING | sys::VIR_DOMAIN_BLOCKED => Self::Running,
            sys::VIR_DOMAIN_PAUSED => Self::Paused,
            sys::VIR_DOMAIN_SHUTDOWN => Self::ShuttingDown,
            sys::VIR_DOMAIN_CRASHED => Self::Crashed,
            sys::VIR_DOMAIN_PMSUSPENDED => Self::Suspended,
            _ => Self::ShutOff,
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::ShutOff => gettext("Shut Off"),
            Self::Running => gettext("Running"),
            Self::Paused => gettext("Paused"),
            Self::ShuttingDown => gettext("Shutting Down"),
            Self::Crashed => gettext("Crashed"),
            Self::Suspended => gettext("Suspended"),
        }
    }

    /// Whether QEMU is up for this machine, whatever the guest is doing.
    pub fn is_active(self) -> bool {
        !matches!(self, Self::ShutOff | Self::Crashed)
    }
}

/// One machine as the last listing saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct MachineInfo {
    pub uuid: String,
    pub name: String,
    pub state: MachineState,
    pub persistent: bool,
    pub autostart: bool,
    /// Whether it was saved to disk, to resume from at its next start.
    pub saved: bool,
    /// What the machine boots with next: the inactive definition where there is one.
    pub config: Option<MachineConfig>,
    /// What QEMU is running with, while it runs, which differs from `config` after an edit
    /// until the next start.
    pub live: Option<MachineConfig>,
    /// What QEMU can give this kind of machine.
    pub capabilities: Capabilities,
    pub snapshots: Vec<Snapshot>,
}

impl MachineInfo {
    /// The state as the user reads it.
    pub fn status(&self) -> String {
        if self.saved && !self.state.is_active() {
            gettext("Saved")
        } else {
            self.state.label()
        }
    }
}

#[derive(Debug, Clone)]
pub struct InterfaceAddresses {
    pub mac: String,
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum InstallSource {
    /// Boot an installer ISO with a new, empty disk of this many GiB.
    Media { iso: String, disk_gib: u64 },
    /// Boot a disk image that already has a system on it.
    Import { image: String },
}

#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub name: String,
    pub os: GuestOs,
    pub osinfo: Option<String>,
    pub uefi: bool,
    pub memory_mib: u64,
    pub vcpus: u32,
    pub source: InstallSource,
}

/// What cloning a machine does with its disks.
#[derive(Debug, Clone, Default)]
pub struct ClonePlan {
    /// Images the copy gets copies of.
    pub copied: Vec<String>,
    /// Disks the copy goes without, as no storage pool has their images to copy.
    pub left_out: Vec<String>,
}

/// What the host can give its machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    pub cpus: u32,
    pub memory_mib: u64,
    /// Whether QEMU has UEFI firmware for new machines.
    pub uefi: bool,
    /// Whether libvirt runs on this computer, so that its files are the host's.
    pub local: bool,
    /// Whether QEMU runs as a user of its own, as the system connection's does, which
    /// opens only the files that user may.
    pub qemu_is_other_user: bool,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            cpus: 1,
            memory_mib: 1024,
            uefi: true,
            local: true,
            qemu_is_other_user: false,
        }
    }
}

pub struct Hypervisor {
    conn: Connect,
    uri: String,
    /// Domain capabilities by virtualization type, architecture and machine type, which
    /// only change with QEMU.
    capabilities: Mutex<HashMap<(String, String, String), Capabilities>>,
    watch: Mutex<events::Watch>,
}

impl Drop for Hypervisor {
    fn drop(&mut self) {
        self.unwatch();
        let _ = self.conn.close();
    }
}

impl Hypervisor {
    pub fn open(uri: &str) -> Result<Self> {
        let conn = Connect::open(Some(uri)).map_err(message)?;
        Ok(Self {
            conn,
            uri: uri.to_owned(),
            capabilities: Mutex::default(),
            watch: Mutex::default(),
        })
    }

    pub fn is_session(&self) -> bool {
        self.uri.contains("/session")
    }

    /// Whether libvirt runs on this host, and what the app sees of the host is its.
    fn is_local(&self) -> bool {
        self.uri
            .split_once("://")
            .is_none_or(|(_, rest)| rest.starts_with('/'))
    }

    pub fn host(&self) -> Host {
        let info = self.conn.get_node_info().ok();
        Host {
            cpus: info.as_ref().map_or(1, |i| i.cpus),
            memory_mib: info.as_ref().map_or(1024, |i| i.memory / 1024),
            uefi: self
                .new_machine_capabilities()
                .is_ok_and(|(_, caps)| Capabilities::parse(&caps).efi),
            local: self.is_local(),
            qemu_is_other_user: self.is_local() && !self.is_session(),
        }
    }

    /// The domain capabilities new machines are made with, x86-64 on q35 with KVM where
    /// the host has it, and the virtualization type they are for.
    fn new_machine_capabilities(&self) -> Result<(&'static str, String)> {
        let caps = |virt_type: &'static str| {
            self.conn
                .get_domain_capabilities(None, Some("x86_64"), Some("q35"), Some(virt_type), 0)
                .map(|caps| (virt_type, caps))
        };
        caps("kvm").or_else(|_| caps("qemu")).map_err(message)
    }

    pub fn machines(&self) -> Result<Vec<MachineInfo>> {
        let domains = self.conn.list_all_domains(0).map_err(message)?;
        Ok(domains.iter().filter_map(|d| self.info(d).ok()).collect())
    }

    fn info(&self, dom: &Domain) -> Result<MachineInfo> {
        let (state, _) = dom.get_state().map_err(message)?;
        let state = MachineState::from_raw(state);
        let persistent = dom.is_persistent().map_err(message)?;
        let flags = if persistent {
            sys::VIR_DOMAIN_XML_INACTIVE
        } else {
            0
        };
        let config = dom
            .get_xml_desc(flags)
            .ok()
            .and_then(|xml| MachineConfig::parse(&xml).ok());
        let live = if state.is_active() {
            dom.get_xml_desc(0)
                .ok()
                .and_then(|xml| MachineConfig::parse(&xml).ok())
        } else {
            None
        };
        let capabilities = config
            .as_ref()
            .map(|c| self.capabilities(c))
            .unwrap_or_default();
        Ok(MachineInfo {
            uuid: dom.get_uuid_string().map_err(message)?,
            name: dom.get_name().map_err(message)?,
            state,
            persistent,
            autostart: persistent && dom.get_autostart().unwrap_or(false),
            saved: persistent && dom.has_managed_save(0).unwrap_or(false),
            config,
            live,
            capabilities,
            snapshots: snapshots::snapshots(dom),
        })
    }

    fn capabilities(&self, config: &MachineConfig) -> Capabilities {
        let key = (
            config.virt_type.clone(),
            config.arch.clone(),
            config.machine.clone(),
        );
        let mut cache = self.capabilities.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry(key)
            .or_insert_with(|| {
                let caps = self.conn.get_domain_capabilities(
                    None,
                    Some(&config.arch)
                        .filter(|a| !a.is_empty())
                        .map(String::as_str),
                    Some(&config.machine)
                        .filter(|m| !m.is_empty())
                        .map(String::as_str),
                    Some(&config.virt_type)
                        .filter(|v| !v.is_empty())
                        .map(String::as_str),
                    0,
                );
                caps.map(|caps| Capabilities::parse(&caps))
                    .unwrap_or_default()
            })
            .clone()
    }

    /// The host's devices with the capabilities `flags` name.
    fn node_devices(
        &self,
        flags: sys::virConnectListAllNodeDeviceFlags,
    ) -> Result<Vec<NodeDevice>> {
        self.conn.list_all_node_devices(flags).map_err(|e| {
            // The daemon that lists them is a separate one, which some hosts leave off.
            if e.code() == ErrorNumber::SystemError && e.domain() == ErrorDomain::Rpc {
                gettext(
                    "The host’s devices cannot be listed, as libvirt’s node device service is \
                     not running. It starts with “systemctl enable --now virtnodedevd.socket”.",
                )
            } else {
                message(e)
            }
        })
    }

    fn domain(&self, uuid: &str) -> Result<Domain> {
        Domain::lookup_by_uuid_string(&self.conn, uuid).map_err(message)
    }

    pub fn start(&self, uuid: &str) -> Result<()> {
        let dom = self.domain(uuid)?;
        match MachineState::from_raw(dom.get_state().map_err(message)?.0) {
            MachineState::Paused => dom.resume().map(drop).map_err(message),
            MachineState::Suspended => dom.pm_wakeup(0).map(drop).map_err(message),
            _ => {
                let reset = dom
                    .get_metadata(
                        sys::VIR_DOMAIN_METADATA_ELEMENT as i32,
                        Some(METADATA_NS),
                        sys::VIR_DOMAIN_AFFECT_CONFIG,
                    )
                    .is_ok_and(|m| m.contains(RESET_NVRAM));
                let flags = if reset {
                    sys::VIR_DOMAIN_START_RESET_NVRAM
                } else {
                    0
                };
                dom.create_with_flags(flags).map_err(message)?;
                if reset {
                    let _ = self.set_reset_nvram(&dom, false);
                }
                Ok(())
            }
        }
    }

    /// Mark the machine's firmware variables to be made afresh at its next start, or clear
    /// the mark.
    fn set_reset_nvram(&self, dom: &Domain, reset: bool) -> Result<()> {
        let element = format!("<machine><{RESET_NVRAM}/></machine>");
        dom.set_metadata(
            sys::VIR_DOMAIN_METADATA_ELEMENT as i32,
            reset.then_some(element.as_str()),
            Some("machines"),
            Some(METADATA_NS),
            sys::VIR_DOMAIN_AFFECT_CONFIG,
        )
        .map(drop)
        .map_err(message)
    }

    pub fn shut_down(&self, uuid: &str) -> Result<()> {
        self.domain(uuid)?.shutdown().map(drop).map_err(message)
    }

    pub fn reboot(&self, uuid: &str) -> Result<()> {
        self.domain(uuid)?
            .reboot(sys::VIR_DOMAIN_REBOOT_DEFAULT)
            .map_err(message)
    }

    pub fn reset(&self, uuid: &str) -> Result<()> {
        self.domain(uuid)?.reset().map(drop).map_err(message)
    }

    pub fn force_off(&self, uuid: &str) -> Result<()> {
        self.domain(uuid)?.destroy().map_err(message)
    }

    /// Save the machine's memory to disk and stop it; its next start resumes from there.
    pub fn save(&self, uuid: &str) -> Result<()> {
        self.domain(uuid)?
            .managed_save(0)
            .map(drop)
            .map_err(message)
    }

    /// Throw away what `save` kept, so the next start boots afresh.
    pub fn discard_saved(&self, uuid: &str) -> Result<()> {
        self.domain(uuid)?
            .managed_save_remove(0)
            .map(drop)
            .map_err(message)
    }

    /// What the machine's first screen shows, as an image file QEMU chose the format of
    /// (PNG or PPM).
    pub fn screenshot(&self, uuid: &str) -> Result<Vec<u8>> {
        let dom = self.domain(uuid)?;
        let stream = Stream::new(&self.conn, 0).map_err(message)?;
        dom.screenshot(&stream, 0, 0).map_err(message)?;
        let mut image = Vec::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match stream.recv(&mut buf) {
                Ok(0) => break,
                Ok(n) => image.extend_from_slice(&buf[..n]),
                Err(e) => {
                    let _ = stream.abort();
                    return Err(message(e));
                }
            }
        }
        stream.finish().map_err(message)?;
        Ok(image)
    }

    pub fn pause(&self, uuid: &str) -> Result<()> {
        self.domain(uuid)?.suspend().map(drop).map_err(message)
    }

    pub fn set_autostart(&self, uuid: &str, autostart: bool) -> Result<()> {
        self.domain(uuid)?
            .set_autostart(autostart)
            .map(drop)
            .map_err(message)
    }

    /// Redefine the machine with its definition as `edit` changes it; takes effect at the
    /// next start.
    fn edit_definition(
        &self,
        uuid: &str,
        edit: impl FnOnce(&str) -> std::result::Result<String, String>,
    ) -> Result<()> {
        let xml = self.definition(uuid)?;
        Domain::define_xml(&self.conn, &edit(&xml)?)
            .map(drop)
            .map_err(message)
    }

    /// Takes effect at the next start. The topology follows the count, laid out as before.
    pub fn set_vcpus(&self, uuid: &str, vcpus: u32) -> Result<()> {
        self.edit_definition(uuid, |xml| {
            let cpu = Cpu {
                count: vcpus,
                ..MachineConfig::parse(xml)?.cpu
            };
            domain_xml::set_cpu(xml, &cpu)
        })
    }

    pub fn set_cpu(&self, uuid: &str, cpu: &Cpu) -> Result<()> {
        self.edit_definition(uuid, |xml| domain_xml::set_cpu(xml, cpu))
    }

    pub fn set_hugepages(&self, uuid: &str, on: bool) -> Result<()> {
        self.edit_definition(uuid, |xml| domain_xml::set_hugepages(xml, on))
    }

    /// Boot with `firmware` from the next start. Between UEFI with and without Secure Boot,
    /// the variables, keys among them, come afresh from the new firmware's template.
    pub fn set_firmware(&self, uuid: &str, firmware: Firmware) -> Result<()> {
        let old = MachineConfig::parse(&self.definition(uuid)?)?.firmware;
        self.edit_definition(uuid, |xml| domain_xml::set_firmware(xml, firmware))?;
        let uefi = |f| matches!(f, Firmware::Uefi | Firmware::UefiSecureBoot);
        if uefi(old) && uefi(firmware) && old != firmware {
            self.set_reset_nvram(&self.domain(uuid)?, true)?;
        }
        Ok(())
    }

    /// Takes effect at the next start, like [`Self::set_vcpus`].
    pub fn set_memory(&self, uuid: &str, mib: u64) -> Result<()> {
        let dom = self.domain(uuid)?;
        let xml = dom
            .get_xml_desc(sys::VIR_DOMAIN_XML_INACTIVE)
            .map_err(message)?;
        let current = MachineConfig::parse(&xml)?.memory_mib;
        let config = sys::VIR_DOMAIN_AFFECT_CONFIG;
        let maximum = config | sys::VIR_DOMAIN_MEM_MAXIMUM;
        let order = if mib > current {
            [maximum, config]
        } else {
            [config, maximum]
        };
        for flags in order {
            dom.set_memory_flags(mib * 1024, flags).map_err(message)?;
        }
        Ok(())
    }

    /// Boot from `order` from the next start on.
    pub fn set_boot_order(&self, uuid: &str, order: &[BootDevice]) -> Result<()> {
        self.edit_definition(uuid, |xml| domain_xml::set_boot_order(xml, order))
    }

    fn definition(&self, uuid: &str) -> Result<String> {
        self.domain(uuid)?
            .get_xml_desc(sys::VIR_DOMAIN_XML_INACTIVE | sys::VIR_DOMAIN_XML_SECURE)
            .map_err(message)
    }

    /// The machine's definition as libvirt keeps it, to edit by hand.
    pub fn xml(&self, uuid: &str) -> Result<String> {
        self.definition(uuid)
    }

    /// Replace the machine's definition with `xml`, which libvirt checks against its schema.
    pub fn define(&self, uuid: &str, xml: &str) -> Result<Change> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| e.to_string())?;
        // libvirt takes it as a C string.
        if xml.contains('\0') {
            return Err(gettext("The definition has a NUL character in it"));
        }
        let same = doc
            .root_element()
            .children()
            .find(|n| n.has_tag_name("uuid"))
            .and_then(|n| n.text())
            .is_some_and(|u| u.trim().eq_ignore_ascii_case(uuid));
        if !same {
            return Err(
                gettext("The definition has to keep the UUID {uuid}").replace("{uuid}", uuid)
            );
        }
        let dom = Domain::define_xml_flags(&self.conn, xml, sys::VIR_DOMAIN_DEFINE_VALIDATE)
            .map_err(message)?;
        Ok(if dom.is_active().map_err(message)? {
            Change::AtNextStart
        } else {
            Change::Done
        })
    }

    pub fn rename(&self, uuid: &str, name: &str) -> Result<()> {
        self.domain(uuid)?
            .rename(name, 0)
            .map(drop)
            .map_err(message)
    }

    /// The disks a clone copies: those it writes to, where their images are volumes of a
    /// pool. CD-ROMs and read-only disks it shares.
    pub fn clone_plan(&self, uuid: &str) -> Result<ClonePlan> {
        let config = MachineConfig::parse(&self.definition(uuid)?)?;
        let mut plan = ClonePlan::default();
        for (_, source) in self.writable_disks(&config) {
            if StorageVol::lookup_by_path(&self.conn, &source).is_ok() {
                plan.copied.push(source);
            } else {
                plan.left_out.push(source);
            }
        }
        Ok(plan)
    }

    fn writable_disks(&self, config: &MachineConfig) -> Vec<(String, String)> {
        config
            .disks
            .iter()
            .filter(|d| d.writable())
            .filter_map(|d| Some((d.xml.clone(), d.source.clone()?)))
            .collect()
    }

    /// Define a copy of the shut-off machine `uuid` named `name`, with copies of the disks
    /// [`Self::clone_plan`] names, in their pools, and return its UUID.
    pub fn clone_machine(&self, uuid: &str, name: &str) -> Result<String> {
        let dom = self.domain(uuid)?;
        if dom.is_active().map_err(message)? {
            return Err(gettext("Shut the virtual machine down to clone it"));
        }
        let xml = self.definition(uuid)?;
        let config = MachineConfig::parse(&xml)?;
        let mut disks = Vec::new();
        let mut made: Vec<StorageVol> = Vec::new();
        let copied = self
            .writable_disks(&config)
            .into_iter()
            .try_for_each(|(disk, source)| {
                let copy = match StorageVol::lookup_by_path(&self.conn, &source) {
                    Ok(vol) => {
                        let copy = self.copy_volume(&vol, name, &source)?;
                        let path = copy.get_path().map_err(message);
                        made.push(copy);
                        Some(path?)
                    }
                    Err(_) => None,
                };
                disks.push((disk, copy));
                Ok(())
            });
        let defined = copied.and_then(|()| {
            let dom = Domain::define_xml(&self.conn, &domain_xml::clone_xml(&xml, name, &disks)?)
                .map_err(message)?;
            dom.get_uuid_string().map_err(message)
        });
        if defined.is_err() {
            for vol in made {
                let _ = vol.delete(0);
            }
        }
        defined
    }

    /// A copy of `vol`, the image at `path`, in its pool, named after the machine `machine`.
    fn copy_volume(&self, vol: &StorageVol, machine: &str, path: &str) -> Result<StorageVol> {
        let pool = StoragePool::lookup_by_volume(vol).map_err(message)?;
        let format = vol
            .get_xml_desc(0)
            .ok()
            .and_then(|xml| host_xml::volume_format(&xml));
        let capacity = vol.get_info().map_err(message)?.capacity;
        let extension = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        let name = unused_volume_name(&pool, machine, &extension);
        let xml = host_xml::copy_volume_xml(&name, capacity, format.as_deref());
        StorageVol::create_xml_from(&pool, &xml, vol, 0).map_err(message)
    }

    /// Put `source` in the CD-ROM drive `disk`, or empty it, now and for later starts.
    pub fn change_media(&self, uuid: &str, disk: &Disk, source: Option<&str>) -> Result<()> {
        let dom = self.domain(uuid)?;
        let mut flags = sys::VIR_DOMAIN_DEVICE_MODIFY_CONFIG;
        if dom.is_active().map_err(message)? {
            flags |= sys::VIR_DOMAIN_DEVICE_MODIFY_LIVE;
        }
        dom.update_device_flags(&domain_xml::cdrom_xml(disk, source), flags)
            .map(drop)
            .map_err(message)
    }

    /// Give the machine `display` from its next start; see [`domain_xml::set_display`].
    pub fn set_display(&self, uuid: &str, display: &Display) -> Result<()> {
        let dom = self.domain(uuid)?;
        let xml = dom
            .get_xml_desc(sys::VIR_DOMAIN_XML_INACTIVE | sys::VIR_DOMAIN_XML_SECURE)
            .map_err(message)?;
        Domain::define_xml(&self.conn, &domain_xml::set_display(&xml, display)?)
            .map(drop)
            .map_err(message)
    }

    /// A socket to the machine's first display, already past its authentication.
    pub fn open_display(&self, uuid: &str) -> Result<i32> {
        let fd = self
            .domain(uuid)?
            .open_graphics_fd(0, sys::VIR_DOMAIN_OPEN_GRAPHICS_SKIPAUTH)
            .map_err(message)?;
        i32::try_from(fd).map_err(|e| e.to_string())
    }

    /// Press and release `keys`, Linux input codes, together.
    pub fn send_keys(&self, uuid: &str, keys: &[u32]) -> Result<()> {
        let mut keys = keys.to_vec();
        let n = i32::try_from(keys.len()).map_err(|e| e.to_string())?;
        self.domain(uuid)?
            .send_key(sys::VIR_KEYCODE_SET_LINUX, 0, keys.as_mut_ptr(), n, 0)
            .map_err(message)
    }

    /// Addresses from the DHCP leases of libvirt's own networks, or else from the guest
    /// agent. Empty when neither knows any.
    pub fn addresses(&self, uuid: &str) -> Vec<InterfaceAddresses> {
        let Ok(dom) = self.domain(uuid) else {
            return Vec::new();
        };
        for source in [
            sys::VIR_DOMAIN_INTERFACE_ADDRESSES_SRC_LEASE,
            sys::VIR_DOMAIN_INTERFACE_ADDRESSES_SRC_AGENT,
        ] {
            let found: Vec<InterfaceAddresses> = dom
                .interface_addresses(source, 0)
                .unwrap_or_default()
                .into_iter()
                .filter(|i| i.name != "lo" && !i.addrs.is_empty())
                .map(|i| InterfaceAddresses {
                    mac: i.hwaddr,
                    addresses: i.addrs.into_iter().map(|a| a.addr).collect(),
                })
                .collect();
            if !found.is_empty() {
                return found;
            }
        }
        Vec::new()
    }

    /// Whether each of `paths` is a volume of a storage pool.
    pub fn in_pools(&self, paths: &[String]) -> Vec<bool> {
        paths
            .iter()
            .map(|p| StorageVol::lookup_by_path(&self.conn, p).is_ok())
            .collect()
    }

    /// Remove the machine, with its firmware variables, saved state and snapshot records,
    /// and delete the disk images `images`; the ones that could not be deleted are returned.
    pub fn delete(&self, uuid: &str, images: &[String]) -> Result<Vec<String>> {
        let dom = self.domain(uuid)?;
        if dom.is_active().map_err(message)? {
            dom.destroy().map_err(message)?;
        }
        if dom.is_persistent().map_err(message)? {
            dom.undefine_flags(
                sys::VIR_DOMAIN_UNDEFINE_NVRAM
                    | sys::VIR_DOMAIN_UNDEFINE_TPM
                    | sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE
                    | sys::VIR_DOMAIN_UNDEFINE_SNAPSHOTS_METADATA
                    | sys::VIR_DOMAIN_UNDEFINE_CHECKPOINTS_METADATA,
            )
            .map_err(message)?;
        }
        Ok(images
            .iter()
            .filter(|f| self.delete_image(f).is_err())
            .cloned()
            .collect())
    }

    fn delete_image(&self, path: &str) -> Result<()> {
        match StorageVol::lookup_by_path(&self.conn, path) {
            Ok(vol) => vol.delete(0).map_err(message),
            // Not in any pool: only a session's images are ours to remove directly.
            Err(e) if e.code() == ErrorNumber::NoStorageVolume && self.is_session() => {
                std::fs::remove_file(path).map_err(|e| e.to_string())
            }
            Err(e) => Err(message(e)),
        }
    }

    /// Define the machine and start it. Its UUID comes back even if only the start failed,
    /// since the machine is there to select.
    pub fn create(
        &self,
        req: &CreateRequest,
    ) -> std::result::Result<String, (Option<String>, String)> {
        let fail = |e: String| (None, e);
        let (virt_type, caps) = self.new_machine_capabilities().map_err(fail)?;
        let (disk, cdrom, new_vol) = match &req.source {
            InstallSource::Media { iso, disk_gib } => {
                let pool = self.default_pool().map_err(fail)?;
                let vol = self.new_disk(&pool, &req.name, *disk_gib).map_err(fail)?;
                let path = vol.get_path().map_err(message).map_err(fail)?;
                (
                    Some((path, "qcow2".to_owned())),
                    Some(iso.clone()),
                    Some(vol),
                )
            }
            InstallSource::Import { image } => (
                Some((image.clone(), image_format(Path::new(image)))),
                None,
                None,
            ),
        };
        let machine = NewMachine {
            name: req.name.clone(),
            virt_type: virt_type.to_owned(),
            os: req.os,
            osinfo: req.osinfo.clone(),
            uefi: req.uefi,
            // swtpm has to be on the host, which from here can only be seen when it is this
            // computer.
            tpm: req.os == GuestOs::Windows && (!self.is_local() || on_path("swtpm")),
            memory_mib: req.memory_mib,
            vcpus: req.vcpus,
            disk,
            cdrom,
            network: self.network(),
            video: domain_xml::video_model(&caps, req.os),
            spice: Capabilities::parse(&caps)
                .graphics
                .iter()
                .any(|g| g == "spice"),
        };
        let dom = match Domain::define_xml(&self.conn, &domain_xml::new_machine_xml(&machine)) {
            Ok(dom) => dom,
            Err(e) => {
                if let Some(vol) = new_vol {
                    let _ = vol.delete(0);
                }
                return Err((None, message(e)));
            }
        };
        let uuid = dom.get_uuid_string().map_err(message).map_err(fail)?;
        match dom.create() {
            Ok(_) => Ok(uuid),
            Err(e) => Err((Some(uuid), message(e))),
        }
    }

    /// libvirt's `default` network where there is one, started if it is not; QEMU's user
    /// networking otherwise, and always in a session, which cannot use the system's
    /// networks.
    fn network(&self) -> NetworkSource {
        if self.is_session() {
            return NetworkSource::User;
        }
        match Network::lookup_by_name(&self.conn, "default") {
            Ok(net) => {
                if !net.is_active().unwrap_or(true) {
                    let _ = net.create();
                }
                NetworkSource::Network("default".to_owned())
            }
            Err(_) => NetworkSource::User,
        }
    }

    /// A new volume in `pool`, named after the machine: a qcow2 image, or in a volume
    /// group, a logical volume.
    fn new_disk(&self, pool: &StoragePool, machine: &str, gib: u64) -> Result<StorageVol> {
        let logical = pool
            .get_xml_desc(0)
            .ok()
            .and_then(|xml| host_xml::PoolConfig::parse(&xml).ok())
            .is_some_and(|c| c.kind == "logical");
        let (extension, format) = if logical {
            ("", None)
        } else {
            (".qcow2", Some("qcow2"))
        };
        let name = unused_volume_name(pool, machine, extension);
        StorageVol::create_xml(pool, &host_xml::volume_xml(&name, gib, format), 0).map_err(message)
    }

    /// Where virt-manager keeps disk images.
    fn images_dir(&self) -> PathBuf {
        if self.is_session() {
            glib::user_data_dir().join("libvirt/images")
        } else {
            PathBuf::from("/var/lib/libvirt/images")
        }
    }

    /// The `default` storage pool, set up where virt-manager would put it if it is missing.
    fn default_pool(&self) -> Result<StoragePool> {
        let pool = match StoragePool::lookup_by_name(&self.conn, "default") {
            Ok(pool) => pool,
            Err(_) => {
                let dir = self.images_dir().to_string_lossy().into_owned();
                let xml = host_xml::pool_xml("default", &host_xml::PoolSource::Dir(dir));
                let pool = StoragePool::define_xml(&self.conn, &xml, 0).map_err(message)?;
                pool.build(0).map_err(message)?;
                let _ = pool.set_autostart(true);
                pool
            }
        };
        if !pool.is_active().map_err(message)? {
            pool.create(0).map_err(message)?;
        }
        Ok(pool)
    }
}

/// A name for a new volume in `pool`, after the machine `machine`, that no volume there has.
fn unused_volume_name(pool: &StoragePool, machine: &str, extension: &str) -> String {
    let _ = pool.refresh(0);
    let stem = machine
        .split(|c: char| !(c.is_alphanumeric() || "._".contains(c)))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    (0..)
        .map(|i| match i {
            0 => format!("{stem}{extension}"),
            i => format!("{stem}-{i}{extension}"),
        })
        .find(|n| StorageVol::lookup_by_name(pool, n).is_err())
        .expect("an unused name")
}

/// qcow2 by its magic number, where the file can be read; otherwise by extension, with raw
/// for anything unknown.
fn image_format(path: &Path) -> String {
    use std::io::Read;
    let mut magic = [0u8; 4];
    let read = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic));
    if read.is_ok() && magic == *b"QFI\xfb" {
        return "qcow2".to_owned();
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext @ ("qcow2" | "vmdk" | "vdi" | "vhdx")) => ext.to_owned(),
        Some("vhd") => "vpc".to_owned(),
        _ => "raw".to_owned(),
    }
}

fn on_path(program: &str) -> bool {
    glib::find_program_in_path(program).is_some()
}
