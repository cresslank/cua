//! Opaque element-token and atomic snapshot-publication substrate.
//!
//! Public tokens remain `s{8 lowercase hex}:{index}`. The public handle is only
//! a process-local lookup key; authoritative identity includes runtime nonce,
//! checked sequence, owner scope, PID incarnation, and full-width window ID.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

pub const LRU_CAP_PER_PID: usize = 8;
pub const STALE_TOKEN_ERROR: &str =
    "element_token is stale; call get_window_state again to refresh";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryCapacities {
    pub lanes: usize,
    pub generations: usize,
    pub candidate_builders: usize,
    pub admitted_mutations: usize,
    pub payload_records: usize,
    pub poison_records: usize,
}
impl Default for RegistryCapacities {
    fn default() -> Self {
        Self {
            lanes: 256,
            generations: 4096,
            candidate_builders: 256,
            admitted_mutations: 256,
            payload_records: 4096,
            poison_records: 256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LaneKey {
    pub runtime_owner: String,
    pub pid: i32,
    pub process_incarnation: u64,
    pub window_id: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SnapshotIdentity {
    pub runtime_nonce: u64,
    pub sequence: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenerationState {
    Open,
    Closing { ticket: u64 },
    Stale,
    Poisoned { reason: String },
}
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum RegistryError {
    #[error("capacity_exhausted: {resource}")]
    CapacityExhausted { resource: &'static str },
    #[error("snapshot generation exhausted")]
    GenerationExhausted,
    #[error("public snapshot handle exhausted")]
    PublicHandleExhausted,
    #[error("snapshot/public-handle collision")]
    Collision,
    #[error("runtime nonce entropy unavailable")]
    EntropyUnavailable,
    #[error("candidate was superseded")]
    Superseded,
    #[error("snapshot lane is closing")]
    Closing,
    #[error("snapshot lane is stale")]
    Stale,
    #[error("snapshot lane is poisoned: {0}")]
    Poisoned(String),
    #[error("snapshot publication timed out")]
    PublicationTimeout,
    #[error("snapshot is not exact-current")]
    NotCurrent,
    #[error("process incarnation unavailable for pid {0}")]
    ProcessIncarnationUnavailable(i32),
}

pub type PublicationPayload = dyn Any + Send + Sync;
pub struct SnapshotGeneration {
    identity: SnapshotIdentity,
    lane: LaneKey,
    public_handle: u32,
    max_element_index: Option<usize>,
    payload: Arc<PublicationPayload>,
}
impl std::fmt::Debug for SnapshotGeneration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotGeneration")
            .field("identity", &self.identity)
            .field("lane", &self.lane)
            .field("public_handle", &self.public_handle)
            .field("max_element_index", &self.max_element_index)
            .finish_non_exhaustive()
    }
}
impl SnapshotGeneration {
    pub fn identity(&self) -> SnapshotIdentity {
        self.identity
    }
    pub fn lane_key(&self) -> &LaneKey {
        &self.lane
    }
    pub fn public_handle(&self) -> u32 {
        self.public_handle
    }
    pub fn element_count(&self) -> usize {
        self.max_element_index.map_or(0, |m| m + 1)
    }
    pub fn payload<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        Arc::downcast::<T>(self.payload.clone()).ok()
    }
}

struct Lane {
    inner: Mutex<LaneInner>,
    drained: Condvar,
}
struct LaneInner {
    current: Option<Arc<SnapshotGeneration>>,
    state: GenerationState,
    admitted: usize,
    candidates: usize,
    latest_ticket: u64,
    abandoned_ticket: Option<u64>,
    validating_ticket: Option<u64>,
    poison_reserved: bool,
    access_tick: u64,
}
struct TokenBinding {
    lane: Weak<Lane>,
    lane_key: LaneKey,
    identity: SnapshotIdentity,
}
struct RegistryState {
    lanes: HashMap<LaneKey, Arc<Lane>>,
    bindings: HashMap<(String, i32, u32), TokenBinding>,
    candidates: usize,
    generations: usize,
    payload_records: usize,
}

trait ProcessIncarnationProvider: Send + Sync {
    fn incarnation(&self, pid: i32) -> Result<u64, RegistryError>;
}
struct NativeProcessIncarnationProvider;
impl ProcessIncarnationProvider for NativeProcessIncarnationProvider {
    fn incarnation(&self, pid: i32) -> Result<u64, RegistryError> {
        process_incarnation(pid)
    }
}
struct RuntimeNonceClaim {
    nonce: u64,
}
impl Drop for RuntimeNonceClaim {
    fn drop(&mut self) {
        let mut claimed = claimed_runtime_nonces().lock().unwrap();
        let removed = claimed.remove(&self.nonce);
        debug_assert!(removed, "active runtime nonce claim must be unique");
    }
}
struct RegistryInner {
    // Drop the claim before `state`: payload destructors may reenter registry
    // construction, and arbitrary payload code must never run under the nonce lock.
    _nonce_claim: Option<RuntimeNonceClaim>,
    state: Mutex<RegistryState>,
    capacities: RegistryCapacities,
    runtime_nonce: Result<u64, RegistryError>,
    next_sequence: AtomicU64,
    next_public_handle: AtomicU64,
    next_access: AtomicU64,
    admitted: AtomicUsize,
    poison_slots: AtomicUsize,
    process: Arc<dyn ProcessIncarnationProvider>,
    #[cfg(test)]
    before_drain_wait: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

pub trait IntoWindowId {
    fn into_window_id(self) -> u64;
}
impl IntoWindowId for u64 {
    fn into_window_id(self) -> u64 {
        self
    }
}
impl IntoWindowId for u32 {
    fn into_window_id(self) -> u64 {
        u64::from(self)
    }
}
impl IntoWindowId for i32 {
    fn into_window_id(self) -> u64 {
        u64::try_from(self).unwrap_or(0)
    }
}

pub struct TokenRegistry {
    inner: Arc<RegistryInner>,
}
pub struct SnapshotCandidate {
    registry: Arc<RegistryInner>,
    lane: Arc<Lane>,
    generation: Option<Arc<SnapshotGeneration>>,
    ticket: u64,
    registered: bool,
}
impl std::fmt::Debug for SnapshotCandidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotCandidate")
            .field("identity", &self.identity())
            .field("ticket", &self.ticket)
            .finish()
    }
}
impl SnapshotCandidate {
    pub fn identity(&self) -> SnapshotIdentity {
        self.generation.as_ref().unwrap().identity
    }
    pub fn public_handle(&self) -> u32 {
        self.generation.as_ref().unwrap().public_handle
    }
    pub fn ticket(&self) -> u64 {
        self.ticket
    }
}
impl Drop for SnapshotCandidate {
    fn drop(&mut self) {
        if self.registered {
            cancel_candidate(self);
        }
    }
}

pub struct MutationPermit {
    registry: Arc<RegistryInner>,
    lane: Arc<Lane>,
    generation: Arc<SnapshotGeneration>,
    released: bool,
}
impl std::fmt::Debug for MutationPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MutationPermit")
            .field("identity", &self.generation.identity)
            .field("released", &self.released)
            .finish()
    }
}
impl MutationPermit {
    pub fn identity(&self) -> SnapshotIdentity {
        self.generation.identity
    }
    pub fn poison_before_release(
        &mut self,
        reason: impl Into<String>,
    ) -> Result<(), RegistryError> {
        let mut inner = self.lane.inner.lock().unwrap();
        if inner.current.as_ref().map(|g| g.identity) != Some(self.generation.identity) {
            return Err(RegistryError::NotCurrent);
        }
        if matches!(inner.state, GenerationState::Poisoned { .. }) {
            return Ok(());
        }
        debug_assert!(
            inner.poison_reserved,
            "every admitted mutation reserves poison capacity"
        );
        inner.state = GenerationState::Poisoned {
            reason: reason.into(),
        };
        inner.abandoned_ticket = None;
        self.lane.drained.notify_all();
        Ok(())
    }
}
impl Drop for MutationPermit {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let mut release_poison_slot = false;
        {
            let mut inner = self.lane.inner.lock().unwrap();
            if inner.admitted > 0 {
                inner.admitted -= 1;
                self.registry.admitted.fetch_sub(1, Ordering::AcqRel);
            }
            if inner.admitted == 0 {
                if let GenerationState::Closing { ticket } = inner.state {
                    if inner.abandoned_ticket == Some(ticket) {
                        inner.state = GenerationState::Open;
                        inner.abandoned_ticket = None;
                    }
                }
                if inner.poison_reserved && !matches!(inner.state, GenerationState::Poisoned { .. })
                {
                    inner.poison_reserved = false;
                    release_poison_slot = true;
                }
                self.lane.drained.notify_all();
            }
        }
        if release_poison_slot {
            self.registry.poison_slots.fetch_sub(1, Ordering::AcqRel);
        }
        self.released = true;
    }
}

impl TokenRegistry {
    fn new() -> Self {
        Self::with_parts(
            RegistryCapacities::default(),
            random_nonce(),
            1,
            1,
            Arc::new(NativeProcessIncarnationProvider),
        )
    }
    pub fn with_capacities(capacities: RegistryCapacities) -> Self {
        Self::with_parts(
            capacities,
            random_nonce(),
            1,
            1,
            Arc::new(NativeProcessIncarnationProvider),
        )
    }
    fn with_parts(
        capacities: RegistryCapacities,
        runtime_nonce: Result<u64, RegistryError>,
        next_sequence: u64,
        next_public_handle: u64,
        process: Arc<dyn ProcessIncarnationProvider>,
    ) -> Self {
        let claimed = runtime_nonce.and_then(claim_runtime_nonce);
        let (runtime_nonce, _nonce_claim) = match claimed {
            Ok(claim) => (Ok(claim.nonce), Some(claim)),
            Err(error) => (Err(error), None),
        };
        Self {
            inner: Arc::new(RegistryInner {
                _nonce_claim,
                state: Mutex::new(RegistryState {
                    lanes: HashMap::new(),
                    bindings: HashMap::new(),
                    candidates: 0,
                    generations: 0,
                    payload_records: 0,
                }),
                capacities,
                runtime_nonce,
                next_sequence: AtomicU64::new(next_sequence),
                next_public_handle: AtomicU64::new(next_public_handle),
                next_access: AtomicU64::new(1),
                admitted: AtomicUsize::new(0),
                poison_slots: AtomicUsize::new(0),
                process,
                #[cfg(test)]
                before_drain_wait: Mutex::new(None),
            }),
        }
    }
    pub fn runtime_nonce(&self) -> Result<u64, RegistryError> {
        self.inner.runtime_nonce.clone()
    }
    fn live_incarnation(&self, pid: i32) -> Result<u64, RegistryError> {
        self.inner.process.incarnation(pid)
    }
    pub fn lane_key_for_current_runtime(
        &self,
        pid: i32,
        window_id: u64,
    ) -> Result<LaneKey, RegistryError> {
        let process_incarnation = self.live_incarnation(pid)?; // I/O before every lock.
        Ok(LaneKey {
            runtime_owner: current_runtime_scope(),
            pid,
            process_incarnation,
            window_id,
        })
    }
    pub fn prepare_current<P: Any + Send + Sync>(
        &self,
        pid: i32,
        window_id: u64,
        element_count: usize,
        payload: Arc<P>,
    ) -> Result<SnapshotCandidate, RegistryError> {
        let key = self.lane_key_for_current_runtime(pid, window_id)?;
        self.prepare(key, element_count, payload)
    }
    pub fn prepare<P: Any + Send + Sync>(
        &self,
        key: LaneKey,
        element_count: usize,
        payload: Arc<P>,
    ) -> Result<SnapshotCandidate, RegistryError> {
        let nonce = self.runtime_nonce()?;
        let identity = SnapshotIdentity {
            runtime_nonce: nonce,
            sequence: take_checked(&self.inner.next_sequence)
                .ok_or(RegistryError::GenerationExhausted)?,
        };
        let public64 = take_checked(&self.inner.next_public_handle)
            .ok_or(RegistryError::PublicHandleExhausted)?;
        let public = u32::try_from(public64).map_err(|_| RegistryError::PublicHandleExhausted)?;
        if public == 0 {
            return Err(RegistryError::PublicHandleExhausted);
        }
        self.cleanup_replaced_process(&key);

        let mut deferred = Vec::new();
        let (lane, ticket) = {
            let mut state = self.inner.state.lock().unwrap();
            if state.candidates >= self.inner.capacities.candidate_builders {
                return Err(RegistryError::CapacityExhausted {
                    resource: "candidate_builders",
                });
            }
            if state.payload_records >= self.inner.capacities.payload_records {
                return Err(RegistryError::CapacityExhausted {
                    resource: "payload_records",
                });
            }
            if state.generations + state.candidates >= self.inner.capacities.generations {
                return Err(RegistryError::CapacityExhausted {
                    resource: "generations",
                });
            }
            if !state.lanes.contains_key(&key) {
                reclaim_for_new_lane(&self.inner, &mut state, &key, &mut deferred)?;
                if state.lanes.len() >= self.inner.capacities.lanes {
                    return Err(RegistryError::CapacityExhausted { resource: "lanes" });
                }
                let tick = take_checked(&self.inner.next_access).unwrap_or(u64::MAX);
                state.lanes.insert(
                    key.clone(),
                    Arc::new(Lane {
                        inner: Mutex::new(LaneInner {
                            current: None,
                            state: GenerationState::Open,
                            admitted: 0,
                            candidates: 0,
                            latest_ticket: 0,
                            abandoned_ticket: None,
                            validating_ticket: None,
                            poison_reserved: false,
                            access_tick: tick,
                        }),
                        drained: Condvar::new(),
                    }),
                );
            }
            let lane = state.lanes.get(&key).unwrap().clone();
            let binding_key = (key.runtime_owner.clone(), key.pid, public);
            if state.bindings.contains_key(&binding_key) {
                return Err(RegistryError::Collision);
            }
            let mut li = lane.inner.lock().unwrap();
            li.latest_ticket = li
                .latest_ticket
                .checked_add(1)
                .ok_or(RegistryError::GenerationExhausted)?;
            li.candidates += 1;
            let ticket = li.latest_ticket;
            drop(li);
            state.bindings.insert(
                binding_key,
                TokenBinding {
                    lane: Arc::downgrade(&lane),
                    lane_key: key.clone(),
                    identity,
                },
            );
            state.candidates += 1;
            state.payload_records += 1;
            (lane, ticket)
        };
        drop(deferred);
        let generation = Arc::new(SnapshotGeneration {
            identity,
            lane: key,
            public_handle: public,
            max_element_index: element_count.checked_sub(1),
            payload,
        });
        Ok(SnapshotCandidate {
            registry: self.inner.clone(),
            lane,
            generation: Some(generation),
            ticket,
            registered: true,
        })
    }

    pub fn publish(
        &self,
        mut candidate: SnapshotCandidate,
        timeout: Duration,
    ) -> Result<u32, RegistryError> {
        let generation = candidate.generation.as_ref().unwrap().clone();
        let live = self.live_incarnation(generation.lane.pid)?; // I/O before locks.
        if live != generation.lane.process_incarnation {
            return Err(RegistryError::NotCurrent);
        }
        {
            let state = self.inner.state.lock().unwrap();
            if !state
                .lanes
                .get(&generation.lane)
                .is_some_and(|l| Arc::ptr_eq(l, &candidate.lane))
            {
                return Err(RegistryError::NotCurrent);
            }
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let lane = candidate.lane.clone();
        let replaced = {
            let mut inner = lane.inner.lock().unwrap();
            if inner
                .validating_ticket
                .is_some_and(|ticket| ticket != candidate.ticket)
            {
                return Err(RegistryError::Closing);
            }
            if candidate.ticket != inner.latest_ticket || inner.candidates == 0 {
                return Err(RegistryError::Superseded);
            }
            match &inner.state {
                GenerationState::Poisoned { reason } => {
                    return Err(RegistryError::Poisoned(reason.clone()))
                }
                GenerationState::Stale => return Err(RegistryError::Stale),
                GenerationState::Open if inner.current.is_some() => {
                    inner.state = GenerationState::Closing {
                        ticket: candidate.ticket,
                    };
                    inner.abandoned_ticket = None;
                }
                GenerationState::Closing { ticket } if *ticket != candidate.ticket => {
                    inner.state = GenerationState::Closing {
                        ticket: candidate.ticket,
                    };
                    inner.abandoned_ticket = None;
                    lane.drained.notify_all();
                }
                _ => {}
            }
            while inner.admitted != 0 {
                if !matches!(inner.state, GenerationState::Closing { ticket } if ticket == candidate.ticket)
                    || candidate.ticket != inner.latest_ticket
                {
                    return Err(RegistryError::Superseded);
                }
                let now = Instant::now();
                if now >= deadline {
                    if matches!(inner.state, GenerationState::Closing { ticket } if ticket == candidate.ticket)
                    {
                        inner.abandoned_ticket = Some(candidate.ticket);
                    }
                    return Err(RegistryError::PublicationTimeout);
                }
                #[cfg(test)]
                if let Some(hook) = self.inner.before_drain_wait.lock().unwrap().clone() {
                    hook();
                }
                let (next, result) = lane.drained.wait_timeout(inner, deadline - now).unwrap();
                inner = next;
                if result.timed_out() && inner.admitted != 0 {
                    if matches!(inner.state, GenerationState::Closing { ticket } if ticket == candidate.ticket)
                        && candidate.ticket == inner.latest_ticket
                    {
                        inner.abandoned_ticket = Some(candidate.ticket);
                        return Err(RegistryError::PublicationTimeout);
                    }
                    return Err(RegistryError::Superseded);
                }
            }
            if candidate.ticket != inner.latest_ticket {
                return Err(RegistryError::Superseded);
            }
            match &inner.state {
                GenerationState::Poisoned { reason } => {
                    return Err(RegistryError::Poisoned(reason.clone()))
                }
                GenerationState::Stale => return Err(RegistryError::Stale),
                _ => {}
            }
            let replaced = inner.current.replace(generation.clone());
            // The pointer is visible, but publication does not linearize until
            // the lock-free process-incarnation post-check succeeds.
            inner.state = GenerationState::Closing {
                ticket: candidate.ticket,
            };
            inner.validating_ticket = Some(candidate.ticket);
            inner.abandoned_ticket = None;
            inner.access_tick = take_checked(&self.inner.next_access).unwrap_or(u64::MAX);
            lane.drained.notify_all();
            replaced
        };
        let post_incarnation = self.live_incarnation(generation.lane.pid);
        if post_incarnation.as_ref().ok() != Some(&generation.lane.process_incarnation) {
            invalidate_swapped_candidate(&mut candidate, &generation, replaced);
            return match post_incarnation {
                Ok(_) => Err(RegistryError::NotCurrent),
                Err(error) => Err(error),
            };
        }
        {
            let mut state = self.inner.state.lock().unwrap();
            if !state
                .lanes
                .get(&generation.lane)
                .is_some_and(|l| Arc::ptr_eq(l, &lane))
            {
                // Candidate count prevents this; fail closed if internal invariants are violated.
                return Err(RegistryError::NotCurrent);
            }
            state.candidates -= 1;
            {
                let mut lane_state = lane.inner.lock().unwrap();
                debug_assert_eq!(
                    lane_state.current.as_ref().map(|g| g.identity),
                    Some(generation.identity)
                );
                debug_assert_eq!(lane_state.validating_ticket, Some(candidate.ticket));
                debug_assert!(lane_state.candidates > 0);
                lane_state.candidates -= 1;
                lane_state.validating_ticket = None;
                lane_state.state = GenerationState::Open;
                lane.drained.notify_all();
            }
            if let Some(old) = replaced.as_ref() {
                state.bindings.remove(&(
                    old.lane.runtime_owner.clone(),
                    old.lane.pid,
                    old.public_handle,
                ));
                state.payload_records -= 1;
            } else {
                state.generations += 1;
            }
        }
        candidate.registered = false;
        let handle = generation.public_handle;
        candidate.generation.take();
        drop(replaced); // arbitrary payload drop after every guard.
        Ok(handle)
    }

    pub fn register_snapshot<W: IntoWindowId>(
        &self,
        pid: i32,
        window_id: W,
        element_count: usize,
    ) -> Result<u32, RegistryError> {
        self.try_register_snapshot(pid, window_id.into_window_id(), element_count)
    }
    pub fn try_register_snapshot(
        &self,
        pid: i32,
        window_id: u64,
        element_count: usize,
    ) -> Result<u32, RegistryError> {
        let candidate = self.prepare_current(pid, window_id, element_count, Arc::new(()))?;
        self.publish(candidate, Duration::ZERO)
    }

    pub fn resolve_generation(
        &self,
        pid: i32,
        token: &str,
    ) -> Result<(Arc<SnapshotGeneration>, usize), String> {
        let (public, index) =
            parse_token(token).ok_or_else(|| "element_token has invalid format".to_owned())?;
        let before = self
            .live_incarnation(pid)
            .map_err(|_| STALE_TOKEN_ERROR.to_owned())?;
        let runtime = current_runtime_scope();
        let state = self.inner.state.lock().unwrap();
        let Some(binding) = state.bindings.get(&(runtime.clone(), pid, public)) else {
            return Err(
                if state.bindings.keys().any(|(scope, bound_pid, handle)| {
                    scope != &runtime && *bound_pid == pid && *handle == public
                }) {
                    "element_token belongs to another runtime generation".to_owned()
                } else {
                    STALE_TOKEN_ERROR.to_owned()
                },
            );
        };
        if binding.lane_key.process_incarnation != before {
            return Err(STALE_TOKEN_ERROR.to_owned());
        }
        let lane = binding
            .lane
            .upgrade()
            .ok_or_else(|| STALE_TOKEN_ERROR.to_owned())?;
        if !state
            .lanes
            .get(&binding.lane_key)
            .is_some_and(|registered| Arc::ptr_eq(registered, &lane))
        {
            return Err(STALE_TOKEN_ERROR.to_owned());
        }
        let inner = lane.inner.lock().unwrap();
        let generation = inner
            .current
            .as_ref()
            .filter(|g| g.identity == binding.identity)
            .cloned()
            .ok_or_else(|| STALE_TOKEN_ERROR.to_owned())?;
        if generation
            .max_element_index
            .is_none_or(|maximum| index > maximum)
        {
            return Err(format!(
                "element_token element_index {index} out of range (snapshot had {} elements)",
                generation.element_count()
            ));
        }
        drop(inner);
        drop(state);
        let after = self
            .live_incarnation(pid)
            .map_err(|_| STALE_TOKEN_ERROR.to_owned())?;
        if before != generation.lane.process_incarnation || after != before {
            return Err(STALE_TOKEN_ERROR.to_owned());
        }
        // Successful post-selection validation is the resolver linearization point.
        Ok((generation, index))
    }
    pub fn resolve(&self, pid: i32, token: &str) -> Result<(u64, usize), String> {
        let (generation, index) = self.resolve_generation(pid, token)?;
        Ok((generation.lane.window_id, index))
    }
    pub fn current_payload<T: Any + Send + Sync>(
        &self,
        pid: i32,
        window_id: u64,
    ) -> Option<Arc<T>> {
        let before = self.live_incarnation(pid).ok()?;
        let key = LaneKey {
            runtime_owner: current_runtime_scope(),
            pid,
            process_incarnation: before,
            window_id,
        };
        let state = self.inner.state.lock().unwrap();
        let lane = state.lanes.get(&key)?.clone();
        let inner = lane.inner.lock().unwrap();
        let generation = inner.current.clone()?;
        drop(inner);
        drop(state);
        let after = self.live_incarnation(pid).ok()?;
        if before != generation.lane.process_incarnation || after != before {
            return None;
        }
        generation.payload::<T>()
    }
    pub fn try_acquire_mutation(
        &self,
        identity: SnapshotIdentity,
    ) -> Result<MutationPermit, RegistryError> {
        let state = self.inner.state.lock().unwrap();
        let lane = state
            .lanes
            .values()
            .find(|lane| {
                lane.inner
                    .lock()
                    .unwrap()
                    .current
                    .as_ref()
                    .is_some_and(|g| g.identity == identity)
            })
            .cloned()
            .ok_or(RegistryError::NotCurrent)?;
        let mut inner = lane.inner.lock().unwrap();
        if inner.current.as_ref().map(|generation| generation.identity) != Some(identity) {
            return Err(RegistryError::NotCurrent);
        }
        match &inner.state {
            GenerationState::Open => {}
            GenerationState::Closing { .. } => return Err(RegistryError::Closing),
            GenerationState::Stale => return Err(RegistryError::Stale),
            GenerationState::Poisoned { reason } => {
                return Err(RegistryError::Poisoned(reason.clone()))
            }
        }
        self.inner
            .admitted
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < self.inner.capacities.admitted_mutations).then_some(count + 1)
            })
            .map_err(|_| RegistryError::CapacityExhausted {
                resource: "admitted_mutations",
            })?;
        if !inner.poison_reserved {
            if self
                .inner
                .poison_slots
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    (count < self.inner.capacities.poison_records).then_some(count + 1)
                })
                .is_err()
            {
                self.inner.admitted.fetch_sub(1, Ordering::AcqRel);
                return Err(RegistryError::CapacityExhausted {
                    resource: "poison_records",
                });
            }
            inner.poison_reserved = true;
        }
        inner.admitted += 1;
        let generation = inner.current.as_ref().unwrap().clone();
        drop(inner);
        drop(state);
        Ok(MutationPermit {
            registry: self.inner.clone(),
            lane,
            generation,
            released: false,
        })
    }
    pub fn state(&self, key: &LaneKey) -> Option<GenerationState> {
        let state = self.inner.state.lock().unwrap();
        let lane = state.lanes.get(key)?.clone();
        let lane_state = lane.inner.lock().unwrap();
        let value = lane_state.state.clone();
        drop(lane_state);
        drop(state);
        Some(value)
    }
    pub fn mark_stale(&self, key: &LaneKey) -> bool {
        let state = self.inner.state.lock().unwrap();
        let Some(lane) = state.lanes.get(key).cloned() else {
            return false;
        };
        let mut inner = lane.inner.lock().unwrap();
        inner.state = GenerationState::Stale;
        lane.drained.notify_all();
        drop(inner);
        drop(state);
        true
    }
    pub fn clear_runtime_scope(&self, runtime_scope: &str) -> usize {
        self.cleanup_where(|key| key.runtime_owner == runtime_scope)
    }
    pub fn clear_exact_owner_process(
        &self,
        runtime_owner: &str,
        pid: i32,
        process_incarnation: u64,
    ) -> usize {
        self.cleanup_where(|key| {
            key.runtime_owner == runtime_owner
                && key.pid == pid
                && key.process_incarnation == process_incarnation
        })
    }
    fn cleanup_replaced_process(&self, incoming: &LaneKey) {
        self.cleanup_where(|key| {
            key.runtime_owner == incoming.runtime_owner
                && key.pid == incoming.pid
                && key.process_incarnation != incoming.process_incarnation
        });
    }
    fn cleanup_where(&self, predicate: impl Fn(&LaneKey) -> bool) -> usize {
        let mut deferred = Vec::new();
        let removed = {
            let mut state = self.inner.state.lock().unwrap();
            let keys: Vec<_> = state
                .lanes
                .keys()
                .filter(|k| predicate(k))
                .cloned()
                .collect();
            let mut removed = 0;
            for key in keys {
                let Some(lane) = state.lanes.get(&key).cloned() else {
                    continue;
                };
                let safe = {
                    let li = lane.inner.lock().unwrap();
                    li.admitted == 0
                        && li.candidates == 0
                        && !matches!(li.state, GenerationState::Closing { .. })
                };
                if safe && remove_lane_locked(&self.inner, &mut state, &key, &mut deferred) {
                    removed += 1;
                }
            }
            removed
        };
        drop(deferred);
        removed
    }
    #[cfg(test)]
    fn counts(&self) -> (usize, usize, usize, usize, usize) {
        let state = self.inner.state.lock().unwrap();
        (
            state.lanes.len(),
            state.generations,
            state.candidates,
            state.payload_records,
            self.inner.poison_slots.load(Ordering::Acquire),
        )
    }
    #[cfg(test)]
    fn set_before_drain_wait(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.inner.before_drain_wait.lock().unwrap() = hook;
    }
}

fn remove_lane_locked(
    inner: &RegistryInner,
    state: &mut RegistryState,
    key: &LaneKey,
    deferred: &mut Vec<Arc<Lane>>,
) -> bool {
    let Some(lane) = state.lanes.remove(key) else {
        return false;
    };
    let (has_current, reserved) = {
        let li = lane.inner.lock().unwrap();
        (li.current.is_some(), li.poison_reserved)
    };
    state.bindings.retain(|_, b| b.lane_key != *key);
    if has_current {
        state.generations -= 1;
        state.payload_records -= 1;
    }
    if reserved {
        inner.poison_slots.fetch_sub(1, Ordering::AcqRel);
    }
    deferred.push(lane);
    true
}
fn reclaim_for_new_lane(
    inner: &RegistryInner,
    state: &mut RegistryState,
    incoming: &LaneKey,
    deferred: &mut Vec<Arc<Lane>>,
) -> Result<(), RegistryError> {
    loop {
        let same_pid = state
            .lanes
            .keys()
            .filter(|key| {
                key.runtime_owner == incoming.runtime_owner
                    && key.pid == incoming.pid
                    && key.process_incarnation == incoming.process_incarnation
            })
            .count();
        let per_pid_pressure = same_pid >= LRU_CAP_PER_PID;
        let global_pressure = state.lanes.len() >= inner.capacities.lanes;
        if !per_pid_pressure && !global_pressure {
            return Ok(());
        }
        let victim = state
            .lanes
            .iter()
            .filter_map(|(key, lane)| {
                let same_pid = key.runtime_owner == incoming.runtime_owner
                    && key.pid == incoming.pid
                    && key.process_incarnation == incoming.process_incarnation;
                if per_pid_pressure && !same_pid {
                    return None;
                }
                let li = lane.inner.lock().unwrap();
                let safe = li.admitted == 0
                    && li.candidates == 0
                    && !li.poison_reserved
                    && matches!(li.state, GenerationState::Open | GenerationState::Stale);
                safe.then_some((!same_pid, li.access_tick, key.clone()))
            })
            .min_by_key(|victim| (victim.0, victim.1))
            .map(|victim| victim.2);
        let Some(victim) = victim else {
            return if per_pid_pressure {
                Err(RegistryError::CapacityExhausted {
                    resource: "lanes_per_pid",
                })
            } else {
                Ok(())
            };
        };
        let removed = remove_lane_locked(inner, state, &victim, deferred);
        debug_assert!(removed, "selected LRU victim must remain present");
    }
}
fn invalidate_swapped_candidate(
    candidate: &mut SnapshotCandidate,
    generation: &Arc<SnapshotGeneration>,
    replaced: Option<Arc<SnapshotGeneration>>,
) {
    {
        let mut state = candidate.registry.state.lock().unwrap();
        state.bindings.remove(&(
            generation.lane.runtime_owner.clone(),
            generation.lane.pid,
            generation.public_handle,
        ));
        if let Some(old) = replaced.as_ref() {
            state.bindings.remove(&(
                old.lane.runtime_owner.clone(),
                old.lane.pid,
                old.public_handle,
            ));
        }
        state.candidates = state.candidates.saturating_sub(1);
        state.payload_records = state.payload_records.saturating_sub(1);
        if replaced.is_some() {
            state.generations = state.generations.saturating_sub(1);
            state.payload_records = state.payload_records.saturating_sub(1);
        }
        if state
            .lanes
            .get(&generation.lane)
            .is_some_and(|lane| Arc::ptr_eq(lane, &candidate.lane))
        {
            let mut inner = candidate.lane.inner.lock().unwrap();
            if inner.current.as_ref().map(|g| g.identity) == Some(generation.identity)
                && inner.validating_ticket == Some(candidate.ticket)
            {
                inner.current = None;
                inner.state = GenerationState::Stale;
                inner.validating_ticket = None;
                inner.abandoned_ticket = None;
            }
            inner.candidates = inner.candidates.saturating_sub(1);
            candidate.lane.drained.notify_all();
        }
    }
    candidate.registered = false;
    candidate.generation.take();
    drop(replaced);
}
fn cancel_candidate(candidate: &mut SnapshotCandidate) {
    let generation = candidate.generation.take().unwrap();
    {
        let mut state = candidate.registry.state.lock().unwrap();
        state.bindings.remove(&(
            generation.lane.runtime_owner.clone(),
            generation.lane.pid,
            generation.public_handle,
        ));
        state.candidates = state.candidates.saturating_sub(1);
        state.payload_records = state.payload_records.saturating_sub(1);
        if state
            .lanes
            .get(&generation.lane)
            .is_some_and(|l| Arc::ptr_eq(l, &candidate.lane))
        {
            let mut li = candidate.lane.inner.lock().unwrap();
            li.candidates = li.candidates.saturating_sub(1);
            if matches!(li.state, GenerationState::Closing { ticket } if ticket == candidate.ticket)
            {
                if li.admitted == 0 {
                    li.state = GenerationState::Open;
                    li.abandoned_ticket = None;
                } else {
                    li.abandoned_ticket = Some(candidate.ticket);
                }
                candidate.lane.drained.notify_all();
            }
        }
    }
    candidate.registered = false;
    drop(generation); // payload after global/lane guards.
}
fn claimed_runtime_nonces() -> &'static Mutex<HashSet<u64>> {
    static CLAIMED: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
    CLAIMED.get_or_init(|| Mutex::new(HashSet::new()))
}
fn claim_runtime_nonce(nonce: u64) -> Result<RuntimeNonceClaim, RegistryError> {
    if nonce == 0 {
        return Err(RegistryError::EntropyUnavailable);
    }
    let mut claimed = claimed_runtime_nonces().lock().unwrap();
    if claimed.insert(nonce) {
        Ok(RuntimeNonceClaim { nonce })
    } else {
        Err(RegistryError::Collision)
    }
}
#[cfg(test)]
fn active_runtime_nonce_claims() -> usize {
    claimed_runtime_nonces().lock().unwrap().len()
}
#[cfg(test)]
fn runtime_nonce_is_claimed(nonce: u64) -> bool {
    claimed_runtime_nonces().lock().unwrap().contains(&nonce)
}
fn random_nonce() -> Result<u64, RegistryError> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).map_err(|_| RegistryError::EntropyUnavailable)?;
    let nonce = u64::from_ne_bytes(bytes);
    (nonce != 0)
        .then_some(nonce)
        .ok_or(RegistryError::EntropyUnavailable)
}
fn take_checked(counter: &AtomicU64) -> Option<u64> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_add(1))
        .ok()
}
pub fn process_incarnation(pid: i32) -> Result<u64, RegistryError> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_err(|_| RegistryError::ProcessIncarnationUnavailable(pid))?;
        let tail = stat
            .rsplit_once(')')
            .map(|(_, t)| t)
            .ok_or(RegistryError::ProcessIncarnationUnavailable(pid))?;
        tail.split_whitespace()
            .nth(19)
            .and_then(|v| v.parse().ok())
            .ok_or(RegistryError::ProcessIncarnationUnavailable(pid))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        Ok(0)
    }
}
fn current_runtime_scope() -> String {
    crate::tool::current_dispatch_runtime_scope().unwrap_or_else(|| "legacy".to_owned())
}
impl Default for TokenRegistry {
    fn default() -> Self {
        Self::new()
    }
}
pub fn global() -> &'static TokenRegistry {
    static REGISTRY: OnceLock<TokenRegistry> = OnceLock::new();
    REGISTRY.get_or_init(TokenRegistry::new)
}
pub fn format_token(snapshot_id: u32, element_index: usize) -> String {
    format!("s{snapshot_id:08x}:{element_index}")
}
pub fn token_for(snapshot_id: u32, element_index: usize) -> String {
    format_token(snapshot_id, element_index)
}
fn parse_token(token: &str) -> Option<(u32, usize)> {
    let (hex, index) = token.strip_prefix('s')?.split_once(':')?;
    (hex.len() == 8).then_some(())?;
    Some((u32::from_str_radix(hex, 16).ok()?, index.parse().ok()?))
}
fn parse_snapshot_handle(snapshot_id: &str) -> Option<u32> {
    let hex = snapshot_id.strip_prefix('s')?;
    (hex.len() == 8 && !hex.contains(':')).then_some(())?;
    u32::from_str_radix(hex, 16).ok()
}

pub fn checked_native_window_id(
    window_id: u64,
    tool_name: &str,
) -> Result<u32, crate::protocol::ToolResult> {
    u32::try_from(window_id).map_err(|_| {
        let message = format!(
            "{tool_name}: window_id {window_id} exceeds the native macOS u32 identifier range"
        );
        crate::protocol::ToolResult::error(message.clone()).with_structured(serde_json::json!({
            "status": "refused",
            "refusal": { "code": "window_id_overflow", "message": message }
        }))
    })
}

pub fn checked_optional_native_window_id(
    window_id: Option<u64>,
    tool_name: &str,
) -> Result<Option<u32>, crate::protocol::ToolResult> {
    window_id
        .map(|value| checked_native_window_id(value, tool_name))
        .transpose()
}

#[derive(Debug, Clone)]
pub enum ResolvedElement {
    None,
    Element {
        window_id: Option<u64>,
        element_index: usize,
        snapshot_identity: SnapshotIdentity,
        via_token: bool,
    },
}
pub fn resolve_element_args(
    pid: i32,
    args_element_index: Option<usize>,
    args_element_token: Option<&str>,
    args_snapshot_id: Option<&str>,
    args_window_id: Option<u64>,
    tool_name: &str,
) -> Result<ResolvedElement, crate::protocol::ToolResult> {
    let refusal = |code: &str, message: String| {
        crate::protocol::ToolResult::error(message.clone()).with_structured(
            serde_json::json!({"status":"refused","refusal":{"code":code,"message":message}}),
        )
    };
    let resolve_token = |token: &str| {
        let (generation, index) = global().resolve_generation(pid, token).map_err(|message| {
            let code = if message.contains("another runtime generation") {
                "generation_mismatch"
            } else if message == STALE_TOKEN_ERROR {
                "stale_element_token"
            } else {
                "invalid_element_token"
            };
            refusal(code, message)
        })?;
        Ok::<_, crate::protocol::ToolResult>((generation, index))
    };
    match (args_element_index, args_element_token, args_snapshot_id) {
        (None, None, None) => Ok(ResolvedElement::None),
        (None, None, Some(_)) => Err(refusal("element_index_required",
            format!("{tool_name}: snapshot_id requires element_index"))),
        (Some(_), None, None) => Err(refusal("snapshot_id_required",
            format!("{tool_name}: bare element_index is not accepted; pass element_token, or snapshot_id together with element_index"))),
        (Some(index), None, Some(handle)) => {
            let public = parse_snapshot_handle(handle).ok_or_else(|| refusal("invalid_snapshot_id",
                format!("{tool_name}: snapshot_id has invalid format")))?;
            let (generation, resolved_index) = resolve_token(&format_token(public, index))?;
            if args_window_id.is_some_and(|w| w != generation.lane.window_id) {
                return Err(refusal("conflicting_element_target", format!(
                    "{tool_name}: snapshot belongs to window_id {}, not {}",
                    generation.lane.window_id, args_window_id.unwrap())));
            }
            Ok(ResolvedElement::Element { window_id: Some(generation.lane.window_id),
                element_index: resolved_index, snapshot_identity: generation.identity, via_token: false })
        }
        (index_arg, Some(token), handle_arg) => {
            let (generation, index) = resolve_token(token)?;
            let public = parse_token(token).map(|(p, _)| p);
            if index_arg.is_some_and(|arg| arg != index)
                || args_window_id.is_some_and(|arg| arg != generation.lane.window_id)
                || handle_arg.is_some_and(|handle| parse_snapshot_handle(handle) != public) {
                return Err(refusal("conflicting_element_target", format!(
                    "{tool_name}: element_token conflicts with element_index, snapshot_id, or window_id")));
            }
            Ok(ResolvedElement::Element { window_id: Some(generation.lane.window_id),
                element_index: index, snapshot_identity: generation.identity, via_token: true })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{mpsc, Barrier};
    use std::thread;

    struct FakeProcess {
        values: Mutex<HashMap<i32, u64>>,
    }
    impl FakeProcess {
        fn new(pid: i32, incarnation: u64) -> Arc<Self> {
            Arc::new(Self {
                values: Mutex::new(HashMap::from([(pid, incarnation)])),
            })
        }
        fn set(&self, pid: i32, incarnation: u64) {
            self.values.lock().unwrap().insert(pid, incarnation);
        }
    }
    impl ProcessIncarnationProvider for FakeProcess {
        fn incarnation(&self, pid: i32) -> Result<u64, RegistryError> {
            self.values
                .lock()
                .unwrap()
                .get(&pid)
                .copied()
                .ok_or(RegistryError::ProcessIncarnationUnavailable(pid))
        }
    }
    struct GatedProcess {
        values: Mutex<HashMap<i32, u64>>,
        calls: AtomicUsize,
        gate: Mutex<Option<(usize, Arc<Barrier>, Arc<Barrier>)>>,
    }
    impl GatedProcess {
        fn new(pid: i32, incarnation: u64) -> Arc<Self> {
            Arc::new(Self {
                values: Mutex::new(HashMap::from([(pid, incarnation)])),
                calls: AtomicUsize::new(0),
                gate: Mutex::new(None),
            })
        }
        fn set(&self, pid: i32, incarnation: u64) {
            self.values.lock().unwrap().insert(pid, incarnation);
        }
        fn arm_after(&self, calls_from_now: usize) -> (Arc<Barrier>, Arc<Barrier>) {
            let entered = Arc::new(Barrier::new(2));
            let release = Arc::new(Barrier::new(2));
            let target = self.calls.load(Ordering::Acquire) + calls_from_now;
            *self.gate.lock().unwrap() = Some((target, entered.clone(), release.clone()));
            (entered, release)
        }
    }
    impl ProcessIncarnationProvider for GatedProcess {
        fn incarnation(&self, pid: i32) -> Result<u64, RegistryError> {
            let call = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
            let gate = self.gate.lock().unwrap().clone();
            if let Some((_, entered, release)) = gate.filter(|gate| gate.0 == call) {
                entered.wait();
                release.wait();
                self.gate.lock().unwrap().take();
            }
            self.values
                .lock()
                .unwrap()
                .get(&pid)
                .copied()
                .ok_or(RegistryError::ProcessIncarnationUnavailable(pid))
        }
    }
    static NEXT_TEST_NONCE: AtomicU64 = AtomicU64::new(0x1000_0000_0000_0000);
    fn registry(
        caps: RegistryCapacities,
        process: Arc<dyn ProcessIncarnationProvider>,
    ) -> TokenRegistry {
        TokenRegistry::with_parts(
            caps,
            Ok(NEXT_TEST_NONCE.fetch_add(1, Ordering::AcqRel)),
            1,
            1,
            process,
        )
    }
    fn key(window: u64, incarnation: u64) -> LaneKey {
        LaneKey {
            runtime_owner: "legacy".into(),
            pid: 7,
            process_incarnation: incarnation,
            window_id: window,
        }
    }
    fn publish_value(r: &TokenRegistry, k: LaneKey, value: u64) -> Arc<SnapshotGeneration> {
        let c = r.prepare(k, 3, Arc::new(value)).unwrap();
        let h = r.publish(c, Duration::ZERO).unwrap();
        r.resolve_generation(7, &format_token(h, 0)).unwrap().0
    }

    #[test]
    fn native_window_id_narrowing_is_checked() {
        assert_eq!(
            checked_native_window_id(u32::MAX as u64, "test").unwrap(),
            u32::MAX
        );
        let error = checked_native_window_id(u64::from(u32::MAX) + 1, "test").unwrap_err();
        assert_eq!(
            error.structured_content.unwrap()["refusal"]["code"],
            "window_id_overflow"
        );
    }

    #[test]
    fn public_wire_pattern_is_unchanged() {
        assert_eq!(format_token(0x1234, 42), "s00001234:42");
        assert_eq!(parse_token("s00001234:42"), Some((0x1234, 42)));
    }
    #[test]
    fn public_resolver_keeps_full_width_window_and_invalidates_replaced_token() {
        let p = FakeProcess::new(7, 1);
        let r = registry(RegistryCapacities::default(), p);
        let window = u64::from(u32::MAX) + 17;
        let first = publish_value(&r, key(window, 1), 1);
        let old_token = format_token(first.public_handle(), 0);
        let second_candidate = r.prepare(key(window, 1), 1, Arc::new(2_u64)).unwrap();
        let second_handle = r.publish(second_candidate, Duration::ZERO).unwrap();
        assert_eq!(
            r.resolve_generation(7, &old_token).unwrap_err(),
            STALE_TOKEN_ERROR
        );
        assert_eq!(
            r.resolve(7, &format_token(second_handle, 0)).unwrap(),
            (window, 0)
        );
    }

    #[test]
    fn prepare_pid_reuse_publish_refuses() {
        let p = FakeProcess::new(7, 1);
        let r = registry(RegistryCapacities::default(), p.clone());
        let c = r.prepare_current(7, 11, 1, Arc::new(())).unwrap();
        p.set(7, 2);
        assert_eq!(r.publish(c, Duration::ZERO), Err(RegistryError::NotCurrent));
        assert_eq!(r.counts().2, 0);
    }
    #[test]
    fn publish_pid_reuse_resolve_refuses() {
        let p = FakeProcess::new(7, 1);
        let r = registry(RegistryCapacities::default(), p.clone());
        let c = r.prepare_current(7, 12, 1, Arc::new(())).unwrap();
        let h = r.publish(c, Duration::ZERO).unwrap();
        p.set(7, 2);
        assert_eq!(
            r.resolve_generation(7, &format_token(h, 0)).unwrap_err(),
            STALE_TOKEN_ERROR
        );
    }
    #[test]
    fn publish_rechecks_incarnation_after_permit_drain() {
        let p = FakeProcess::new(7, 1);
        let r = Arc::new(registry(RegistryCapacities::default(), p.clone()));
        let current = publish_value(&r, key(30, 1), 1);
        let permit = r.try_acquire_mutation(current.identity).unwrap();
        let candidate = r.prepare_current(7, 30, 1, Arc::new(2_u64)).unwrap();
        let handle = candidate.public_handle();
        let waiting = Arc::new(Barrier::new(2));
        let hook = waiting.clone();
        r.set_before_drain_wait(Some(Arc::new(move || {
            hook.wait();
        })));
        let rr = r.clone();
        let publisher = thread::spawn(move || rr.publish(candidate, Duration::from_secs(5)));
        waiting.wait();
        r.set_before_drain_wait(None);
        p.set(7, 2);
        drop(permit);
        assert_eq!(publisher.join().unwrap(), Err(RegistryError::NotCurrent));
        assert_eq!(
            r.resolve_generation(7, &format_token(handle, 0))
                .unwrap_err(),
            STALE_TOKEN_ERROR
        );
        assert_eq!(r.counts().2, 0);
    }
    #[test]
    fn publish_invalidates_swap_when_postcheck_observes_pid_reuse() {
        let p = GatedProcess::new(7, 1);
        let r = Arc::new(registry(RegistryCapacities::default(), p.clone()));
        let candidate = r.prepare_current(7, 31, 1, Arc::new(())).unwrap();
        let handle = candidate.public_handle();
        let (entered, release) = p.arm_after(2);
        let rr = r.clone();
        let publisher = thread::spawn(move || rr.publish(candidate, Duration::ZERO));
        entered.wait();
        p.set(7, 2);
        assert_eq!(
            r.resolve_generation(7, &format_token(handle, 0))
                .unwrap_err(),
            STALE_TOKEN_ERROR
        );
        release.wait();
        assert_eq!(publisher.join().unwrap(), Err(RegistryError::NotCurrent));
        assert_eq!(
            r.resolve_generation(7, &format_token(handle, 0))
                .unwrap_err(),
            STALE_TOKEN_ERROR
        );
        assert_eq!(r.counts().2, 0);
    }
    #[test]
    fn resolve_rechecks_incarnation_after_exact_generation_selection() {
        let p = GatedProcess::new(7, 1);
        let r = Arc::new(registry(RegistryCapacities::default(), p.clone()));
        let candidate = r.prepare_current(7, 32, 1, Arc::new(())).unwrap();
        let handle = r.publish(candidate, Duration::ZERO).unwrap();
        let (entered, release) = p.arm_after(2);
        let rr = r.clone();
        let resolver = thread::spawn(move || rr.resolve_generation(7, &format_token(handle, 0)));
        entered.wait();
        p.set(7, 2);
        release.wait();
        assert_eq!(resolver.join().unwrap().unwrap_err(), STALE_TOKEN_ERROR);
    }
    #[test]
    fn detached_lane_and_cleanup_with_candidate_refuse_removal() {
        let p = FakeProcess::new(7, 1);
        let r = registry(RegistryCapacities::default(), p);
        let c = r.prepare(key(13, 1), 1, Arc::new(())).unwrap();
        assert_eq!(r.clear_exact_owner_process("legacy", 7, 1), 0);
        r.inner.state.lock().unwrap().lanes.remove(&key(13, 1));
        assert_eq!(r.publish(c, Duration::ZERO), Err(RegistryError::NotCurrent));
    }
    #[test]
    fn process_replacement_cannot_remove_live_candidate() {
        let p = FakeProcess::new(7, 1);
        let r = registry(RegistryCapacities::default(), p.clone());
        let old = r.prepare(key(14, 1), 1, Arc::new(())).unwrap();
        p.set(7, 2);
        let new = r.prepare(key(15, 2), 1, Arc::new(())).unwrap();
        assert!(r
            .inner
            .state
            .lock()
            .unwrap()
            .lanes
            .contains_key(&key(14, 1)));
        drop(old);
        drop(new);
    }
    #[test]
    fn stale_waiter_cannot_abandon_newer_ticket() {
        let p = FakeProcess::new(7, 1);
        let r = Arc::new(registry(RegistryCapacities::default(), p));
        let g = publish_value(&r, key(16, 1), 1);
        let permit = r.try_acquire_mutation(g.identity).unwrap();
        let c1 = r.prepare(key(16, 1), 1, Arc::new(2_u64)).unwrap();
        let ticket1 = c1.ticket();
        let first_waiting = Arc::new(Barrier::new(2));
        let first_hook = first_waiting.clone();
        r.set_before_drain_wait(Some(Arc::new(move || {
            first_hook.wait();
        })));
        let (tx, rx) = mpsc::channel();
        let t1 = {
            let r = r.clone();
            thread::spawn(move || tx.send(r.publish(c1, Duration::from_secs(5))).unwrap())
        };
        first_waiting.wait();
        assert_eq!(
            r.state(&key(16, 1)),
            Some(GenerationState::Closing { ticket: ticket1 })
        );
        let c2 = r.prepare(key(16, 1), 1, Arc::new(3_u64)).unwrap();
        let ticket2 = c2.ticket();
        let second_waiting = Arc::new(Barrier::new(2));
        let second_hook = second_waiting.clone();
        r.set_before_drain_wait(Some(Arc::new(move || {
            second_hook.wait();
        })));
        let t2 = {
            let r = r.clone();
            thread::spawn(move || r.publish(c2, Duration::from_secs(5)))
        };
        second_waiting.wait();
        r.set_before_drain_wait(None);
        assert_eq!(rx.recv().unwrap(), Err(RegistryError::Superseded));
        assert_eq!(
            r.state(&key(16, 1)),
            Some(GenerationState::Closing { ticket: ticket2 })
        );
        drop(permit);
        t1.join().unwrap();
        assert!(t2.join().unwrap().is_ok());
    }
    #[test]
    fn poison_capacity_reserved_before_permit_and_closing_poison_wins() {
        let caps = RegistryCapacities {
            poison_records: 1,
            ..RegistryCapacities::default()
        };
        let p = FakeProcess::new(7, 1);
        let r = Arc::new(registry(caps, p));
        let a = publish_value(&r, key(17, 1), 1);
        let b = publish_value(&r, key(18, 1), 1);
        let mut permit = r.try_acquire_mutation(a.identity).unwrap();
        assert_eq!(
            r.try_acquire_mutation(b.identity).unwrap_err(),
            RegistryError::CapacityExhausted {
                resource: "poison_records"
            }
        );
        let c = r.prepare(key(17, 1), 1, Arc::new(2_u64)).unwrap();
        let rr = r.clone();
        let worker = thread::spawn(move || rr.publish(c, Duration::from_secs(2)));
        while !matches!(r.state(&key(17, 1)), Some(GenerationState::Closing { .. })) {
            thread::yield_now();
        }
        permit.poison_before_release("unknown").unwrap();
        drop(permit);
        assert!(matches!(
            worker.join().unwrap(),
            Err(RegistryError::Poisoned(_))
        ));
        assert_eq!(r.clear_exact_owner_process("legacy", 7, 1), 2);
        assert_eq!(r.counts().4, 0);
    }
    #[test]
    fn inactive_per_pid_lru_progresses_beyond_cap_but_protects_candidate() {
        let p = FakeProcess::new(7, 1);
        let r = registry(RegistryCapacities::default(), p);
        for w in 0..(LRU_CAP_PER_PID + 3) as u64 {
            publish_value(&r, key(100 + w, 1), w);
        }
        assert_eq!(r.counts().0, LRU_CAP_PER_PID);
        let c = r.prepare(key(999, 1), 1, Arc::new(())).unwrap();
        assert_eq!(
            r.clear_exact_owner_process("legacy", 7, 1),
            LRU_CAP_PER_PID - 1
        );
        drop(c);
    }

    #[test]
    fn saturated_per_pid_lanes_refuse_without_evicting_unrelated_pid() {
        let p = FakeProcess::new(7, 1);
        p.set(8, 1);
        let r = registry(RegistryCapacities::default(), p);
        let unrelated_key = LaneKey {
            runtime_owner: "legacy".into(),
            pid: 8,
            process_incarnation: 1,
            window_id: 900,
        };
        let unrelated = r
            .prepare(unrelated_key.clone(), 1, Arc::new(900_u64))
            .unwrap();
        let unrelated_handle = r.publish(unrelated, Duration::ZERO).unwrap();

        let mut permits = Vec::new();
        for window in 0..LRU_CAP_PER_PID as u64 {
            let generation = publish_value(&r, key(1_000 + window, 1), window);
            permits.push(r.try_acquire_mutation(generation.identity).unwrap());
        }

        assert_eq!(
            r.prepare(key(2_000, 1), 1, Arc::new(())).unwrap_err(),
            RegistryError::CapacityExhausted {
                resource: "lanes_per_pid"
            }
        );
        assert_eq!(r.counts().0, LRU_CAP_PER_PID + 1);
        assert!(r
            .inner
            .state
            .lock()
            .unwrap()
            .lanes
            .contains_key(&unrelated_key));
        assert!(r
            .resolve_generation(8, &format_token(unrelated_handle, 0))
            .is_ok());
        drop(permits);
    }
    #[test]
    fn nonce_claims_are_bounded_reusable_and_refuse_zero_or_collision() {
        let p = FakeProcess::new(7, 1);
        let nonce = 0xfeed_0000_0000_0001;
        for _ in 0..64 {
            let r = TokenRegistry::with_parts(
                RegistryCapacities::default(),
                Ok(nonce),
                1,
                1,
                p.clone(),
            );
            assert_eq!(r.runtime_nonce(), Ok(nonce));
            assert!(runtime_nonce_is_claimed(nonce));
            drop(r);
            assert!(!runtime_nonce_is_claimed(nonce));
        }
        let start = Arc::new(Barrier::new(3));
        let ready = Arc::new(Barrier::new(3));
        let release = Arc::new(Barrier::new(3));
        let racers: Vec<_> = (0..2)
            .map(|_| {
                let start = start.clone();
                let ready = ready.clone();
                let release = release.clone();
                let p = p.clone();
                thread::spawn(move || {
                    start.wait();
                    let registry = TokenRegistry::with_parts(
                        RegistryCapacities::default(),
                        Ok(nonce),
                        1,
                        1,
                        p,
                    );
                    let result = registry.runtime_nonce();
                    ready.wait();
                    release.wait();
                    result
                })
            })
            .collect();
        start.wait();
        ready.wait();
        assert!(runtime_nonce_is_claimed(nonce));
        assert!(active_runtime_nonce_claims() >= 1);
        release.wait();
        let results: Vec<_> = racers
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Ok(nonce))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Err(RegistryError::Collision))
                .count(),
            1
        );
        assert!(!runtime_nonce_is_claimed(nonce));

        let zero = TokenRegistry::with_parts(RegistryCapacities::default(), Ok(0), 1, 1, p);
        assert_eq!(zero.runtime_nonce(), Err(RegistryError::EntropyUnavailable));
    }
    #[test]
    fn entropy_and_counter_exhaustion_refuse() {
        let p = FakeProcess::new(7, 1);
        let no_entropy = TokenRegistry::with_parts(
            RegistryCapacities::default(),
            Err(RegistryError::EntropyUnavailable),
            1,
            1,
            p.clone(),
        );
        assert_eq!(
            no_entropy.prepare(key(1, 1), 1, Arc::new(())).unwrap_err(),
            RegistryError::EntropyUnavailable
        );
        let seq = TokenRegistry::with_parts(
            RegistryCapacities::default(),
            Ok(NEXT_TEST_NONCE.fetch_add(1, Ordering::AcqRel)),
            u64::MAX,
            1,
            p.clone(),
        );
        assert_eq!(
            seq.prepare(key(1, 1), 1, Arc::new(())).unwrap_err(),
            RegistryError::GenerationExhausted
        );
        let zero = TokenRegistry::with_parts(
            RegistryCapacities::default(),
            Ok(NEXT_TEST_NONCE.fetch_add(1, Ordering::AcqRel)),
            1,
            0,
            p,
        );
        assert_eq!(
            zero.prepare(key(1, 1), 1, Arc::new(())).unwrap_err(),
            RegistryError::PublicHandleExhausted
        );
    }
    struct ReenterDrop {
        registry: Arc<TokenRegistry>,
        dropped: Arc<Barrier>,
    }
    impl Drop for ReenterDrop {
        fn drop(&mut self) {
            let _ = self.registry.counts();
            self.dropped.wait();
        }
    }
    #[test]
    fn payload_drop_occurs_after_lane_and_registry_guards() {
        let p = FakeProcess::new(7, 1);
        let r = Arc::new(registry(RegistryCapacities::default(), p));
        let barrier = Arc::new(Barrier::new(2));
        let c = r
            .prepare(
                key(20, 1),
                1,
                Arc::new(ReenterDrop {
                    registry: r.clone(),
                    dropped: barrier.clone(),
                }),
            )
            .unwrap();
        r.publish(c, Duration::ZERO).unwrap();
        let c2 = r.prepare(key(20, 1), 1, Arc::new(())).unwrap();
        let rr = r.clone();
        let t = thread::spawn(move || rr.publish(c2, Duration::ZERO));
        barrier.wait();
        assert!(r.counts().0 > 0);
        assert!(t.join().unwrap().is_ok());
    }
    struct CountingReenterDrop {
        registry: Arc<TokenRegistry>,
        dropped: Arc<AtomicUsize>,
    }
    impl Drop for CountingReenterDrop {
        fn drop(&mut self) {
            let _ = self.registry.counts();
            self.dropped.fetch_add(1, Ordering::AcqRel);
        }
    }
    struct NonceReenterDrop {
        nonce: u64,
        process: Arc<dyn ProcessIncarnationProvider>,
        reclaimed: Arc<AtomicUsize>,
    }
    impl Drop for NonceReenterDrop {
        fn drop(&mut self) {
            let probe = TokenRegistry::with_parts(
                RegistryCapacities::default(),
                Ok(self.nonce),
                1,
                1,
                self.process.clone(),
            );
            if probe.runtime_nonce() == Ok(self.nonce) {
                self.reclaimed.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
    #[test]
    fn payload_drop_reentry_is_lock_free_on_cancel_cleanup_teardown_lru_and_destroy() {
        let p = FakeProcess::new(7, 1);
        let r = Arc::new(registry(RegistryCapacities::default(), p.clone()));
        let dropped = Arc::new(AtomicUsize::new(0));
        let payload = || {
            Arc::new(CountingReenterDrop {
                registry: r.clone(),
                dropped: dropped.clone(),
            })
        };

        drop(r.prepare(key(200, 1), 1, payload()).unwrap());
        assert_eq!(dropped.load(Ordering::Acquire), 1);

        let candidate = r.prepare(key(201, 1), 1, payload()).unwrap();
        r.publish(candidate, Duration::ZERO).unwrap();
        assert!(r.clear_exact_owner_process("legacy", 7, 1) >= 1);
        assert_eq!(dropped.load(Ordering::Acquire), 2);

        let candidate = r.prepare(key(202, 1), 1, payload()).unwrap();
        r.publish(candidate, Duration::ZERO).unwrap();
        assert_eq!(r.clear_runtime_scope("legacy"), 1);
        assert_eq!(dropped.load(Ordering::Acquire), 3);

        let candidate = r.prepare(key(300, 1), 1, payload()).unwrap();
        r.publish(candidate, Duration::ZERO).unwrap();
        for window in 301..=(300 + LRU_CAP_PER_PID as u64) {
            publish_value(&r, key(window, 1), window);
        }
        assert_eq!(dropped.load(Ordering::Acquire), 4);
        drop(r);

        let nonce = 0xfeed_0000_0000_0002;
        let reclaimed = Arc::new(AtomicUsize::new(0));
        let destroy =
            TokenRegistry::with_parts(RegistryCapacities::default(), Ok(nonce), 1, 1, p.clone());
        let candidate = destroy
            .prepare(
                key(400, 1),
                1,
                Arc::new(NonceReenterDrop {
                    nonce,
                    process: p,
                    reclaimed: reclaimed.clone(),
                }),
            )
            .unwrap();
        destroy.publish(candidate, Duration::ZERO).unwrap();
        drop(destroy);
        assert_eq!(reclaimed.load(Ordering::Acquire), 1);
        assert!(!runtime_nonce_is_claimed(nonce));
    }
    #[test]
    fn runtime_nonce_collision_and_public_binding_collision_refuse() {
        let p = FakeProcess::new(7, 1);
        let r = registry(RegistryCapacities::default(), p);
        let c = r.prepare(key(21, 1), 1, Arc::new(())).unwrap();
        r.inner
            .next_public_handle
            .store(c.public_handle() as u64, Ordering::Release);
        assert_eq!(
            r.prepare(key(22, 1), 1, Arc::new(())).unwrap_err(),
            RegistryError::Collision
        );
        drop(c);
    }
}
