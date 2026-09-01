//! AT-SPI immutable snapshot payload for Linux.
//!
//! Tokens and element keys are published together by the core generation
//! registry.  `ElementCache` is a thin compatibility reader; it owns no second
//! map and therefore cannot expose a token/cache split.

use super::AtspiNode;
use cua_driver_core::element_token::{
    MutationPermit, RegistryError, SnapshotCandidate, SnapshotIdentity,
};
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
        cua_driver_core::element_token::global().publish(candidate, Duration::from_secs(2))
    }

    fn snapshot(&self, pid: u32, xid: u64) -> Option<Arc<CachedSnapshot>> {
        cua_driver_core::element_token::global().current_payload(pid as i32, xid)
    }

    pub fn get_element_key(&self, pid: u32, xid: u64, idx: usize) -> Option<u64> {
        self.snapshot(pid, xid)?.elements.get(idx).copied()
    }

    /// Admit mutation against exactly the generation resolved from the caller's
    /// token and read its key from that generation's immutable payload.
    pub fn acquire_element_mutation(
        &self,
        identity: SnapshotIdentity,
        idx: usize,
    ) -> Result<(MutationPermit, u64), RegistryError> {
        let permit = cua_driver_core::element_token::global().try_acquire_mutation(identity)?;
        let key = permit
            .payload::<CachedSnapshot>()
            .and_then(|snapshot| snapshot.elements.get(idx).copied())
            .ok_or(RegistryError::Stale)?;
        Ok((permit, key))
    }

    /// Start one native element mutation with the generation permit owned by
    /// the native task itself. Aborting or dropping the async waiter can detach
    /// a blocking task, so the permit must not remain in the caller's future.
    pub fn spawn_element_mutation<F, R>(
        &self,
        identity: SnapshotIdentity,
        idx: usize,
        mutation: F,
    ) -> Result<tokio::task::JoinHandle<R>, RegistryError>
    where
        F: FnOnce(u64) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (permit, key) = self.acquire_element_mutation(identity, idx)?;
        Ok(cua_driver_core::blocking::spawn(move || {
            let _permit = permit;
            mutation(key)
        }))
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
    use std::sync::atomic::{AtomicU64, Ordering};

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

    #[test]
    fn permit_reads_resolved_generation_and_stale_generation_refuses() {
        static NEXT_XID: AtomicU64 = AtomicU64::new(0x7f00_0000);
        let xid = NEXT_XID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let cache = ElementCache::new();
        let first = cache.prepare(pid, xid, &[node(0, 41)]).unwrap();
        let first_identity = first.identity();
        cache.publish(first).unwrap();
        let (permit, key) = cache.acquire_element_mutation(first_identity, 0).unwrap();
        assert_eq!(key, 41);

        let next = cache.prepare(pid, xid, &[node(0, 99)]).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            sent.send(
                cua_driver_core::element_token::global().publish(next, Duration::from_secs(1)),
            )
            .unwrap();
        });
        assert!(received.recv_timeout(Duration::from_millis(40)).is_err());
        assert_eq!(permit.payload::<CachedSnapshot>().unwrap().elements[0], 41);
        drop(permit);
        received
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert!(matches!(
            cache.acquire_element_mutation(first_identity, 0),
            Err(RegistryError::NotCurrent)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_waiter_cannot_publish_or_act_on_b_before_native_a_ends() {
        static NEXT_XID: AtomicU64 = AtomicU64::new(0x7f10_0000);
        let xid = NEXT_XID.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let cache = ElementCache::new();
        let first = cache.prepare(pid, xid, &[node(0, 41)]).unwrap();
        let first_identity = first.identity();
        cache.publish(first).unwrap();

        let active = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let active_in_a = active.clone();
        let task_a = cache
            .spawn_element_mutation(first_identity, 0, move |key| {
                assert_eq!(key, 41);
                active_in_a.store(true, Ordering::SeqCst);
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                active_in_a.store(false, Ordering::SeqCst);
            })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        task_a.abort();
        assert!(task_a.await.unwrap_err().is_cancelled());

        let second = cache.prepare(pid, xid, &[node(0, 99)]).unwrap();
        let second_identity = second.identity();
        let active_at_publish = active.clone();
        let (published_tx, published_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result =
                cua_driver_core::element_token::global().publish(second, Duration::from_secs(1));
            published_tx
                .send((result, active_at_publish.load(Ordering::SeqCst)))
                .unwrap();
        });
        assert!(published_rx
            .recv_timeout(Duration::from_millis(40))
            .is_err());

        release_tx.send(()).unwrap();
        let (published, overlapped_a) = published_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        published.unwrap();
        assert!(
            !overlapped_a,
            "generation B published while native A was active"
        );

        let active_in_b = active.clone();
        cache
            .spawn_element_mutation(second_identity, 0, move |key| {
                assert_eq!(key, 99);
                assert!(!active_in_b.load(Ordering::SeqCst));
            })
            .unwrap()
            .await
            .unwrap();
    }
}
