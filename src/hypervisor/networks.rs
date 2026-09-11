//! libvirt's virtual networks.

use virt::network::Network;

use super::{Hypervisor, Result, message};
use crate::host_xml::{self, NetworkConfig, NewNetwork};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualNetwork {
    pub uuid: String,
    pub name: String,
    pub config: NetworkConfig,
    pub active: bool,
    pub autostart: bool,
    pub persistent: bool,
}

impl Hypervisor {
    /// Every virtual network, by name.
    pub fn networks(&self) -> Result<Vec<VirtualNetwork>> {
        let networks = self.conn.list_all_networks(0).map_err(message)?;
        let mut networks: Vec<VirtualNetwork> = networks
            .iter()
            .filter_map(|n| network_info(n).ok())
            .collect();
        networks.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(networks)
    }

    fn network_by_uuid(&self, uuid: &str) -> Result<Network> {
        Network::lookup_by_uuid_string(&self.conn, uuid).map_err(message)
    }

    /// Define the network, start it, and have it start with libvirt.
    pub fn create_network(&self, new: &NewNetwork) -> Result<()> {
        let net =
            Network::define_xml(&self.conn, &host_xml::new_network_xml(new)).map_err(message)?;
        if let Err(e) = net.create() {
            let _ = net.undefine();
            return Err(message(e));
        }
        net.set_autostart(true).map(drop).map_err(message)
    }

    pub fn set_network_active(&self, uuid: &str, active: bool) -> Result<()> {
        let net = self.network_by_uuid(uuid)?;
        if active {
            net.create().map(drop)
        } else {
            net.destroy()
        }
        .map_err(message)
    }

    pub fn set_network_autostart(&self, uuid: &str, autostart: bool) -> Result<()> {
        self.network_by_uuid(uuid)?
            .set_autostart(autostart)
            .map(drop)
            .map_err(message)
    }

    /// Stop the network and forget it.
    pub fn remove_network(&self, uuid: &str) -> Result<()> {
        let net = self.network_by_uuid(uuid)?;
        if net.is_active().map_err(message)? {
            net.destroy().map_err(message)?;
        }
        if net.is_persistent().map_err(message)? {
            net.undefine().map_err(message)?;
        }
        Ok(())
    }
}

fn network_info(net: &Network) -> Result<VirtualNetwork> {
    let persistent = net.is_persistent().map_err(message)?;
    Ok(VirtualNetwork {
        uuid: net.get_uuid_string().map_err(message)?,
        name: net.get_name().map_err(message)?,
        config: NetworkConfig::parse(&net.get_xml_desc(0).map_err(message)?)?,
        active: net.is_active().map_err(message)?,
        autostart: persistent && net.get_autostart().unwrap_or(false),
        persistent,
    })
}
