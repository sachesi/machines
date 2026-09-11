//! Storage pools and the volumes in them.

use virt::storage_pool::StoragePool;
use virt::storage_vol::StorageVol;

use super::{Hypervisor, Result, message};
use crate::host_xml::{self, PoolConfig};

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

    /// A pool of the image files in the directory `path`, made if it is not there, that
    /// starts with libvirt.
    pub fn create_pool(&self, name: &str, path: &str) -> Result<()> {
        let pool = StoragePool::define_xml(&self.conn, &host_xml::dir_pool_xml(name, path), 0)
            .map_err(message)?;
        if let Err(e) = pool.build(0).and_then(|_| pool.create(0)) {
            let _ = pool.undefine();
            return Err(message(e));
        }
        pool.set_autostart(true).map(drop).map_err(message)
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

    pub fn create_volume(&self, pool: &str, name: &str, gib: u64, format: &str) -> Result<()> {
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
