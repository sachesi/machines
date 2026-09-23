//! Storage pools and the volumes in them.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use virt::storage_pool::StoragePool;
use virt::storage_vol::StorageVol;
use virt::stream::Stream;
use virt::sys;

use gettextrs::gettext;

use super::{Hypervisor, Result, message};
use crate::domain_xml::MachineConfig;
use crate::host_xml::{self, HostDisk, PoolConfig, PoolSource};

/// How much of a file goes to libvirt at a time when uploading it.
const UPLOAD_CHUNK: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    pub uuid: String,
    pub name: String,
    pub config: PoolConfig,
    pub active: bool,
    pub autostart: bool,
    pub persistent: bool,
    /// Bytes, as of the last refresh.
    pub capacity: u64,
    pub available: u64,
    /// Empty while the pool is not active.
    pub volumes: Vec<Volume>,
}

impl Pool {
    /// Whether new qcow2 images can go in it: it is a directory of files, and running.
    pub fn holds_images(&self) -> bool {
        self.active && matches!(self.config.kind.as_str(), "dir" | "fs" | "netfs")
    }

    /// Whether new volumes can be made in it: image files, or logical volumes.
    pub fn makes_volumes(&self) -> bool {
        self.holds_images() || self.active && self.config.kind == "logical"
    }
}

/// Whether the host has a disk in use itself, and a machine must keep off it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostUse {
    Free,
    /// Mounted, swapped to, or under a device mapper, RAID or bcache device.
    InUse,
    /// The connection is to another host, whose mounts cannot be seen from here.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    pub name: String,
    pub path: String,
    pub format: Option<String>,
    pub capacity: u64,
    /// What it takes up on the host, less than the capacity for a sparse image.
    pub allocation: u64,
}

impl Hypervisor {
    /// Every pool with its volumes, by name.
    pub fn pools(&self) -> Result<Vec<Pool>> {
        let pools = self.conn.list_all_storage_pools(0).map_err(message)?;
        let mut pools: Vec<Pool> = pools.iter().filter_map(|p| pool_info(p).ok()).collect();
        pools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(pools)
    }

    fn pool(&self, uuid: &str) -> Result<StoragePool> {
        StoragePool::lookup_by_uuid_string(&self.conn, uuid).map_err(message)
    }

    /// A pool of `source`, started, that starts with libvirt. A directory, or an NFS
    /// export's mount point, is made if it is not there.
    pub fn create_pool(&self, name: &str, source: &PoolSource) -> Result<()> {
        let pool = StoragePool::define_xml(&self.conn, &host_xml::pool_xml(name, source), 0)
            .map_err(message)?;
        let started = if source.needs_build() {
            pool.build(0).and_then(|_| pool.create(0))
        } else {
            pool.create(0)
        };
        if let Err(e) = started {
            let _ = pool.undefine();
            return Err(message(e));
        }
        pool.set_autostart(true).map(drop).map_err(message)
    }

    /// Where a new NFS pool of this name gets mounted.
    pub fn nfs_mount_point(&self, name: &str) -> String {
        format!("{}/{name}", self.images_dir().to_string_lossy())
    }

    pub fn set_pool_active(&self, uuid: &str, active: bool) -> Result<()> {
        let pool = self.pool(uuid)?;
        if active {
            pool.create(0).map(drop)
        } else {
            pool.destroy()
        }
        .map_err(message)
    }

    pub fn set_pool_autostart(&self, uuid: &str, autostart: bool) -> Result<()> {
        self.pool(uuid)?
            .set_autostart(autostart)
            .map(drop)
            .map_err(message)
    }

    /// Stop the pool and forget it; its directory and the files in it stay.
    pub fn remove_pool(&self, uuid: &str) -> Result<()> {
        let pool = self.pool(uuid)?;
        if pool.is_active().map_err(message)? {
            pool.destroy().map_err(message)?;
        }
        if pool.is_persistent().map_err(message)? {
            pool.undefine().map_err(message)?;
        }
        Ok(())
    }

    pub fn create_volume(
        &self,
        pool: &str,
        name: &str,
        gib: u64,
        format: Option<&str>,
    ) -> Result<()> {
        StorageVol::create_xml(
            &self.pool(pool)?,
            &host_xml::volume_xml(name, gib, format),
            0,
        )
        .map(drop)
        .map_err(message)
    }

    pub fn delete_volume(&self, path: &str) -> Result<()> {
        StorageVol::lookup_by_path(&self.conn, path)
            .and_then(|vol| vol.delete(0))
            .map_err(message)
    }

    /// How big the disk `target` of the machine `uuid` is to its guest, in bytes.
    pub fn disk_capacity(&self, uuid: &str, target: &str) -> Result<u64> {
        self.domain(uuid)?
            .get_block_info(target, 0)
            .map(|info| info.capacity)
            .map_err(message)
    }

    /// Grow the volume at `path` to `bytes`. A running machine that has it as a disk
    /// grows it itself, so that the guest sees the new size at once and QEMU's lock on
    /// the image is no obstacle.
    pub fn resize_volume(&self, path: &str, bytes: u64) -> Result<()> {
        for dom in self
            .conn
            .list_all_domains(sys::VIR_CONNECT_LIST_DOMAINS_ACTIVE)
            .map_err(message)?
        {
            let Some(config) = dom
                .get_xml_desc(0)
                .ok()
                .and_then(|xml| MachineConfig::parse(&xml).ok())
            else {
                continue;
            };
            if let Some(disk) = config
                .disks
                .iter()
                .find(|d| d.source.as_deref() == Some(path))
            {
                return dom
                    .block_resize(&disk.target, bytes, sys::VIR_DOMAIN_BLOCK_RESIZE_BYTES)
                    .map(drop)
                    .map_err(message);
            }
        }
        StorageVol::lookup_by_path(&self.conn, path)
            .and_then(|vol| vol.resize(bytes, 0))
            .map(drop)
            .map_err(message)
    }

    /// Copy the local file `file` into the pool as a new volume of its name, counting the
    /// bytes sent in `sent`. A volume left half written is deleted.
    pub fn upload_volume(&self, pool: &str, file: &Path, sent: &Arc<AtomicU64>) -> Result<()> {
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| file.display().to_string())?;
        let mut local = fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let bytes = local.metadata().map_err(|e| e.to_string())?.len();
        let vol = StorageVol::create_xml(
            &self.pool(pool)?,
            &host_xml::upload_volume_xml(&name, bytes),
            0,
        )
        .map_err(message)?;
        let mut upload = || -> Result<()> {
            let stream = Stream::new(&self.conn, 0).map_err(message)?;
            vol.upload(&stream, 0, bytes, 0).map_err(message)?;
            let mut buffer = vec![0u8; UPLOAD_CHUNK];
            loop {
                let n = local.read(&mut buffer).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                let mut chunk = &buffer[..n];
                while !chunk.is_empty() {
                    match stream.send(chunk) {
                        Err(e) => {
                            let _ = stream.abort();
                            return Err(message(e));
                        }
                        Ok(0) => {
                            let _ = stream.abort();
                            return Err(gettext("libvirt stopped taking the file"));
                        }
                        Ok(written) => {
                            chunk = &chunk[written..];
                            sent.fetch_add(written as u64, Ordering::Relaxed);
                        }
                    }
                }
            }
            stream.finish().map_err(message)
        };
        upload().inspect_err(|_| {
            let _ = vol.delete(0);
        })
    }

    /// The host's disks, by device node.
    pub fn host_disks(&self) -> Result<Vec<(HostDisk, HostUse)>> {
        let devices = self.node_devices(sys::VIR_CONNECT_LIST_NODE_DEVICES_CAP_STORAGE)?;
        let mut disks: Vec<(HostDisk, HostUse)> = devices
            .iter()
            .filter_map(|d| d.get_xml_desc(0).ok())
            .filter_map(|xml| HostDisk::parse(&xml))
            .map(|disk| {
                let used = if !self.is_local() {
                    HostUse::Unknown
                } else if host_uses(&disk.block) {
                    HostUse::InUse
                } else {
                    HostUse::Free
                };
                (disk, used)
            })
            .collect();
        disks.sort_by(|a, b| a.0.block.cmp(&b.0.block));
        Ok(disks)
    }

    /// The PCI devices, by address, that disks the host uses hang off, such as the NVMe
    /// or SATA controller it runs from. Empty where the host is another computer.
    pub fn pci_devices_in_use(&self) -> Vec<String> {
        if !self.is_local() {
            return Vec::new();
        }
        let Ok(disks) = fs::read_dir("/sys/class/block") else {
            return Vec::new();
        };
        disks
            .flatten()
            .filter(|d| !d.path().join("partition").exists())
            .filter(|d| host_uses(&d.file_name().to_string_lossy()))
            // e.g. /sys/devices/pci0000:00/0000:00:01.1/0000:01:00.0/nvme/nvme0/nvme0n1
            .filter_map(|d| fs::canonicalize(d.path()).ok())
            .flat_map(|path| {
                path.iter()
                    .filter_map(|c| c.to_str())
                    .filter(|c| is_pci_address(c))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

/// Whether `text` is a PCI address as sysfs writes it, `0000:01:00.0`.
fn is_pci_address(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 12
        && bytes[4] == b':'
        && bytes[7] == b':'
        && bytes[10] == b'.'
        && text
            .chars()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7 | 10) || c.is_ascii_hexdigit())
}

/// Whether the disk `block`, or one of its partitions, is mounted, swapped to, or held by
/// another block device such as a LUKS mapping or an LVM volume.
fn host_uses(block: &str) -> bool {
    let Some(name) = Path::new(block).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let sys = Path::new("/sys/class/block");
    let mut names = vec![name.to_owned()];
    if let Ok(entries) = fs::read_dir(sys.join(name)) {
        names.extend(
            entries
                .flatten()
                .filter(|e| e.path().join("partition").exists())
                .map(|e| e.file_name().to_string_lossy().into_owned()),
        );
    }
    let held = names.iter().any(|n| {
        fs::read_dir(sys.join(n).join("holders")).is_ok_and(|mut holders| holders.next().is_some())
    });
    if held {
        return true;
    }
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let mounted = mountinfo
        .lines()
        .filter_map(|line| line.split_once(" - ")?.1.split(' ').nth(1));
    let swaps = fs::read_to_string("/proc/swaps").unwrap_or_default();
    let swapped = swaps
        .lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next());
    mounted.chain(swapped).any(|source| {
        fs::canonicalize(source.replace("\\040", " ")).is_ok_and(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| names.iter().any(|m| m == n))
        })
    })
}

fn pool_info(pool: &StoragePool) -> Result<Pool> {
    let active = pool.is_active().map_err(message)?;
    if active {
        let _ = pool.refresh(0);
    }
    let info = pool.get_info().map_err(message)?;
    let config = PoolConfig::parse(&pool.get_xml_desc(0).map_err(message)?)?;
    let mut volumes: Vec<Volume> = if active {
        pool.list_all_volumes(0)
            .unwrap_or_default()
            .iter()
            .filter_map(|v| volume_info(v).ok())
            .collect()
    } else {
        Vec::new()
    };
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    let persistent = pool.is_persistent().map_err(message)?;
    Ok(Pool {
        uuid: pool.get_uuid_string().map_err(message)?,
        name: pool.get_name().map_err(message)?,
        config,
        active,
        autostart: persistent && pool.get_autostart().unwrap_or(false),
        persistent,
        capacity: info.capacity,
        available: info.available,
        volumes,
    })
}

fn volume_info(vol: &StorageVol) -> Result<Volume> {
    let info = vol.get_info().map_err(message)?;
    Ok(Volume {
        name: vol.get_name().map_err(message)?,
        path: vol.get_path().map_err(message)?,
        format: vol
            .get_xml_desc(0)
            .ok()
            .and_then(|xml| host_xml::volume_format(&xml)),
        capacity: info.capacity,
        allocation: info.allocation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_addresses_among_sysfs_path_components() {
        assert!(is_pci_address("0000:01:00.0"));
        assert!(is_pci_address("0000:3d:1f.7"));
        assert!(!is_pci_address("pci0000:00"));
        assert!(!is_pci_address("target0:0:0"));
        assert!(!is_pci_address("0:0:0:0"));
        assert!(!is_pci_address("nvme0n1"));
    }
}
