//! AT-SPI immutable snapshot payload for Linux.
//!
//! Tokens and element keys are published together by the core generation
//! registry.  `ElementCache` is a thin compatibility reader; it owns no second
//! map and therefore cannot expose a token/cache split.

use super::AtspiNode;
use cua_driver_core::element_token::{RegistryError, SnapshotCandidate};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
pub struct CachedSnapshot {
    /// element_index → element_key (opaque AT-SPI path hash).
    pub elements: Vec<u64>,
}

fn collect_unique_element_keys<'a>(
    nodes: impl Iterator<Item = &'a AtspiNode>,
) -> Result<Vec<u64>, RegistryError> {
    let mut seen = HashSet::new();
    nodes
        .filter(|node| node.element_index.is_some())
        .map(|node| node.element_key)
        .map(|key| {
            if seen.insert(key) {
                Ok(key)
            } else {
                Err(RegistryError::Collision)
            }
        })
        .collect()
}

pub struct ElementCache;

impl ElementCache {
    pub fn new() -> Self {
        Self
    }

    /// Build the complete immutable Linux payload before taking any registry
    /// lock. The subsequent prepare call performs only bounded in-memory work.
    pub fn prepare(
        &self,
        pid: u32,
        xid: u64,
        nodes: &[AtspiNode],
    ) -> Result<SnapshotCandidate, RegistryError> {
        let elements = collect_unique_element_keys(nodes.iter())?;
        let count = elements.len();
        cua_driver_core::element_token::global().prepare_current(
            pid as i32,
            xid,
            count,
            Arc::new(CachedSnapshot { elements }),
        )
    }

    pub fn publish(&self, candidate: SnapshotCandidate) -> Result<u32, RegistryError> {
        // Action dispatch has not adopted permits in Slice 1, so ordinary
        // publication is expected to be immediate. A zero wait also guarantees
        // this synchronous tool path never sleeps while publishing.
        cua_driver_core::element_token::global().publish(candidate, Duration::ZERO)
    }

    fn snapshot(&self, pid: u32, xid: u64) -> Option<Arc<CachedSnapshot>> {
        cua_driver_core::element_token::global().current_payload(pid as i32, xid)
    }

    pub fn get_element_key(&self, pid: u32, xid: u64, idx: usize) -> Option<u64> {
        self.snapshot(pid, xid)?.elements.get(idx).copied()
    }

    pub fn element_count(&self, pid: u32, xid: u64) -> usize {
        self.snapshot(pid, xid)
            .map_or(0, |snapshot| snapshot.elements.len())
    }
}

impl Default for ElementCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(index: usize, key: u64) -> AtspiNode {
        AtspiNode {
            element_index: Some(index),
            role: "button".into(),
            name: None,
            value: None,
            checked: None,
            enabled: None,
            selected: None,
            description: None,
            actions: vec!["click".into()],
            element_key: key,
            depth: 0,
            parent_element_index: None,
            in_web_content: false,
        }
    }

    #[test]
    fn duplicate_stable_element_key_refuses_candidate_payload() {
        let nodes = [node(0, 41), node(1, 41)];
        assert_eq!(
            collect_unique_element_keys(nodes.iter()),
            Err(RegistryError::Collision)
        );
    }
}
