//! AT-SPI immutable snapshot payload for Linux.
//!
//! Tokens and element keys are published together by the core generation
//! registry.  `ElementCache` is a thin compatibility reader; it owns no second
//! map and therefore cannot expose a token/cache split.

use super::{AtspiIdentity, AtspiNode};
use cua_driver_core::element_token::{
    MutationPermit, RegistryError, SnapshotCandidate, SnapshotIdentity,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

/// Both native addressing forms belong to the same immutable generation.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedElement {
    key: u64,
    identity: Option<AtspiIdentity>,
}

#[derive(Debug)]
pub struct CachedSnapshot {
    // Window-scoped walks keep application-wide (possibly sparse) indices.
    elements: HashMap<usize, CachedElement>,
}

fn collect_unique_elements<'a>(
    nodes: impl Iterator<Item = &'a AtspiNode>,
) -> Result<HashMap<usize, CachedElement>, RegistryError> {
    let mut keys = HashSet::new();
    let mut elements = HashMap::new();
    for node in nodes {
        let Some(index) = node.element_index else {
            continue;
        };
        if !keys.insert(node.element_key) || elements.contains_key(&index) {
            return Err(RegistryError::Collision);
        }
        elements.insert(
            index,
            CachedElement {
                key: node.element_key,
                identity: node.identity.clone(),
            },
        );
    }
    Ok(elements)
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
        let elements = collect_unique_elements(nodes.iter())?;
        // Core validates the index envelope. Exact sparse membership is checked
        // below, both at argument resolution and at mutation admission.
        let count = match elements.keys().max() {
            Some(index) => index.checked_add(1).ok_or(RegistryError::Collision)?,
            None => 0,
        };
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
        self.snapshot(pid, xid)?
            .elements
            .get(&idx)
            .map(|element| element.key)
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
            .and_then(|snapshot| snapshot.elements.get(&idx).map(|element| element.key))
            .ok_or(RegistryError::Stale)?;
        Ok((permit, key))
    }

    /// Resolve membership without substituting a dense vector offset for a
    /// public index. Mutation workers must still acquire their own permit.
    pub fn resolve_element_args(
        &self,
        pid: i32,
        index: Option<usize>,
        token: Option<&str>,
        snapshot: Option<&str>,
        window: Option<u64>,
        tool: &str,
    ) -> Result<
        cua_driver_core::element_token::ResolvedElement,
        cua_driver_core::protocol::ToolResult,
    > {
        use cua_driver_core::element_token::{self, ResolvedElement};
        let resolved =
            element_token::resolve_element_args_wide(pid, index, token, snapshot, window, tool)?;
        if let ResolvedElement::Element {
            snapshot_identity,
            element_index,
            ..
        } = &resolved
        {
            self.acquire_element_mutation(*snapshot_identity, *element_index)
                .map_err(|error| {
                    let message = error.to_string();
                    cua_driver_core::protocol::ToolResult::error(message.clone()).with_structured(
                        serde_json::json!({"status": "refused", "refusal": {
                            "code": "stale_element_token", "message": message,
                        }}),
                    )
                })?;
        }
        Ok(resolved)
    }

    /// Retain the X11 object/frame identity under the same generation permit as
    /// the Wayland key. Missing native identity is discovery-only, never an
    /// excuse to re-walk by ordinal or fall back to pixels.
    pub fn acquire_observed_mutation(
        &self,
        identity: SnapshotIdentity,
        idx: usize,
    ) -> Result<(MutationPermit, AtspiIdentity), RegistryError> {
        let permit = cua_driver_core::element_token::global().try_acquire_mutation(identity)?;
        let native = permit
            .payload::<CachedSnapshot>()
            .and_then(|snapshot| snapshot.elements.get(&idx)?.identity.clone())
            .ok_or(RegistryError::Stale)?;
        Ok((permit, native))
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
            identity: Some(AtspiIdentity {
                bus_name: ":1.1".into(),
                path: format!("/node/{key}"),
                frame_bus_name: ":1.1".into(),
                frame_path: "/frame".into(),
            }),
            depth: 0,
            parent_element_index: None,
            in_web_content: false,
        }
    }

    #[test]
    fn duplicate_stable_element_key_refuses_candidate_payload() {
        let nodes = [node(0, 41), node(1, 41)];
        assert_eq!(
            collect_unique_elements(nodes.iter()),
            Err(RegistryError::Collision)
        );
    }

    #[test]
    fn sparse_indices_are_members_not_dense_offsets() {
        let cache = ElementCache::new();
        let pid = std::process::id();
        let xid = 0x7f30_0001;
        let candidate = cache
            .prepare(pid, xid, &[node(11, 41), node(7, 99)])
            .unwrap();
        let identity = candidate.identity();
        let public = cache.publish(candidate).unwrap();
        assert_eq!(cache.element_count(pid, xid), 2);
        for (index, key) in [(11, 41), (7, 99)] {
            let token = cua_driver_core::element_token::token_for(public, index);
            assert!(cache
                .resolve_element_args(pid as i32, None, Some(&token), None, Some(xid), "click")
                .is_ok());
            assert_eq!(cache.get_element_key(pid, xid, index), Some(key));
            let (_permit, retained) = cache.acquire_observed_mutation(identity, index).unwrap();
            assert_eq!(retained.path, format!("/node/{key}"));
        }
        for index in [0, 1, 8, 12, 41, 99] {
            let token = cua_driver_core::element_token::token_for(public, index);
            assert!(cache
                .resolve_element_args(pid as i32, None, Some(&token), None, Some(xid), "click")
                .is_err());
            assert!(cache.acquire_element_mutation(identity, index).is_err());
        }
    }

    #[test]
    fn duplicate_indices_refuse_and_unindexed_nodes_are_not_members() {
        assert_eq!(
            collect_unique_elements([node(7, 41), node(7, 99)].iter()),
            Err(RegistryError::Collision)
        );
        let mut unindexed = node(8, 41);
        unindexed.element_index = None;
        let elements = collect_unique_elements([node(7, 41), unindexed].iter()).unwrap();
        assert_eq!(elements.len(), 1);
        assert!(!elements.contains_key(&8));
    }

    #[test]
    fn overflowing_index_refuses_instead_of_wrapping() {
        assert!(matches!(
            ElementCache::new().prepare(std::process::id(), 0x7f30_0002, &[node(usize::MAX, 41)]),
            Err(RegistryError::Collision)
        ));
    }

    #[test]
    fn missing_native_identity_cannot_fall_back_to_index_or_key_on_x11() {
        let cache = ElementCache::new();
        let pid = std::process::id();
        let mut discovery = node(0, 41);
        discovery.identity = None;
        let candidate = cache.prepare(pid, 0x7f30_0003, &[discovery]).unwrap();
        let identity = candidate.identity();
        cache.publish(candidate).unwrap();
        assert!(matches!(
            cache.acquire_observed_mutation(identity, 0),
            Err(RegistryError::Stale)
        ));
        // Exact Wayland continues to use its stable key and compositor proof.
        assert_eq!(cache.acquire_element_mutation(identity, 0).unwrap().1, 41);
    }

    #[test]
    fn replacement_retires_observed_identity_and_empty_scope_retires_tokens() {
        let cache = ElementCache::new();
        let pid = std::process::id();
        let xid = 0x7f30_0004;
        let first = cache.prepare(pid, xid, &[node(5, 41)]).unwrap();
        let first_identity = first.identity();
        let old_public = cache.publish(first).unwrap();
        let second = cache.prepare(pid, xid, &[node(5, 99)]).unwrap();
        let second_identity = second.identity();
        let public = cache.publish(second).unwrap();
        assert!(cache.acquire_observed_mutation(first_identity, 5).is_err());
        assert_eq!(
            cache
                .acquire_observed_mutation(second_identity, 5)
                .unwrap()
                .1
                .path,
            "/node/99"
        );
        let empty = cache.prepare(pid, xid, &[]).unwrap();
        cache.publish(empty).unwrap();
        for handle in [old_public, public] {
            let token = cua_driver_core::element_token::token_for(handle, 5);
            assert!(cache
                .resolve_element_args(pid as i32, None, Some(&token), None, Some(xid), "click")
                .is_err());
        }
        assert_eq!(cache.element_count(pid, xid), 0);
    }

    #[test]
    fn full_width_window_ids_remain_distinct() {
        let cache = ElementCache::new();
        let pid = std::process::id();
        let low = 0x7f30_0005;
        let high = (1_u64 << 40) | low;
        let low_id = cache
            .publish(cache.prepare(pid, low, &[node(7, 41)]).unwrap())
            .unwrap();
        let high_id = cache
            .publish(cache.prepare(pid, high, &[node(11, 99)]).unwrap())
            .unwrap();
        let high_token = cua_driver_core::element_token::token_for(high_id, 11);
        assert!(cache
            .resolve_element_args(
                pid as i32,
                None,
                Some(&high_token),
                None,
                Some(high),
                "click"
            )
            .is_ok());
        assert!(cache
            .resolve_element_args(
                pid as i32,
                None,
                Some(&high_token),
                None,
                Some(low),
                "click"
            )
            .is_err());
        let low_token = cua_driver_core::element_token::token_for(low_id, 7);
        assert!(cache
            .resolve_element_args(pid as i32, None, Some(&low_token), None, Some(low), "click")
            .is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observed_identity_permit_survives_cancelled_native_worker() {
        let cache = ElementCache::new();
        let pid = std::process::id();
        let xid = 0x7f30_0006;
        let first = cache.prepare(pid, xid, &[node(5, 41)]).unwrap();
        let first_identity = first.identity();
        cache.publish(first).unwrap();
        let (permit, retained) = cache.acquire_observed_mutation(first_identity, 5).unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let worker = cua_driver_core::blocking::spawn(move || {
            let _permit = permit;
            assert_eq!(retained.path, "/node/41");
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        let next = cache.prepare(pid, xid, &[node(5, 99)]).unwrap();
        let next_identity = next.identity();
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            sent.send(
                cua_driver_core::element_token::global().publish(next, Duration::from_secs(1)),
            )
            .unwrap();
        });
        assert!(received.recv_timeout(Duration::from_millis(40)).is_err());
        release_tx.send(()).unwrap();
        received
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert!(cache.acquire_observed_mutation(first_identity, 5).is_err());
        assert_eq!(
            cache
                .acquire_observed_mutation(next_identity, 5)
                .unwrap()
                .1
                .path,
            "/node/99"
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
        assert_eq!(
            permit.payload::<CachedSnapshot>().unwrap().elements[&0].key,
            41
        );
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
