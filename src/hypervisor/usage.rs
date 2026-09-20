//! What a running machine uses of the host, as counters to take rates from.

use virt::sys;

use super::{Hypervisor, Result, message};
use crate::domain_xml::MachineConfig;

/// How often the guest's balloon driver reports its memory, in seconds.
const MEMORY_STATS_PERIOD: i32 = 2;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Processor time the machine has had, in nanoseconds.
    pub cpu_ns: u64,
    pub vcpus: u32,
    /// The memory the guest uses, as it tells, else what QEMU holds of the host's.
    pub memory_used_kib: u64,
    pub memory_kib: u64,
    /// Bytes read from and written to its disks since it started.
    pub disk_read: u64,
    pub disk_written: u64,
    /// Bytes received and sent over its network interfaces since it started.
    pub net_received: u64,
    pub net_sent: u64,
}

impl Hypervisor {
    pub fn usage(&self, uuid: &str) -> Result<Usage> {
        let dom = self.domain(uuid)?;
        let info = dom.get_info().map_err(message)?;
        let config = MachineConfig::parse(&dom.get_xml_desc(0).map_err(message)?)?;
        let stats = dom.memory_stats(0).unwrap_or_default();
        let stat = |tag| stats.iter().find(|s| s.tag == tag).map(|s| s.val);
        let guest = stat(sys::VIR_DOMAIN_MEMORY_STAT_AVAILABLE)
            .zip(stat(sys::VIR_DOMAIN_MEMORY_STAT_UNUSED));
        let (memory_used_kib, memory_kib) = match guest {
            Some((available, unused)) => (available.saturating_sub(unused), available),
            None => {
                // The guest reports only once asked to; until it does, QEMU's share of the
                // host's memory stands in.
                let _ =
                    dom.set_memory_stats_period(MEMORY_STATS_PERIOD, sys::VIR_DOMAIN_AFFECT_LIVE);
                (
                    stat(sys::VIR_DOMAIN_MEMORY_STAT_RSS).unwrap_or(info.memory),
                    stat(sys::VIR_DOMAIN_MEMORY_STAT_ACTUAL_BALLOON).unwrap_or(info.max_mem),
                )
            }
        };
        let mut usage = Usage {
            cpu_ns: info.cpu_time,
            vcpus: info.nr_virt_cpu,
            memory_used_kib,
            memory_kib,
            ..Usage::default()
        };
        for disk in config.disks.iter().filter(|d| d.source.is_some()) {
            if let Ok(stats) = dom.get_block_stats(&disk.target) {
                usage.disk_read += stats.rd_bytes.max(0) as u64;
                usage.disk_written += stats.wr_bytes.max(0) as u64;
            }
        }
        // libvirt finds an interface by its MAC address as well as by its host device.
        for mac in config.nics.iter().filter_map(|n| n.mac.as_deref()) {
            if let Ok(stats) = dom.interface_stats(mac) {
                usage.net_received += stats.rx_bytes.max(0) as u64;
                usage.net_sent += stats.tx_bytes.max(0) as u64;
            }
        }
        Ok(usage)
    }
}
