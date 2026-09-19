//! A machine's snapshots.

use virt::domain::Domain;
use virt::domain_snapshot::DomainSnapshot;

use super::{Hypervisor, Result, message};
use crate::domain_xml::{self, Snapshot};

/// Every snapshot of `dom`, oldest first.
pub(super) fn snapshots(dom: &Domain) -> Vec<Snapshot> {
    let mut snapshots: Vec<Snapshot> = dom
        .list_all_snapshots(0)
        .unwrap_or_default()
        .iter()
        .filter_map(|s| {
            let mut snapshot = Snapshot::parse(&s.get_xml_desc(0).ok()?).ok()?;
            snapshot.current = s.is_current(0).unwrap_or(false);
            Some(snapshot)
        })
        .collect();
    snapshots.sort_by_key(|s| s.created);
    snapshots
}

impl Hypervisor {
    fn snapshot(&self, uuid: &str, name: &str) -> Result<DomainSnapshot> {
        DomainSnapshot::lookup_by_name(&self.domain(uuid)?, name, 0).map_err(message)
    }

    /// Save the machine's disks, and its memory while it runs, as the snapshot `name`.
    pub fn take_snapshot(&self, uuid: &str, name: &str, description: &str) -> Result<()> {
        DomainSnapshot::create_xml(
            &self.domain(uuid)?,
            &domain_xml::snapshot_xml(name, description),
            0,
        )
        .map(drop)
        .map_err(message)
    }

    /// Put the machine back as it was in the snapshot `name`: running if it ran then.
    pub fn revert_snapshot(&self, uuid: &str, name: &str) -> Result<()> {
        self.snapshot(uuid, name)?.revert(0).map_err(message)
    }

    /// Forget the snapshot `name`; the machine and the other snapshots stay as they are.
    pub fn delete_snapshot(&self, uuid: &str, name: &str) -> Result<()> {
        self.snapshot(uuid, name)?.delete(0).map_err(message)
    }
}
