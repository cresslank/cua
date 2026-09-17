//! Typed snapshot projection backed by the hardened generation registry.
use crate::element_token::{self, MutationPermit, RegistryError, ResolvedElement};
use crate::protocol::ToolResult;
use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

pub trait SnapshotPayload: Send + Sync + 'static {
    type Element;
    fn len(&self) -> usize;
    fn retain(&self, index: usize) -> Option<Self::Element>;
}

/// Both the native reference and admission outlive detached blocking work.
#[derive(Debug)]
pub struct AdmittedElement<T> {
    element: T,
    _permit: Option<Arc<MutationPermit>>,
}
impl<T: Clone> Clone for AdmittedElement<T> {
    fn clone(&self) -> Self {
        Self {
            element: self.element.clone(),
            _permit: self._permit.clone(),
        }
    }
}
impl<T> Drop for AdmittedElement<T> {
    fn drop(&mut self) {
        if let Some(permit) = self._permit.take() {
            let identity = permit.identity();
            drop(permit);
            element_token::global().reap_stale_generation(identity);
        }
    }
}
impl<T> std::ops::Deref for AdmittedElement<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.element
    }
}

pub struct ElementCacheCore<S: SnapshotPayload> {
    runtime_scope: String,
    _payload: std::marker::PhantomData<S>,
    owned: Mutex<HashMap<(i32, u64), element_token::SnapshotIdentity>>,
}
impl<S: SnapshotPayload> ElementCacheCore<S> {
    pub fn new() -> Self {
        Self {
            runtime_scope: current_runtime_scope(),
            _payload: std::marker::PhantomData,
            owned: Mutex::new(HashMap::new()),
        }
    }
    pub fn try_publish(&self, pid: i32, window_id: u64, payload: S) -> Result<u32, RegistryError> {
        if current_runtime_scope() != self.runtime_scope {
            return Err(RegistryError::NotCurrent);
        }
        let registry = element_token::global();
        let candidate =
            registry.prepare_current(pid, window_id, payload.len(), Arc::new(payload))?;
        let identity = candidate.identity();
        let id = registry.publish(candidate, Duration::from_secs(2))?;
        let mut owned = self.owned.lock().unwrap();
        owned.retain(|_, identity| registry.contains_generation(*identity));
        if registry.contains_generation(identity) {
            owned
                .entry((pid, window_id))
                .and_modify(|old| {
                    if identity.sequence > old.sequence {
                        *old = identity;
                    }
                })
                .or_insert(identity);
        }
        Ok(id)
    }
    /// Fixture convenience; production must propagate try_publish failures.
    pub fn publish(&self, pid: i32, window_id: u64, payload: S) -> u32 {
        self.try_publish(pid, window_id, payload)
            .expect("snapshot fixture publication")
    }
    pub fn resolve_element_args(
        &self,
        pid: i32,
        index: Option<usize>,
        token: Option<&str>,
        snapshot: Option<&str>,
        window: Option<u64>,
        tool: &str,
    ) -> Result<ResolvedElement<AdmittedElement<S::Element>>, ToolResult> {
        if current_runtime_scope() != self.runtime_scope {
            return Err(element_token::refusal(
                "generation_mismatch",
                "element_token belongs to another runtime generation".into(),
            ));
        }
        match element_token::resolve_element_args(pid, index, token, snapshot, window, tool)? {
            ResolvedElement::None => Ok(ResolvedElement::None),
            ResolvedElement::Element {
                window_id,
                element_index,
                snapshot_identity,
                via_token,
                ..
            } => {
                let permit = element_token::global()
                    .try_acquire_mutation(snapshot_identity)
                    .map_err(|e| element_token::refusal("stale_element_token", e.to_string()))?;
                let element = permit
                    .payload::<S>()
                    .and_then(|s| s.retain(element_index))
                    .ok_or_else(|| {
                        element_token::refusal(
                            "stale_element_token",
                            element_token::STALE_TOKEN_ERROR.into(),
                        )
                    })?;
                Ok(ResolvedElement::Element {
                    window_id,
                    element_index,
                    snapshot_identity,
                    via_token,
                    element: AdmittedElement {
                        element,
                        _permit: Some(Arc::new(permit)),
                    },
                })
            }
        }
    }
    pub fn remove(&self, pid: i32, window_id: u64) {
        let identity = self.owned.lock().unwrap().remove(&(pid, window_id));
        if let Some(identity) = identity {
            element_token::global().retire_generation(identity);
        }
    }
    pub fn clear(&self) -> usize {
        let owned = std::mem::take(&mut *self.owned.lock().unwrap());
        owned
            .into_values()
            .filter(|id| element_token::global().retire_generation(*id))
            .count()
    }
}
impl<S: SnapshotPayload> Default for ElementCacheCore<S> {
    fn default() -> Self {
        Self::new()
    }
}
impl<S: SnapshotPayload> Drop for ElementCacheCore<S> {
    fn drop(&mut self) {
        self.clear();
        let mut caches = runtime_caches().lock().unwrap();
        if caches
            .get(&self.runtime_scope)
            .is_some_and(|cache| std::ptr::addr_eq(cache.as_ptr(), self as *const Self))
        {
            caches.remove(&self.runtime_scope);
        }
        if caches.is_empty() {
            caches.shrink_to_fit();
        }
    }
}
trait RuntimeCache: Any + Send + Sync {}
impl<S: SnapshotPayload> RuntimeCache for ElementCacheCore<S> {}
fn runtime_caches() -> &'static Mutex<HashMap<String, Weak<dyn RuntimeCache>>> {
    static CACHES: OnceLock<Mutex<HashMap<String, Weak<dyn RuntimeCache>>>> = OnceLock::new();
    CACHES.get_or_init(|| Mutex::new(HashMap::new()))
}
fn current_runtime_scope() -> String {
    crate::tool::current_dispatch_runtime_scope().unwrap_or_else(|| "legacy".into())
}
pub fn register_runtime_cache<S: SnapshotPayload>(cache: &Arc<ElementCacheCore<S>>) {
    let erased: Arc<dyn RuntimeCache> = cache.clone();
    let mut caches = runtime_caches().lock().unwrap();
    caches.retain(|_, cache| cache.strong_count() > 0);
    caches.insert(cache.runtime_scope.clone(), Arc::downgrade(&erased));
}
pub fn current_runtime_cache<S: SnapshotPayload>() -> Option<Arc<ElementCacheCore<S>>> {
    let cache = runtime_caches()
        .lock()
        .unwrap()
        .get(&current_runtime_scope())?
        .upgrade()?;
    let erased: Arc<dyn Any + Send + Sync> = cache;
    erased.downcast().ok()
}
pub fn retire_runtime_scope(runtime_scope: &str) -> usize {
    runtime_caches().lock().unwrap().remove(runtime_scope);
    element_token::global().clear_runtime_scope(runtime_scope)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::element_token::token_for;
    use crate::snapshot_test_support::Payload;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn publish_then_resolve_returns_projection() {
        let cache = ElementCacheCore::new();
        let id = cache.publish(std::process::id() as i32, 7, Payload(vec![10, 20, 30]));
        let result = cache
            .resolve_element_args(
                std::process::id() as i32,
                None,
                Some(&token_for(id, 2)),
                None,
                None,
                "click",
            )
            .unwrap();
        assert!(matches!(
            result,
            ResolvedElement::Element { element, .. } if *element == 30
        ));
    }

    #[test]
    fn miss_returns_refusal() {
        let cache = ElementCacheCore::<Payload>::new();
        assert!(cache
            .resolve_element_args(
                std::process::id() as i32,
                None,
                Some(&token_for(0, 0)),
                None,
                None,
                "click"
            )
            .is_err());
    }

    #[test]
    fn membership_matches_payload_length() {
        let cache = ElementCacheCore::new();
        let id = cache.publish(std::process::id() as i32, 99, Payload(vec![1, 2, 3, 4, 5]));
        for index in 0..5 {
            assert!(cache
                .resolve_element_args(
                    std::process::id() as i32,
                    None,
                    Some(&token_for(id, index)),
                    None,
                    None,
                    "click"
                )
                .is_ok());
        }
        assert!(cache
            .resolve_element_args(
                std::process::id() as i32,
                None,
                Some(&token_for(id, 5)),
                None,
                None,
                "click"
            )
            .is_err());
    }

    struct DropCounter {
        owner: Weak<ElementCacheCore<DropCounter>>,
        drops: Arc<AtomicUsize>,
    }
    impl SnapshotPayload for DropCounter {
        type Element = ();
        fn len(&self) -> usize {
            1
        }
        fn retain(&self, index: usize) -> Option<()> {
            (index == 0).then_some(())
        }
    }
    impl Drop for DropCounter {
        fn drop(&mut self) {
            if let Some(owner) = self.owner.upgrade() {
                assert!(
                    owner.owned.try_lock().is_ok(),
                    "native cleanup ran under the storage lock"
                );
            }
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn replacement_remove_and_clear_run_drop_outside_lock() {
        let cache = Arc::new(ElementCacheCore::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let payload = || DropCounter {
            owner: Arc::downgrade(&cache),
            drops: drops.clone(),
        };
        cache.publish(std::process::id() as i32, 1, payload());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        cache.publish(std::process::id() as i32, 1, payload());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        cache.remove(std::process::id() as i32, 1);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        cache.publish(std::process::id() as i32, 1, payload());
        cache.clear();
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn window_identity_preserves_high_bits_through_resolution_and_retirement() {
        let cache = ElementCacheCore::new();
        let low = 7;
        let high = (1_u64 << 32) | low;
        let first = cache.publish(std::process::id() as i32, low, Payload(vec![10]));
        let second = cache.publish(std::process::id() as i32, high, Payload(vec![20]));
        let token = token_for(second, 0);
        let resolved = cache
            .resolve_element_args(
                std::process::id() as i32,
                None,
                Some(&token),
                None,
                Some(high),
                "click",
            )
            .unwrap();
        assert!(
            matches!(resolved, ResolvedElement::Element { window_id: Some(window), element, .. } if window == high && *element == 20)
        );
        assert!(cache
            .resolve_element_args(
                std::process::id() as i32,
                None,
                Some(&token),
                None,
                Some(low),
                "click"
            )
            .is_err());
        let handle = format!("s{second:08x}");
        assert!(cache
            .resolve_element_args(
                std::process::id() as i32,
                Some(0),
                None,
                Some(&handle),
                Some(high),
                "click"
            )
            .is_ok());
        assert!(cache
            .resolve_element_args(
                std::process::id() as i32,
                Some(0),
                None,
                Some(&handle),
                Some(low),
                "click"
            )
            .is_err());
        cache.remove(std::process::id() as i32, high);
        assert!(cache
            .resolve_element_args(
                std::process::id() as i32,
                None,
                Some(&token),
                None,
                None,
                "click"
            )
            .is_err());
        assert!(cache
            .resolve_element_args(
                std::process::id() as i32,
                None,
                Some(&token_for(first, 0)),
                None,
                Some(low),
                "click"
            )
            .is_ok());
    }

    #[test]
    fn bindings_sharing_a_scope_keep_independent_payload_ownership() {
        crate::tool::with_runtime_scope("snapshot-binding-ownership".into(), || {
            let first = Arc::new(ElementCacheCore::new());
            let second = Arc::new(ElementCacheCore::new());
            register_runtime_cache(&first);
            register_runtime_cache(&second);
            let first_id = first.publish(std::process::id() as i32, 7, Payload(vec![10]));
            let second_id = second.publish(std::process::id() as i32, 7, Payload(vec![20]));
            assert!(second
                .resolve_element_args(
                    std::process::id() as i32,
                    None,
                    Some(&token_for(first_id, 0)),
                    None,
                    None,
                    "click"
                )
                .is_err());
            drop(first);
            let resolved = second
                .resolve_element_args(
                    std::process::id() as i32,
                    None,
                    Some(&token_for(second_id, 0)),
                    None,
                    None,
                    "click",
                )
                .unwrap();
            assert!(matches!(
                resolved,
                ResolvedElement::Element { element, .. } if *element == 20
            ));
            assert!(Arc::ptr_eq(
                &current_runtime_cache::<Payload>().unwrap(),
                &second
            ));
            retire_runtime_scope("snapshot-binding-ownership");
        });
    }

    #[test]
    fn recording_discovery_does_not_extend_payload_lifetime() {
        crate::tool::with_runtime_scope("snapshot-weak-discovery".into(), || {
            let cache = Arc::new(ElementCacheCore::new());
            let drops = Arc::new(AtomicUsize::new(0));
            cache.publish(
                std::process::id() as i32,
                7,
                DropCounter {
                    owner: Arc::downgrade(&cache),
                    drops: drops.clone(),
                },
            );
            register_runtime_cache(&cache);
            assert_eq!(Arc::strong_count(&cache), 1);
            drop(cache);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(current_runtime_cache::<DropCounter>().is_none());
            {
                let caches = runtime_caches().lock().unwrap();
                assert!(!caches.contains_key("snapshot-weak-discovery"));
                if caches.is_empty() {
                    assert_eq!(caches.capacity(), 0);
                }
            }
            assert_eq!(retire_runtime_scope("snapshot-weak-discovery"), 0);
        });
    }

    #[test]
    fn default_impl_matches_new() {
        let _cache: ElementCacheCore<Payload> = ElementCacheCore::default();
    }
}
