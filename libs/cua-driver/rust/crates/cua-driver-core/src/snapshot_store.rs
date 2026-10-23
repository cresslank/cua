//! Typed snapshot projection backed by the hardened generation registry.
use crate::element_token::{self, MutationPermit, RegistryError, ResolvedElement};
use crate::protocol::ToolResult;
use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

/// Set only by trusted in-process dispatch after caller underscore arguments are stripped.
pub const NATIVE_WINDOW_PIXELS_ARG: &str = "_native_window_pixels";

pub trait SnapshotPayload: Send + Sync + 'static {
    type Element;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Published row membership; must not retain or touch native objects.
    fn contains_index(&self, index: usize) -> bool {
        index < self.len()
    }
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

// A projection of registry identity only. Native payload ownership stays in TokenRegistry.
struct SnapshotMetadata {
    identity: element_token::SnapshotIdentity,
    id: u32,
    window_id: u64,
    screenshot_owner: Option<String>,
    screenshot_scale: Option<f64>,
    screenshot_window_size: Option<(f64, f64)>,
    frame_revision: Arc<AtomicU64>,
    zoom: Option<ZoomContext>,
    semantic: bool,
}

struct SnapshotStoreState {
    snapshots: HashMap<i32, Vec<SnapshotMetadata>>,
    retired_screenshots: HashSet<(i32, u64)>,
    retired_screenshot_order: VecDeque<(i32, u64)>,
    retired_screenshot_overflowed: bool,
    retirement_epoch: u64,
    retired_sessions: HashSet<String>,
    sessions_overflowed: bool,
    runtime_retired: bool,
}

const RETIRED_SCREENSHOT_CAPACITY: usize = 256;

impl Default for SnapshotStoreState {
    fn default() -> Self {
        Self {
            snapshots: HashMap::new(),
            retired_screenshots: HashSet::new(),
            retired_screenshot_order: VecDeque::new(),
            retired_screenshot_overflowed: false,
            retirement_epoch: 0,
            retired_sessions: HashSet::new(),
            sessions_overflowed: false,
            runtime_retired: false,
        }
    }
}

impl SnapshotStoreState {
    fn retire_screenshot(&mut self, key: (i32, u64)) {
        if self.retired_screenshot_overflowed || self.retired_screenshots.contains(&key) {
            return;
        }
        if self.retired_screenshots.len() == RETIRED_SCREENSHOT_CAPACITY {
            self.retired_screenshot_overflowed = true;
            return;
        }
        self.retired_screenshots.insert(key);
        self.retired_screenshot_order.push_back(key);
    }

    fn restore_screenshot(&mut self, key: (i32, u64)) {
        if self.retired_screenshots.remove(&key) {
            self.retired_screenshot_order.retain(|entry| *entry != key);
        }
    }

    fn missing_screenshot_is_retired(&self, pid: i32, window_id: Option<u64>) -> bool {
        self.retired_screenshot_overflowed
            || match window_id {
                Some(window_id) => self.retired_screenshots.contains(&(pid, window_id)),
                None => self
                    .retired_screenshots
                    .iter()
                    .any(|(retired_pid, _)| *retired_pid == pid),
            }
    }

    fn reset_retired_screenshots(&mut self) {
        self.retired_screenshots.clear();
        self.retired_screenshot_order.clear();
        self.retired_screenshot_overflowed = false;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScreenshotContextError {
    ReplacedOrUnavailable,
}

/// A weak revision witness does not extend native payload lifetime.
#[derive(Clone, Debug)]
pub struct ScreenshotFrameBinding {
    revision: Weak<AtomicU64>,
    expected: u64,
}
impl ScreenshotFrameBinding {
    pub(crate) fn is_current(&self) -> bool {
        self.revision
            .upgrade()
            .is_some_and(|r| r.load(Ordering::Acquire) == self.expected)
    }
}
#[derive(Clone)]
pub struct ScreenshotRefreshTicket {
    store: Weak<Mutex<SnapshotStoreState>>,
    pid: i32,
    window_id: u64,
    owner: Option<String>,
    runtime_scope: String,
    epoch: u64,
    expected: Option<(element_token::SnapshotIdentity, u64)>,
}
fn valid_window_size(size: Option<(f64, f64)>) -> Option<(f64, f64)> {
    size.filter(|(w, h)| w.is_finite() && h.is_finite() && *w > 0.0 && *h > 0.0)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScreenshotContext {
    pub frame_revision: u64,
    pub snapshot_id: u32,
    pub window_id: u64,
    pub scale: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoomContext {
    pub screenshot: ScreenshotContext,
    pub origin_x: f64,
    pub origin_y: f64,
    pub scale_inv: f64,
}

impl ZoomContext {
    pub fn zoom_to_window(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.origin_x + x * self.scale_inv,
            self.origin_y + y * self.scale_inv,
        )
    }
}

fn screenshot_context_refusal(pid: Option<i32>, window_id: Option<u64>) -> ToolResult {
    ToolResult::error(
        "The latest snapshot for this window does not contain a screenshot owned by this session. Call get_window_state with a screenshot on the same connection before using pixels.",
    )
    .with_structured(serde_json::json!({
        "code": "screenshot_context_missing",
        "pid": pid,
        "window_id": window_id,
    }))
}

fn zoom_context_refusal(pid: i32, window_id: Option<u64>) -> ToolResult {
    let message = "The zoom coordinate context is missing or was replaced by a newer snapshot. Call get_window_state and zoom again on the same connection before using from_zoom coordinates.";
    ToolResult::error(message).with_structured(serde_json::json!({
        // Explicit shared refusal envelope: the context is resolved before any
        // input is dispatched, so nothing was delivered. Top-level fields stay
        // for existing consumers.
        "status": "refused",
        "refusal": { "code": "zoom_context_missing", "message": message },
        "code": "zoom_context_missing",
        "pid": pid,
        "window_id": window_id,
    }))
}

pub struct SnapshotStore<S: SnapshotPayload> {
    runtime_scope: String,
    _payload: std::marker::PhantomData<S>,
    inner: Arc<Mutex<SnapshotStoreState>>,
    _revive_hook: crate::session::SessionReviveHookRegistration,
}
impl<S: SnapshotPayload> SnapshotStore<S> {
    pub fn new() -> Self {
        let inner = Arc::new(Mutex::new(SnapshotStoreState::default()));
        let weak = Arc::downgrade(&inner);
        let revive_hook = crate::session::register_scoped_session_revive_hook(move |session| {
            if let Some(inner) = weak.upgrade() {
                let mut inner = inner.lock().unwrap();
                // Only an authenticated new lifecycle episode may use this
                // label again. Old in-flight publications keep the old epoch.
                if inner.retired_sessions.remove(session) {
                    inner.retirement_epoch = inner.retirement_epoch.saturating_add(1);
                }
            }
        });
        Self {
            runtime_scope: current_runtime_scope(),
            _payload: std::marker::PhantomData,
            inner,
            _revive_hook: revive_hook,
        }
    }
    pub fn try_publish(&self, pid: i32, window_id: u64, payload: S) -> Result<u32, RegistryError> {
        self.try_publish_for_session(pid, window_id, payload, None, None)?
            .map(|(id, _)| id)
            .ok_or(RegistryError::NotCurrent)
    }

    /// Fixture convenience; production must propagate registry errors.
    pub fn publish(&self, pid: i32, window_id: u64, payload: S) -> u32 {
        self.try_publish(pid, window_id, payload)
            .expect("snapshot fixture publication")
    }

    /// Compatibility convenience for fixtures. A registry failure is never a
    /// successful empty publication; production uses the fallible method below.
    pub fn publish_for_session(
        &self,
        pid: i32,
        window_id: u64,
        payload: S,
        session: Option<&str>,
        screenshot_scale: Option<f64>,
    ) -> Option<(u32, Vec<u32>)> {
        self.try_publish_for_session(pid, window_id, payload, session, screenshot_scale)
            .expect("snapshot fixture publication")
    }

    pub fn try_publish_for_session(
        &self,
        pid: i32,
        window_id: u64,
        payload: S,
        session: Option<&str>,
        screenshot_scale: Option<f64>,
    ) -> Result<Option<(u32, Vec<u32>)>, RegistryError> {
        self.try_publish_snapshot(
            pid,
            window_id,
            payload,
            session,
            screenshot_scale,
            None,
            true,
            None,
        )
    }

    pub fn publish_capture_for_session(
        &self,
        pid: i32,
        window_id: u64,
        payload: S,
        session: Option<&str>,
        screenshot_scale: Option<f64>,
    ) -> Option<(u32, Vec<u32>)> {
        self.try_publish_capture_for_session(pid, window_id, payload, session, screenshot_scale)
            .expect("snapshot fixture publication")
    }

    pub fn try_publish_capture_for_session(
        &self,
        pid: i32,
        window_id: u64,
        payload: S,
        session: Option<&str>,
        screenshot_scale: Option<f64>,
    ) -> Result<Option<(u32, Vec<u32>)>, RegistryError> {
        self.try_publish_snapshot(
            pid,
            window_id,
            payload,
            session,
            screenshot_scale,
            None,
            false,
            None,
        )
    }

    pub fn try_publish_sized_for_session(
        &self,
        pid: i32,
        window_id: u64,
        payload: S,
        session: Option<&str>,
        screenshot_scale: Option<f64>,
        window_size: Option<(f64, f64)>,
    ) -> Result<Option<(u32, Vec<u32>)>, RegistryError> {
        self.try_publish_snapshot(
            pid,
            window_id,
            payload,
            session,
            screenshot_scale,
            window_size,
            true,
            None,
        )
    }

    /// Capture this ticket before image work. Commit never adopts a newer tree,
    /// another owner's generation, or an image published while the work ran.
    pub fn screenshot_refresh_ticket(
        &self,
        pid: i32,
        window_id: u64,
        session: Option<&str>,
    ) -> Result<ScreenshotRefreshTicket, RegistryError> {
        let ended = Self::session_ended(session);
        let inner = self.inner.lock().unwrap();
        if ended
            || current_runtime_scope() != self.runtime_scope
            || Self::session_retired(&inner, session)
            || inner.retired_screenshot_overflowed
            || inner.sessions_overflowed
            || inner.missing_screenshot_is_retired(pid, Some(window_id))
        {
            return Err(RegistryError::NotCurrent);
        }
        let snapshot = inner
            .snapshots
            .get(&pid)
            .into_iter()
            .flatten()
            .find(|s| s.window_id == window_id);
        if snapshot.is_some_and(|s| {
            !self.metadata_live(pid, s) || s.screenshot_owner.as_deref() != session
        }) {
            return Err(RegistryError::NotCurrent);
        }
        Ok(ScreenshotRefreshTicket {
            store: Arc::downgrade(&self.inner),
            pid,
            window_id,
            owner: session.map(str::to_owned),
            runtime_scope: self.runtime_scope.clone(),
            epoch: inner.retirement_epoch,
            expected: snapshot.map(|s| (s.identity, s.frame_revision.load(Ordering::Acquire))),
        })
    }

    pub fn refresh_screenshot_for_session(
        &self,
        ticket: ScreenshotRefreshTicket,
        screenshot_scale: f64,
        window_size: Option<(f64, f64)>,
    ) -> Result<Option<ScreenshotContext>, RegistryError> {
        let session = ticket.owner.as_deref();
        let ended = Self::session_ended(session);
        let mut inner = self.inner.lock().unwrap();
        if ended
            || current_runtime_scope() != self.runtime_scope
            || !ticket.store.ptr_eq(&Arc::downgrade(&self.inner))
            || ticket.runtime_scope != self.runtime_scope
            || ticket.epoch != inner.retirement_epoch
            || Self::session_retired(&inner, session)
            || inner.retired_screenshot_overflowed
            || inner.sessions_overflowed
            || !screenshot_scale.is_finite()
            || screenshot_scale <= 0.0
        {
            return Err(RegistryError::NotCurrent);
        }
        let snapshot = inner
            .snapshots
            .get_mut(&ticket.pid)
            .into_iter()
            .flatten()
            .find(|s| s.window_id == ticket.window_id);
        let actual = snapshot
            .as_ref()
            .map(|s| (s.identity, s.frame_revision.load(Ordering::Acquire)));
        if actual != ticket.expected {
            return Err(RegistryError::NotCurrent);
        }
        let Some(snapshot) = snapshot else {
            return Ok(None);
        };
        if !self.metadata_live(ticket.pid, snapshot)
            || snapshot.screenshot_owner.as_deref() != session
        {
            return Err(RegistryError::NotCurrent);
        }
        let permit = element_token::global().try_acquire_mutation(snapshot.identity)?;
        let payload = permit.payload::<S>().ok_or(RegistryError::NotCurrent)?;
        if !snapshot.semantic || payload.is_empty() {
            return Ok(None);
        }
        let revision = snapshot
            .frame_revision
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or(RegistryError::NotCurrent)?;
        snapshot.frame_revision.store(revision, Ordering::Release);
        snapshot.screenshot_scale = Some(screenshot_scale);
        snapshot.screenshot_window_size = valid_window_size(window_size);
        snapshot.zoom = None;
        self.screenshot(ticket.pid, snapshot, session, false)
            .map(Some)
            .ok_or(RegistryError::NotCurrent)
    }

    pub fn try_publish_capture_for_refresh(
        &self,
        ticket: ScreenshotRefreshTicket,
        payload: S,
        scale: f64,
    ) -> Result<Option<(u32, Vec<u32>)>, RegistryError> {
        self.try_publish_snapshot(
            ticket.pid,
            ticket.window_id,
            payload,
            ticket.owner.as_deref(),
            Some(scale),
            None,
            false,
            Some(&ticket),
        )
    }

    pub fn frame_binding(
        &self,
        pid: i32,
        session: Option<&str>,
        expected: ScreenshotContext,
    ) -> Result<ScreenshotFrameBinding, ToolResult> {
        let ended = Self::session_ended(session);
        let inner = self.inner.lock().unwrap();
        let snapshot = inner
            .snapshots
            .get(&pid)
            .into_iter()
            .flatten()
            .find(|s| {
                !Self::session_retired(&inner, session)
                    && self.screenshot(pid, s, session, ended) == Some(expected)
            })
            .ok_or_else(|| screenshot_context_refusal(Some(pid), Some(expected.window_id)))?;
        Ok(ScreenshotFrameBinding {
            revision: Arc::downgrade(&snapshot.frame_revision),
            expected: expected.frame_revision,
        })
    }

    fn try_publish_snapshot(
        &self,
        pid: i32,
        window_id: u64,
        payload: S,
        session: Option<&str>,
        screenshot_scale: Option<f64>,
        window_size: Option<(f64, f64)>,
        semantic: bool,
        refresh: Option<&ScreenshotRefreshTicket>,
    ) -> Result<Option<(u32, Vec<u32>)>, RegistryError> {
        if current_runtime_scope() != self.runtime_scope {
            return Err(RegistryError::NotCurrent);
        }
        if session.is_some_and(crate::session::is_session_ended) {
            return Ok(None);
        }
        let (epoch, inherited) = {
            let inner = self.inner.lock().unwrap();
            if Self::session_retired(&inner, session) {
                return Ok(None);
            }
            if let Some(ticket) = refresh {
                let current = inner
                    .snapshots
                    .get(&pid)
                    .into_iter()
                    .flatten()
                    .find(|s| s.window_id == window_id)
                    .map(|s| (s.identity, s.frame_revision.load(Ordering::Acquire)));
                if !ticket.store.ptr_eq(&Arc::downgrade(&self.inner))
                    || ticket.epoch != inner.retirement_epoch
                    || ticket.expected != current
                    || ticket.runtime_scope != self.runtime_scope
                    || ticket.owner.as_deref() != session
                {
                    return Err(RegistryError::NotCurrent);
                }
            }
            let inherited = inner
                .snapshots
                .get(&pid)
                .into_iter()
                .flatten()
                .find(|s| {
                    s.window_id == window_id
                        && self.metadata_live(pid, s)
                        && s.screenshot_owner.as_deref() == session
                })
                .filter(
                    |s| match (valid_window_size(window_size), s.screenshot_window_size) {
                        (Some((w, h)), Some((old_w, old_h))) => {
                            (w - old_w).abs() < 0.5 && (h - old_h).abs() < 0.5
                        }
                        _ => false,
                    },
                )
                .and_then(|s| {
                    Some((
                        s.screenshot_scale?,
                        s.screenshot_window_size?,
                        s.frame_revision.clone(),
                        s.frame_revision.load(Ordering::Acquire),
                        s.identity,
                    ))
                });
            (inner.retirement_epoch, inherited)
        };
        let registry = element_token::global();
        // Both prepare (eviction) and publish (replacement) can destroy native
        // payloads. Never call them while holding our metadata mutex.
        let candidate =
            registry.prepare_current(pid, window_id, payload.len(), Arc::new(payload))?;
        let identity = candidate.identity();
        let id = if let Some(ticket) = refresh {
            registry.publish_replacing(
                candidate,
                Duration::from_secs(2),
                ticket.expected.map(|(identity, _)| identity),
            )?
        } else if screenshot_scale.is_none() && inherited.is_some() {
            registry.publish_replacing(
                candidate,
                Duration::from_secs(2),
                inherited.as_ref().map(|(_, _, _, _, identity)| *identity),
            )?
        } else {
            registry.publish(candidate, Duration::from_secs(2))?
        };
        let mut invalidated = Vec::new();
        let accepted = {
            let mut inner = self.inner.lock().unwrap();
            if epoch != inner.retirement_epoch || Self::session_retired(&inner, session) {
                false
            } else if !registry.is_current_generation(pid, window_id, identity) {
                false
            } else {
                let inherited = inherited.filter(|(_, _, revision, expected, _)| {
                    revision.load(Ordering::Acquire) == *expected
                });
                let mut stale = Vec::new();
                for (entry_pid, lane) in &mut inner.snapshots {
                    lane.retain(|entry| {
                        let live = registry.is_current_generation(
                            *entry_pid,
                            entry.window_id,
                            entry.identity,
                        );
                        if !live {
                            stale.push((*entry_pid, entry.window_id));
                            if *entry_pid == pid {
                                invalidated.push(entry.id);
                            }
                        }
                        live
                    });
                }
                inner.snapshots.retain(|_, lane| !lane.is_empty());
                for key in stale {
                    inner.retire_screenshot(key);
                }
                inner.restore_screenshot((pid, window_id));
                let lane = inner.snapshots.entry(pid).or_default();
                lane.retain(|entry| entry.window_id != window_id);
                lane.push(SnapshotMetadata {
                    identity,
                    id,
                    window_id,
                    screenshot_owner: session.map(str::to_owned),
                    zoom: None,
                    semantic,
                    screenshot_window_size: if screenshot_scale.is_some() {
                        valid_window_size(window_size)
                    } else {
                        inherited.as_ref().map(|(_, size, _, _, _)| *size)
                    },
                    frame_revision: Arc::new(AtomicU64::new(1)),
                    screenshot_scale: screenshot_scale
                        .or(inherited.as_ref().map(|(scale, _, _, _, _)| *scale))
                        .filter(|scale| scale.is_finite() && *scale > 0.0),
                });
                true
            }
        };
        if accepted {
            Ok(Some((id, invalidated)))
        } else {
            registry.retire_generation(identity);
            Err(RegistryError::NotCurrent)
        }
    }

    fn session_retired(inner: &SnapshotStoreState, session: Option<&str>) -> bool {
        inner.runtime_retired
            || session.is_some_and(|session| {
                inner.sessions_overflowed || inner.retired_sessions.contains(session)
            })
    }

    fn metadata_live(&self, pid: i32, snapshot: &SnapshotMetadata) -> bool {
        current_runtime_scope() == self.runtime_scope
            && element_token::global().is_current_generation(
                pid,
                snapshot.window_id,
                snapshot.identity,
            )
    }

    /// Read session tombstones before taking the store mutex. Revival runs its
    /// hooks, this store's included, while holding the tombstone lock, so a
    /// tombstone read under the store mutex inverts that order and deadlocks
    /// against a concurrent revival of an idle-reclaimed session.
    fn session_ended(session: Option<&str>) -> bool {
        session.is_some_and(crate::session::is_session_ended)
    }

    fn screenshot(
        &self,
        pid: i32,
        snapshot: &SnapshotMetadata,
        session: Option<&str>,
        session_ended: bool,
    ) -> Option<ScreenshotContext> {
        if !self.metadata_live(pid, snapshot)
            || session_ended
            || snapshot.screenshot_owner.as_deref() != session
        {
            return None;
        }
        Some(ScreenshotContext {
            frame_revision: snapshot.frame_revision.load(Ordering::Acquire),
            snapshot_id: snapshot.id,
            window_id: snapshot.window_id,
            scale: snapshot.screenshot_scale?,
        })
    }

    pub fn validate_screenshot_context(
        &self,
        pid: i32,
        session: Option<&str>,
        expected: ScreenshotContext,
    ) -> Result<(), ToolResult> {
        if self.screenshot_context(pid, Some(expected.window_id), session)? == expected {
            Ok(())
        } else {
            Err(screenshot_context_refusal(
                Some(pid),
                Some(expected.window_id),
            ))
        }
    }

    pub fn screenshot_context_or_refusal(
        &self,
        pid: i32,
        window_id: u64,
        session: Option<&str>,
    ) -> Result<ScreenshotContext, ToolResult> {
        self.screenshot_context(pid, Some(window_id), session)
    }
    pub fn with_valid_screenshot_context<R>(
        &self,
        pid: i32,
        session: Option<&str>,
        expected: ScreenshotContext,
        publish: impl FnOnce() -> R,
    ) -> Result<R, ToolResult> {
        let session_ended = Self::session_ended(session);
        let inner = self.inner.lock().unwrap();
        if Self::session_retired(&inner, session)
            || !inner.snapshots.get(&pid).is_some_and(|lane| {
                lane.iter().any(|snapshot| {
                    self.screenshot(pid, snapshot, session, session_ended) == Some(expected)
                })
            })
        {
            return Err(screenshot_context_refusal(
                Some(pid),
                Some(expected.window_id),
            ));
        }
        let result = publish();
        drop(inner);
        Ok(result)
    }

    /// Resolve the screenshot transform from the same authoritative latest
    /// snapshot used for element tokens. Without a window, every snapshot of
    /// the process must agree on one transform owned by this session.
    pub fn screenshot_context(
        &self,
        pid: i32,
        window_id: Option<u64>,
        session: Option<&str>,
    ) -> Result<ScreenshotContext, ToolResult> {
        let session_ended = Self::session_ended(session);
        let inner = self.inner.lock().unwrap();
        let lane = inner
            .snapshots
            .get(&pid)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let context = match window_id {
            Some(window_id) => lane
                .iter()
                .find(|snapshot| snapshot.window_id == window_id)
                .and_then(|snapshot| self.screenshot(pid, snapshot, session, session_ended)),
            None => {
                let mut contexts = lane
                    .iter()
                    .map(|snapshot| self.screenshot(pid, snapshot, session, session_ended));
                contexts.next().flatten().filter(|first| {
                    contexts.all(|context| {
                        context.is_some_and(|context| (context.scale - first.scale).abs() < 1e-9)
                    })
                })
            }
        };
        context.ok_or_else(|| screenshot_context_refusal(Some(pid), window_id))
    }

    /// The native-image / delivered-image scale for a window-relative pixel
    /// action: 1.0 for a trusted in-process call whose pixels are already
    /// native ([`NATIVE_WINDOW_PIXELS_ARG`]), otherwise the scale of the
    /// session's current screenshot of the window (refused without one).
    pub fn screenshot_scale(
        &self,
        pid: i32,
        window_id: Option<u64>,
        args: &serde_json::Value,
    ) -> Result<f64, ToolResult> {
        if args.get(NATIVE_WINDOW_PIXELS_ARG) == Some(&serde_json::Value::Bool(true)) {
            return Ok(1.0);
        }
        self.screenshot_context(
            pid,
            window_id,
            args.get("_session_id").and_then(serde_json::Value::as_str),
        )
        .map(|context| context.scale)
    }

    pub fn screenshot_context_for_zoom(
        &self,
        pid: Option<i32>,
        window_id: u64,
        session: Option<&str>,
    ) -> Result<(i32, ScreenshotContext), ToolResult> {
        if let Some(pid) = pid {
            return self
                .screenshot_context(pid, Some(window_id), session)
                .map(|context| (pid, context));
        }
        let session_ended = Self::session_ended(session);
        let inner = self.inner.lock().unwrap();
        let mut matches = inner.snapshots.iter().filter_map(|(pid, lane)| {
            let snapshot = lane
                .iter()
                .find(|snapshot| snapshot.window_id == window_id)?;
            Some((
                *pid,
                self.screenshot(*pid, snapshot, session, session_ended)?,
            ))
        });
        match (matches.next(), matches.next()) {
            (Some(found), None) => Ok(found),
            _ => Err(screenshot_context_refusal(None, Some(window_id))),
        }
    }

    pub fn set_zoom(
        &self,
        pid: i32,
        session: Option<&str>,
        zoom: ZoomContext,
    ) -> Result<(), ToolResult> {
        let session_ended = Self::session_ended(session);
        let mut inner = self.inner.lock().unwrap();
        let snapshot = inner
            .snapshots
            .get_mut(&pid)
            .and_then(|lane| {
                lane.iter_mut()
                    .find(|snapshot| snapshot.window_id == zoom.screenshot.window_id)
            })
            .filter(|snapshot| {
                self.screenshot(pid, snapshot, session, session_ended) == Some(zoom.screenshot)
            })
            .ok_or_else(|| zoom_context_refusal(pid, Some(zoom.screenshot.window_id)))?;
        snapshot.zoom = Some(zoom);
        Ok(())
    }

    pub fn zoom(
        &self,
        pid: i32,
        window_id: Option<u64>,
        session: Option<&str>,
    ) -> Result<ZoomContext, ToolResult> {
        let session_ended = Self::session_ended(session);
        let inner = self.inner.lock().unwrap();
        let mut zooms = inner
            .snapshots
            .get(&pid)
            .into_iter()
            .flatten()
            .filter(|snapshot| {
                window_id.is_none_or(|window_id| snapshot.window_id == window_id)
                    && self
                        .screenshot(pid, snapshot, session, session_ended)
                        .is_some()
            })
            .filter_map(|snapshot| snapshot.zoom);
        match (zooms.next(), zooms.next()) {
            (Some(zoom), None) => Ok(zoom),
            _ => Err(zoom_context_refusal(pid, window_id)),
        }
    }

    /// Retirement and metadata publication linearize under the same mutex.
    /// The epoch rejects already-building publications; a bounded tombstone set
    /// rejects work beginning after retirement, even before the session hook finishes.
    pub fn retire_session_screenshots(&self, session: &str) -> usize {
        let retired = {
            let mut inner = self.inner.lock().unwrap();
            inner.retirement_epoch = inner.retirement_epoch.saturating_add(1);
            if inner.retired_sessions.len() < RETIRED_SCREENSHOT_CAPACITY {
                inner.retired_sessions.insert(session.to_owned());
            } else {
                inner.sessions_overflowed = true;
            }
            let mut retired = Vec::new();
            for (pid, lane) in &mut inner.snapshots {
                lane.retain(|entry| {
                    if entry.screenshot_owner.as_deref() == Some(session) {
                        retired.push((*pid, entry.window_id, entry.identity));
                        false
                    } else {
                        true
                    }
                });
            }
            inner.snapshots.retain(|_, lane| !lane.is_empty());
            retired.sort_unstable_by_key(|(pid, window, _)| (*pid, *window));
            for (pid, window, _) in &retired {
                inner.retire_screenshot((*pid, *window));
            }
            retired
        };
        for (_, _, identity) in &retired {
            element_token::global().retire_generation(*identity);
        }
        retired.len()
    }

    pub fn window_for_snapshot(&self, pid: i32, snapshot_id: u32) -> Option<u64> {
        let inner = self.inner.lock().unwrap();
        inner
            .snapshots
            .get(&pid)?
            .iter()
            .find(|entry| {
                entry.id == snapshot_id
                    && self.metadata_live(pid, entry)
                    && !Self::session_retired(&inner, entry.screenshot_owner.as_deref())
            })
            .map(|entry| entry.window_id)
    }

    /// Metadata bridge only: this identity never owns a second native payload.
    pub fn identity_for_snapshot(
        &self,
        pid: i32,
        snapshot_id: u32,
    ) -> Option<element_token::SnapshotIdentity> {
        let inner = self.inner.lock().unwrap();
        inner
            .snapshots
            .get(&pid)?
            .iter()
            .find(|entry| {
                entry.id == snapshot_id
                    && self.metadata_live(pid, entry)
                    && !Self::session_retired(&inner, entry.screenshot_owner.as_deref())
            })
            .map(|entry| entry.identity)
    }

    pub fn contains_window(&self, pid: i32, window_id: u64) -> bool {
        self.inner
            .lock()
            .unwrap()
            .snapshots
            .get(&pid)
            .is_some_and(|lane| {
                lane.iter()
                    .any(|entry| entry.window_id == window_id && self.metadata_live(pid, entry))
            })
    }
    pub fn contains_semantic_window(&self, pid: i32, window_id: u64) -> bool {
        self.inner
            .lock()
            .unwrap()
            .snapshots
            .get(&pid)
            .is_some_and(|lane| {
                lane.iter().any(|entry| {
                    entry.window_id == window_id && entry.semantic && self.metadata_live(pid, entry)
                })
            })
    }

    /// Infer a token's process only from this store's live runtime generation.
    /// This is routing metadata, not mutation authority: `resolve` must still
    /// acquire and retain the generation's MutationPermit before any action.
    pub fn pid_for_token(&self, args: &serde_json::Value) -> Option<i32> {
        let token = args.get("element_token")?.as_str()?;
        let (snapshot_id, _) = element_token::parse_token(token)?;
        let inner = self.inner.lock().unwrap();
        inner.snapshots.iter().find_map(|(pid, lane)| {
            lane.iter()
                .any(|snapshot| {
                    snapshot.id == snapshot_id
                        && self.metadata_live(*pid, snapshot)
                        && !Self::session_retired(&inner, snapshot.screenshot_owner.as_deref())
                })
                .then_some(*pid)
        })
    }

    fn canonical_row(&self, pid: i32, window_id: Option<u64>, row: usize) -> Option<(String, u64)> {
        let inner = self.inner.lock().unwrap();
        let mut matching = inner.snapshots.get(&pid).into_iter().flatten().filter(|s| {
            s.semantic
                && window_id.is_none_or(|w| w == s.window_id)
                && self.metadata_live(pid, s)
                && !Self::session_retired(&inner, s.screenshot_owner.as_deref())
        });
        let snapshot = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        let token = element_token::token_for(snapshot.id, row);
        let (generation, _) = element_token::global()
            .resolve_generation(pid, &token)
            .ok()?;
        if !generation.payload::<S>()?.contains_index(row) {
            return None;
        }
        Some((token, snapshot.window_id))
    }

    pub fn resolve(
        &self,
        pid: i32,
        args: &serde_json::Value,
    ) -> Result<ResolvedElement<AdmittedElement<S::Element>>, ToolResult> {
        let Some(token) = args
            .get("element_token")
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(ResolvedElement::None);
        };
        let stale = || {
            element_token::refusal(
                "stale_element_token",
                element_token::STALE_TOKEN_ERROR.into(),
            )
        };
        if current_runtime_scope() != self.runtime_scope {
            return Err(stale());
        }
        let (generation, element_index) = element_token::global().resolve_generation(pid, token).map_err(|message| {
            let code = if message == element_token::STALE_TOKEN_ERROR || message.contains("another runtime generation") { "stale_element_token" } else { "invalid_element_token" };
            let mut refusal = element_token::refusal(code, message);
            if code == "stale_element_token" {
                let inner = self.inner.lock().unwrap();
                let current: Vec<_> = inner.snapshots.get(&pid).into_iter().flatten()
                    .filter(|entry| self.metadata_live(pid, entry))
                    .map(|entry| serde_json::json!({"snapshot_id": element_token::format_snapshot_id(entry.id), "window_id": entry.window_id})).collect();
                refusal.structured_content.as_mut().unwrap()["current_snapshots"] = serde_json::json!(current);
            }
            refusal
        })?;
        element_token::cross_check_legacy_index(args, element_index)?;
        let snapshot_identity = generation.identity();
        let window_id = generation.lane_key().window_id;
        if args["window_id"]
            .as_u64()
            .is_some_and(|window| window != window_id)
        {
            return Err(element_token::refusal(
                "conflicting_element_target",
                "element_token conflicts with window_id".into(),
            ));
        }
        let inner = self.inner.lock().unwrap();
        let owned = inner.snapshots.get(&pid).is_some_and(|lane| {
            lane.iter().any(|entry| {
                entry.identity == snapshot_identity
                    && self.metadata_live(pid, entry)
                    && !Self::session_retired(&inner, entry.screenshot_owner.as_deref())
            })
        });
        if !owned {
            return Err(stale());
        }
        let permit = element_token::global()
            .try_acquire_mutation(snapshot_identity)
            .map_err(|_| stale())?;
        drop(inner);
        let element = permit
            .payload::<S>()
            .and_then(|payload| payload.retain(element_index))
            .ok_or_else(stale)?;
        Ok(ResolvedElement::Element {
            window_id,
            element_index,
            snapshot_identity,
            element: AdmittedElement {
                element,
                _permit: Some(Arc::new(permit)),
            },
        })
    }
    pub fn remove(&self, pid: i32, window_id: u64) -> Option<u32> {
        let retired = {
            let mut inner = self.inner.lock().unwrap();
            inner.retirement_epoch = inner.retirement_epoch.saturating_add(1);
            let retired = inner.snapshots.get_mut(&pid).and_then(|lane| {
                let position = lane.iter().position(|entry| entry.window_id == window_id)?;
                Some(lane.remove(position))
            });
            inner.snapshots.retain(|_, lane| !lane.is_empty());
            if retired.is_some() {
                inner.retire_screenshot((pid, window_id));
            }
            retired
        };
        retired.map(|entry| {
            element_token::global().retire_generation(entry.identity);
            entry.id
        })
    }
    pub fn clear(&self) -> usize {
        let retired = {
            let mut inner = self.inner.lock().unwrap();
            inner.retirement_epoch = inner.retirement_epoch.saturating_add(1);
            inner.reset_retired_screenshots();
            // Session tombstones survive clear: clear cannot revive an ended session.
            std::mem::take(&mut inner.snapshots)
        };
        retired
            .into_values()
            .flatten()
            .filter(|entry| element_token::global().retire_generation(entry.identity))
            .count()
    }
}
impl<S: SnapshotPayload> Default for SnapshotStore<S> {
    fn default() -> Self {
        Self::new()
    }
}
impl<S: SnapshotPayload> Drop for SnapshotStore<S> {
    fn drop(&mut self) {
        self.clear();
        let mut caches = runtime_stores().lock().unwrap();
        if let Some(lane) = caches.get_mut(&self.runtime_scope) {
            lane.retain(|cache| {
                cache.strong_count() > 0 && !std::ptr::addr_eq(cache.as_ptr(), self as *const Self)
            });
        }
        caches.retain(|_, lane| !lane.is_empty());
        if caches.is_empty() {
            caches.shrink_to_fit();
        }
    }
}
trait RuntimeStore: Any + Send + Sync {
    fn retire(&self) -> usize;
    fn canonical_row(&self, pid: i32, window: Option<u64>, row: usize) -> Option<(String, u64)>;
}
impl<S: SnapshotPayload> RuntimeStore for SnapshotStore<S> {
    fn canonical_row(&self, pid: i32, window: Option<u64>, row: usize) -> Option<(String, u64)> {
        self.canonical_row(pid, window, row)
    }
    fn retire(&self) -> usize {
        self.inner.lock().unwrap().runtime_retired = true;
        self.clear()
    }
}
fn runtime_stores() -> &'static Mutex<HashMap<String, Vec<Weak<dyn RuntimeStore>>>> {
    static CACHES: OnceLock<Mutex<HashMap<String, Vec<Weak<dyn RuntimeStore>>>>> = OnceLock::new();
    CACHES.get_or_init(|| Mutex::new(HashMap::new()))
}
fn current_runtime_scope() -> String {
    crate::tool::current_dispatch_runtime_scope().unwrap_or_else(|| "legacy".into())
}
pub fn register_runtime_store<S: SnapshotPayload>(cache: &Arc<SnapshotStore<S>>) {
    let erased: Arc<dyn RuntimeStore> = cache.clone();
    let mut caches = runtime_stores().lock().unwrap();
    for lane in caches.values_mut() {
        lane.retain(|cache| cache.strong_count() > 0);
    }
    caches.retain(|_, lane| !lane.is_empty());
    let lane = caches.entry(cache.runtime_scope.clone()).or_default();
    lane.retain(|cache| !std::ptr::addr_eq(cache.as_ptr(), Arc::as_ptr(&erased)));
    lane.push(Arc::downgrade(&erased));
}
pub fn current_runtime_store<S: SnapshotPayload>() -> Option<Arc<SnapshotStore<S>>> {
    let cache = runtime_stores()
        .lock()
        .unwrap()
        .get(&current_runtime_scope())?
        .last()?
        .upgrade()?;
    let erased: Arc<dyn Any + Send + Sync> = cache;
    erased.downcast().ok()
}
/// Pin a bare row to a full token and exact target before policy or queuing.
/// Full-token refusal remains at the existing post-authorization boundary.
pub(crate) fn canonicalize_bare_row(args: &mut serde_json::Value) -> Result<(), ToolResult> {
    let Some(raw) = args
        .get("element_token")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(());
    };
    if element_token::parse_token(raw).is_some() {
        return Ok(());
    }
    let Some(row) = raw.trim().parse::<usize>().ok() else {
        return Ok(());
    };
    let invalid = || {
        element_token::refusal("invalid_element_token", "A bare row requires a pid and one current window containing that published row; use the full token from get_window_state.".into())
    };
    let pid = args
        .get("pid")
        .and_then(serde_json::Value::as_i64)
        .and_then(|p| i32::try_from(p).ok())
        .ok_or_else(invalid)?;
    let window = args.get("window_id").and_then(serde_json::Value::as_u64);
    let stores: Vec<_> = runtime_stores()
        .lock()
        .unwrap()
        .get(&current_runtime_scope())
        .into_iter()
        .flatten()
        .filter_map(Weak::upgrade)
        .collect();
    let mut matches = stores
        .iter()
        .filter_map(|store| store.canonical_row(pid, window, row));
    let (token, window) = matches.next().ok_or_else(invalid)?;
    if matches.any(|found| found != (token.clone(), window)) {
        return Err(invalid());
    }
    args["element_token"] = token.into();
    args["pid"] = pid.into();
    args["window_id"] = window.into();
    Ok(())
}

pub fn retire_runtime_scope(runtime_scope: &str) -> usize {
    let caches = runtime_stores()
        .lock()
        .unwrap()
        .remove(runtime_scope)
        .unwrap_or_default();
    let mut retired = 0;
    for cache in caches {
        if let Some(cache) = cache.upgrade() {
            retired += cache.retire();
        }
    }
    #[cfg(target_os = "linux")]
    let registry_retired = element_token::global_if_initialized()
        .map_or(0, |registry| registry.clear_runtime_scope(runtime_scope));
    #[cfg(not(target_os = "linux"))]
    let registry_retired = element_token::global().clear_runtime_scope(runtime_scope);
    retired + registry_retired
}
#[cfg(test)]
mod hardened_tests {
    use super::*;
    use crate::element_token::{token_for, LRU_CAP_PER_PID, STALE_TOKEN_ERROR};
    use crate::snapshot_test_support::Payload;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// This fork binds snapshot metadata to the process-global
    /// `TokenRegistry`, keyed by runtime scope, pid, and window. Tests carried
    /// from upstream reuse this process's pid and fixed window ids, so under
    /// the default parallel runner one test's publication would legitimately
    /// retire another's generation. Each runs in its own runtime scope.
    fn isolated(test: impl FnOnce()) {
        crate::tool::with_runtime_scope(
            format!("element-cache-test-{}", uuid::Uuid::new_v4()),
            test,
        )
    }

    fn resolve(
        cache: &SnapshotStore<Payload>,
        pid: i32,
        token: &str,
    ) -> Result<(u64, usize), String> {
        match cache.resolve(
            pid,
            &serde_json::json!({"element_token": token, "window_id": Option::<u64>::None}),
        ) {
            Ok(ResolvedElement::Element {
                window_id: window,
                element_index,
                ..
            }) => Ok((window, element_index)),
            _ => Err(STALE_TOKEN_ERROR.into()),
        }
    }

    #[test]
    fn capture_only_metadata_revokes_elements_and_rejects_stale_identity() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let pid = std::process::id() as i32;
            let cache = SnapshotStore::new();
            let old = cache
                .try_publish_for_session(
                    pid,
                    701,
                    Payload(vec![1]),
                    Some("bridge-owner"),
                    Some(2.0),
                )
                .map(|entry| entry.map(|(id, _)| id))
                .unwrap()
                .unwrap();
            let old_context = cache
                .screenshot_context(pid, Some(701), Some("bridge-owner"))
                .unwrap();
            let empty = cache
                .try_publish_for_session(pid, 701, Payload(vec![]), Some("bridge-owner"), Some(3.0))
                .map(|entry| entry.map(|(id, _)| id))
                .unwrap()
                .unwrap();
            assert_ne!(old, empty);
            assert_eq!(
                cache
                    .validate_screenshot_context(pid, Some("bridge-owner"), old_context)
                    .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                Err(ScreenshotContextError::ReplacedOrUnavailable)
            );
            for (id, code) in [
                (old, "stale_element_token"),
                (empty, "invalid_element_token"),
            ] {
                let error = cache
                    .resolve(
                        pid,
                        &serde_json::json!({"element_token": &token_for(id, 0), "window_id": 701}),
                    )
                    .unwrap_err();
                assert_eq!(error.structured_content.unwrap()["refusal"]["code"], code);
            }
            assert_eq!(
                cache
                    .screenshot_context(pid, Some(701), Some("bridge-owner"))
                    .map(|context| Some(context.scale))
                    .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                Ok(Some(3.0))
            );
            let identity = cache.identity_for_snapshot(pid, empty).unwrap();
            element_token::global().retire_generation(identity);
            assert_eq!(
                cache
                    .screenshot_context(pid, Some(701), Some("bridge-owner"))
                    .map(|context| Some(context.scale))
                    .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                Err(ScreenshotContextError::ReplacedOrUnavailable)
            );
        });
    }

    #[test]
    fn retired_session_cannot_publish_after_clear() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let pid = std::process::id() as i32;
            cache
                .try_publish_for_session(
                    pid,
                    702,
                    Payload(vec![1]),
                    Some("bridge-retired"),
                    Some(1.0),
                )
                .map(|entry| entry.map(|(id, _)| id))
                .unwrap();
            assert_eq!(cache.retire_session_screenshots("bridge-retired"), 1);
            cache.clear();
            assert_eq!(
                cache
                    .try_publish_for_session(
                        pid,
                        702,
                        Payload(vec![2]),
                        Some("bridge-retired"),
                        Some(1.0)
                    )
                    .map(|entry| entry.map(|(id, _)| id)),
                Ok(None)
            );
            assert_eq!(
                cache
                    .screenshot_context(pid, Some(702), Some("bridge-retired"))
                    .map(|context| Some(context.scale))
                    .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                Err(ScreenshotContextError::ReplacedOrUnavailable)
            );
        });
    }

    #[test]
    fn retirement_during_payload_preparation_refuses_late_publication() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            struct PausedPayload(Arc<std::sync::Barrier>);
            impl SnapshotPayload for PausedPayload {
                type Element = ();
                fn len(&self) -> usize {
                    self.0.wait();
                    self.0.wait();
                    0
                }
                fn retain(&self, _: usize) -> Option<()> {
                    None
                }
            }
            let cache = Arc::new(SnapshotStore::new());
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let worker_cache = cache.clone();
            let worker_barrier = barrier.clone();
            let pid = std::process::id() as i32;
            let runtime_scope = current_runtime_scope();
            let worker = std::thread::spawn(move || {
                crate::tool::with_runtime_scope(runtime_scope, || {
                    worker_cache
                        .try_publish_for_session(
                            pid,
                            703,
                            PausedPayload(worker_barrier),
                            Some("bridge-race"),
                            Some(1.0),
                        )
                        .map(|entry| entry.map(|(id, _)| id))
                })
            });
            barrier.wait();
            cache.retire_session_screenshots("bridge-race");
            barrier.wait();
            assert_eq!(worker.join().unwrap(), Err(RegistryError::NotCurrent));
            assert_eq!(
                cache
                    .screenshot_context(pid, Some(703), Some("bridge-race"))
                    .map(|context| Some(context.scale))
                    .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                Err(ScreenshotContextError::ReplacedOrUnavailable)
            );
        });
    }

    #[test]
    fn metadata_rejects_other_runtime_and_retained_cache_after_shutdown() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            crate::tool::with_runtime_scope("bridge-runtime".into(), || {
                let cache = Arc::new(SnapshotStore::new());
                register_runtime_store(&cache);
                let other = Arc::new(SnapshotStore::<Payload>::new());
                register_runtime_store(&other);
                let pid = std::process::id() as i32;
                cache
                    .try_publish_for_session(pid, 704, Payload(vec![]), Some("owner"), Some(1.0))
                    .map(|entry| entry.map(|(id, _)| id))
                    .unwrap();
                crate::tool::with_runtime_scope("bridge-other-runtime".into(), || {
                    assert_eq!(
                        cache
                            .screenshot_context(pid, Some(704), Some("owner"))
                            .map(|context| Some(context.scale))
                            .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                        Err(ScreenshotContextError::ReplacedOrUnavailable)
                    );
                    assert_eq!(
                        cache.try_publish(pid, 704, Payload(vec![])),
                        Err(RegistryError::NotCurrent)
                    );
                });
                retire_runtime_scope("bridge-runtime");
                assert_eq!(
                    cache.try_publish(pid, 704, Payload(vec![])),
                    Err(RegistryError::NotCurrent)
                );
                assert_eq!(
                    other.try_publish(pid, 705, Payload(vec![])),
                    Err(RegistryError::NotCurrent)
                );
                assert_eq!(
                    cache
                        .screenshot_context(pid, Some(704), Some("owner"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable)
                );
            });
        });
    }

    #[test]
    fn publish_then_resolve_returns_projection() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let id = cache.publish(std::process::id() as i32, 7, Payload(vec![10, 20, 30]));
            let result = cache
            .resolve(std::process::id() as i32, &serde_json::json!({"element_token": &token_for(id, 2), "window_id": Option::<u64>::None}))
            .unwrap();
            assert!(matches!(
                result,
                ResolvedElement::Element { element, .. } if *element == 30
            ));
        });
    }

    #[test]
    fn miss_returns_refusal() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::<Payload>::new();
            assert!(cache
            .resolve(std::process::id() as i32, &serde_json::json!({"element_token": &token_for(0, 0), "window_id": Option::<u64>::None}))
            .is_err());
        });
    }

    #[test]
    fn membership_matches_payload_length() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let id = cache.publish(std::process::id() as i32, 99, Payload(vec![1, 2, 3, 4, 5]));
            for index in 0..5 {
                assert!(cache
                .resolve(std::process::id() as i32, &serde_json::json!({"element_token": &token_for(id, index), "window_id": Option::<u64>::None}))
                .is_ok());
            }
            assert!(cache
            .resolve(std::process::id() as i32, &serde_json::json!({"element_token": &token_for(id, 5), "window_id": Option::<u64>::None}))
            .is_err());
        });
    }

    #[test]
    fn screenshot_coordinates_never_borrow_another_sessions_latest_transform() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(7.35),
                    )
                    .map(|(id, _)| id);
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), Some("client-a"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Ok(Some(7.35))
                );

                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-b"),
                        Some(1.0),
                    )
                    .map(|(id, _)| id);
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), Some("client-b"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Ok(Some(1.0))
                );
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), Some("client-a"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable)
                );
                let refusal = cache
                    .screenshot_scale(
                        std::process::id() as i32,
                        Some(20),
                        &serde_json::json!({"_session_id":"client-a"}),
                    )
                    .expect_err("stale image coordinates must be refused");
                assert_eq!(
                    refusal.structured_content.as_ref().unwrap()["code"],
                    "screenshot_context_missing"
                );
            });
        });
    }

    #[test]
    fn screenshot_transforms_are_independent_across_windows() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(7.35),
                    )
                    .map(|(id, _)| id);
                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        21,
                        Payload(vec![]),
                        Some("client-b"),
                        Some(2.0),
                    )
                    .map(|(id, _)| id);
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), Some("client-a"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Ok(Some(7.35))
                );
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(21), Some("client-b"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Ok(Some(2.0))
                );
            });
        });
    }

    #[test]
    fn same_session_latest_snapshot_replaces_or_refuses_older_image_context() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(7.35),
                    )
                    .map(|(id, _)| id);
                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(1.0),
                    )
                    .map(|(id, _)| id);
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), Some("client-a"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Ok(Some(1.0)),
                    "a newer native capture replaces the older resized frame"
                );

                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        None,
                    )
                    .map(|(id, _)| id);
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), Some("client-a"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable),
                    "a newer tree-only observation retires the older image frame"
                );
            });
        });
    }

    #[test]
    fn recreating_an_idle_reclaimed_implicit_session_keeps_its_old_tokens_stale() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                use crate::session::{
                    begin_session_dispatch, evict_idle_with_prefix,
                    register_scoped_session_end_hook, SessionClientKind, SessionTransport,
                };
                let session = format!("element-cache-idle-implicit-{}", std::process::id());
                let cache = Arc::new(SnapshotStore::<Payload>::new());
                let retiring = cache.clone();
                // End hooks are process-global: another test's session end must not
                // retire this store's screenshots and reject its publications.
                let own_session = session.clone();
                let _hook = register_scoped_session_end_hook(move |ended| {
                    if ended == own_session.as_str() {
                        retiring.retire_session_screenshots(ended);
                    }
                });
                let begin = || {
                    begin_session_dispatch(
                        &session,
                        None,
                        &session,
                        true,
                        SessionTransport::McpStdio,
                        SessionClientKind::Mcp,
                    )
                };

                let guard = begin().expect("first unnamed call starts the session");
                let snapshot = cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![0, 1]),
                        Some(&session),
                        Some(1.0),
                    )
                    .map(|(id, _)| id)
                    .expect("live session publishes");
                let token = token_for(snapshot, 1);
                drop(guard);
                assert_eq!(
                    evict_idle_with_prefix(std::time::Duration::ZERO, &session),
                    [session.clone()]
                );

                let guard = begin().expect("next unnamed call recreates the session");
                assert_eq!(
                    resolve(&cache, std::process::id() as i32, &token),
                    Err(STALE_TOKEN_ERROR.into())
                );
                let fresh = cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![0, 1]),
                        Some(&session),
                        Some(1.0),
                    )
                    .map(|(id, _)| id)
                    .expect("recreated session publishes again");
                assert_eq!(
                    resolve(&cache, std::process::id() as i32, &token_for(fresh, 1)),
                    Ok((20, 1))
                );
                drop(guard);
                crate::session::end_session(&session);
                crate::session::revive_session(&session);
            });
        });
    }

    #[test]
    fn screenshot_reads_take_session_tombstones_before_the_store_mutex() {
        // Revival runs its hooks while holding the session tombstone lock, and
        // this store's hook then takes the store mutex. A screenshot read that
        // waits for the tombstone lock must not already hold the store mutex,
        // or the two deadlock.
        let scope = format!("snapshot-test-{}", uuid::Uuid::new_v4());
        crate::tool::with_runtime_scope(scope.clone(), || {
            use crate::session::{
                begin_session_dispatch, fire_session_revive_for_owner,
                register_scoped_session_revive_hook, SessionClientKind, SessionTransport,
            };
            use std::sync::atomic::AtomicBool;
            use std::time::{Duration, Instant};

            let session = format!("snapshot-lock-order-{}", uuid::Uuid::new_v4());
            let pid = std::process::id() as i32;
            let mut store = SnapshotStore::<Payload>::new();
            // Swap out this store's own revive hook so an inversion is reported
            // below instead of deadlocking the test.
            store._revive_hook = register_scoped_session_revive_hook(|_| {});
            let store = Arc::new(store);
            let _guard = begin_session_dispatch(
                &session,
                None,
                &session,
                true,
                SessionTransport::McpStdio,
                SessionClientKind::Mcp,
            )
            .expect("session starts");
            store
                .publish_for_session(pid, 20, Payload(vec![0]), Some(&session), Some(1.0))
                .expect("live session publishes");

            let inverted = Arc::new(AtomicBool::new(false));
            let reader = Arc::new(Mutex::new(None));
            let _probe = {
                let (store, session, inverted, reader) = (
                    store.clone(),
                    session.clone(),
                    inverted.clone(),
                    reader.clone(),
                );
                register_scoped_session_revive_hook(move |revived| {
                    if revived != session {
                        return;
                    }
                    // The tombstone lock is held here: start a read and watch
                    // whether it holds the store mutex while it waits.
                    let handle = {
                        let (store, session, scope) =
                            (store.clone(), session.clone(), scope.clone());
                        std::thread::spawn(move || {
                            crate::tool::with_runtime_scope(scope, || {
                                store.screenshot_context(pid, Some(20), Some(&session))
                            })
                        })
                    };
                    let deadline = Instant::now() + Duration::from_millis(500);
                    while Instant::now() < deadline && !handle.is_finished() {
                        if store.inner.try_lock().is_err() {
                            inverted.store(true, Ordering::SeqCst);
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    *reader.lock().unwrap() = Some(handle);
                })
            };

            assert!(fire_session_revive_for_owner(&session, &session));
            let handle = reader.lock().unwrap().take().expect("probe ran");
            let context = handle.join().expect("reader finishes once revival returns");
            assert!(
                !inverted.load(Ordering::SeqCst),
                "screenshot read held the store mutex while waiting for session state"
            );
            assert_eq!(context.expect("live session context").scale, 1.0);
        });
    }

    #[test]
    fn session_retirement_removes_only_snapshots_owned_by_that_session() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("ending"),
                        Some(7.35),
                    )
                    .map(|(id, _)| id);
                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        21,
                        Payload(vec![]),
                        Some("survivor"),
                        Some(2.0),
                    )
                    .map(|(id, _)| id);
                assert_eq!(cache.retire_session_screenshots("ending"), 1);
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), Some("ending"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable)
                );
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(21), Some("survivor"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Ok(Some(2.0))
                );
            });
        });
    }

    #[test]
    fn retired_screenshot_refuses_cross_session_and_anonymous_replay_until_republished() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("ending"),
                        Some(7.35),
                    )
                    .map(|(id, _)| id);
                assert_eq!(cache.retire_session_screenshots("ending"), 1);

                for session in [Some("ending"), Some("other"), None] {
                    assert_eq!(
                        cache
                            .screenshot_context(std::process::id() as i32, Some(20), session)
                            .map(|context| Some(context.scale))
                            .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                        Err(ScreenshotContextError::ReplacedOrUnavailable),
                        "a retired screenshot must not become native-pixel fallback"
                    );
                }
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, None, Some("other"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable)
                );

                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("other"),
                        Some(2.0),
                    )
                    .map(|(id, _)| id);
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), Some("other"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Ok(Some(2.0)),
                    "a fresh snapshot clears the lightweight retirement tombstone"
                );
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), None)
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable)
                );

                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        None,
                        Some(1.5),
                    )
                    .map(|(id, _)| id);
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), None)
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Ok(Some(1.5)),
                    "a fresh anonymous snapshot also recovers the coordinate context"
                );
            });
        });
    }

    #[test]
    fn lru_eviction_retires_only_the_evicted_screenshot_key() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                for window_id in 0..LRU_CAP_PER_PID as u64 {
                    cache
                        .publish_for_session(
                            std::process::id() as i32,
                            window_id,
                            Payload(vec![]),
                            Some("client-a"),
                            Some(2.0),
                        )
                        .map(|(id, _)| id);
                }
                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        LRU_CAP_PER_PID as u64,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(2.0),
                    )
                    .map(|(id, _)| id);

                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(0), Some("client-a"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable),
                    "evicting an observed window must not restore native-pixel fallback"
                );
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(1), Some("client-a"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Ok(Some(2.0))
                );
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(9999), Some("client-a"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable),
                    "a never-observed key keeps the legacy fallback"
                );
            });
        });
    }

    #[test]
    fn explicit_remove_retires_only_a_snapshot_that_existed() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(2.0),
                    )
                    .map(|(id, _)| id);
                cache.remove(std::process::id() as i32, 20);

                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), Some("client-a"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable),
                    "removing an observed window must not restore native-pixel fallback"
                );
                cache.remove(std::process::id() as i32, 21);
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(21), Some("client-a"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable),
                    "removing an absent key must not retire a never-observed window"
                );
            });
        });
    }

    #[test]
    fn capture_completing_after_session_end_is_not_published() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                let session = format!("snapshot-late-capture-{}", uuid::Uuid::new_v4());
                assert!(crate::session::fire_session_end(&session));
                assert_eq!(
                    cache
                        .publish_for_session(
                            std::process::id() as i32,
                            20,
                            Payload(vec![]),
                            Some(&session),
                            Some(7.35)
                        )
                        .map(|(id, _)| id),
                    None
                );
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), Some(&session))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable)
                );
            });
        });
    }

    #[test]
    fn no_snapshot_refuses_unproven_window_pixels() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::<Payload>::new();
                assert_eq!(
                    cache
                        .screenshot_context(std::process::id() as i32, Some(20), Some("client-a"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable)
                );
            });
        });
    }

    #[test]
    fn retired_screenshot_index_has_deterministic_fixed_capacity() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let mut state = SnapshotStoreState::default();
            let expected = (0..RETIRED_SCREENSHOT_CAPACITY)
                .map(|index| (1000 + index as i32, 2000 + index as u64))
                .collect::<Vec<_>>();
            for key in &expected {
                state.retire_screenshot(*key);
            }

            assert_eq!(state.retired_screenshots.len(), RETIRED_SCREENSHOT_CAPACITY);
            assert_eq!(
                state
                    .retired_screenshot_order
                    .iter()
                    .copied()
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(!state.retired_screenshot_overflowed);

            let order_before_duplicate = state.retired_screenshot_order.clone();
            state.retire_screenshot(expected[0]);
            assert_eq!(state.retired_screenshot_order, order_before_duplicate);

            state.retire_screenshot((9999, 9999));
            assert!(state.retired_screenshot_overflowed);
            assert_eq!(state.retired_screenshots.len(), RETIRED_SCREENSHOT_CAPACITY);
            assert_eq!(state.retired_screenshot_order, order_before_duplicate);
            assert!(!state.retired_screenshots.contains(&(9999, 9999)));
        });
    }

    fn overflow_retired_screenshots(cache: &SnapshotStore<Payload>) -> (i32, u64) {
        let pid = std::process::id() as i32;
        let mut inner = cache.inner.lock().unwrap();
        for index in 0..=RETIRED_SCREENSHOT_CAPACITY {
            inner.retire_screenshot((pid, 2000 + index as u64));
        }
        (pid, 2000 + RETIRED_SCREENSHOT_CAPACITY as u64)
    }

    #[test]
    fn retired_screenshot_overflow_refuses_unrecorded_and_unseen_replay() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                let overflow_key = overflow_retired_screenshots(&cache);

                let inner = cache.inner.lock().unwrap();
                assert!(inner.retired_screenshot_overflowed);
                assert_eq!(inner.retired_screenshots.len(), RETIRED_SCREENSHOT_CAPACITY);
                assert_eq!(
                    inner.retired_screenshot_order.len(),
                    RETIRED_SCREENSHOT_CAPACITY
                );
                assert!(!inner.retired_screenshots.contains(&overflow_key));
                drop(inner);

                assert_eq!(
                    cache
                        .screenshot_context(overflow_key.0, Some(overflow_key.1), None)
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable)
                );
                assert_eq!(
                    cache
                        .screenshot_context(9999, Some(9999), None)
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable)
                );
            });
        });
    }

    #[test]
    fn fresh_snapshot_resolves_while_retirement_index_is_overflowed() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                let overflow_key = overflow_retired_screenshots(&cache);

                cache
                    .publish_for_session(
                        overflow_key.0,
                        overflow_key.1,
                        Payload(vec![]),
                        Some("fresh"),
                        Some(3.0),
                    )
                    .map(|(id, _)| id);
                assert_eq!(
                    cache
                        .screenshot_context(overflow_key.0, Some(overflow_key.1), Some("fresh"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Ok(Some(3.0))
                );
                assert_eq!(
                    cache
                        .screenshot_context(overflow_key.0, Some(overflow_key.1), Some("other"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable)
                );
                assert_eq!(
                    cache
                        .screenshot_context(9999, Some(9999), Some("fresh"))
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable)
                );
            });
        });
    }

    #[test]
    fn clear_resets_retired_screenshot_overflow() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                overflow_retired_screenshots(&cache);
                cache.clear();

                let inner = cache.inner.lock().unwrap();
                assert!(inner.retired_screenshots.is_empty());
                assert!(inner.retired_screenshot_order.is_empty());
                assert!(!inner.retired_screenshot_overflowed);
                drop(inner);
                assert_eq!(
                    cache
                        .screenshot_context(9999, Some(9999), None)
                        .map(|context| Some(context.scale))
                        .map_err(|_| ScreenshotContextError::ReplacedOrUnavailable),
                    Err(ScreenshotContextError::ReplacedOrUnavailable)
                );
            });
        });
    }

    #[test]
    fn zoom_context_is_bound_to_snapshot_session_and_window() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                let snapshot = cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(7.35),
                    )
                    .map(|(id, _)| id)
                    .unwrap();
                let context = ZoomContext {
                    screenshot: ScreenshotContext {
                        frame_revision: 1,
                        snapshot_id: snapshot,
                        window_id: 20,
                        scale: 7.35,
                    },
                    origin_x: 100.0,
                    origin_y: 50.0,
                    scale_inv: 2.0,
                };
                cache
                    .set_zoom(std::process::id() as i32, Some("client-a"), context)
                    .unwrap();

                assert_eq!(
                    cache
                        .zoom(std::process::id() as i32, Some(20), Some("client-a"))
                        .unwrap(),
                    context
                );
                assert_eq!(context.zoom_to_window(3.0, 4.0), (106.0, 58.0));
                assert_eq!(
                    cache
                        .zoom(std::process::id() as i32, Some(20), Some("client-b"))
                        .unwrap_err()
                        .structured_content
                        .as_ref()
                        .unwrap()["code"],
                    "zoom_context_missing"
                );
                assert_eq!(
                    cache
                        .zoom(std::process::id() as i32, Some(21), Some("client-a"))
                        .unwrap_err()
                        .structured_content
                        .as_ref()
                        .unwrap()["code"],
                    "zoom_context_missing"
                );
            });
        });
    }

    #[test]
    fn window_only_screenshot_lookup_requires_one_current_owned_snapshot() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                let mut other_process = super::tests::LiveProcess::new();
                let first = cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(2.0),
                    )
                    .map(|(id, _)| id)
                    .unwrap();
                assert_eq!(
                    cache
                        .screenshot_context_for_zoom(None, 20, Some("client-a"))
                        .unwrap(),
                    (
                        std::process::id() as i32,
                        ScreenshotContext {
                            frame_revision: 1,
                            snapshot_id: first,
                            window_id: 20,
                            scale: 2.0,
                        }
                    )
                );
                assert_eq!(
                    cache
                        .screenshot_context_for_zoom(
                            Some(std::process::id() as i32),
                            20,
                            Some("client-a")
                        )
                        .unwrap()
                        .0,
                    std::process::id() as i32
                );
                assert!(cache
                    .screenshot_context_for_zoom(None, 20, Some("client-b"))
                    .is_err());

                cache
                    .publish_for_session(
                        other_process.pid(),
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(1.0),
                    )
                    .map(|(id, _)| id);
                assert!(cache
                    .screenshot_context_for_zoom(None, 20, Some("client-a"))
                    .is_err());
            });
        });
    }

    #[test]
    fn late_zoom_completion_cannot_replace_newer_valid_context() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                let snapshot_a = cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(2.0),
                    )
                    .map(|(id, _)| id)
                    .unwrap();
                let slow_a = ZoomContext {
                    screenshot: ScreenshotContext {
                        frame_revision: 1,
                        snapshot_id: snapshot_a,
                        window_id: 20,
                        scale: 2.0,
                    },
                    origin_x: 10.0,
                    origin_y: 20.0,
                    scale_inv: 2.0,
                };

                let snapshot_b = cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(1.0),
                    )
                    .map(|(id, _)| id)
                    .unwrap();
                let valid_b = ZoomContext {
                    screenshot: ScreenshotContext {
                        frame_revision: 1,
                        snapshot_id: snapshot_b,
                        window_id: 20,
                        scale: 1.0,
                    },
                    origin_x: 30.0,
                    origin_y: 40.0,
                    scale_inv: 1.0,
                };
                cache
                    .set_zoom(std::process::id() as i32, Some("client-a"), valid_b)
                    .unwrap();
                assert!(cache
                    .set_zoom(std::process::id() as i32, Some("client-a"), slow_a)
                    .is_err());
                assert_eq!(
                    cache
                        .zoom(std::process::id() as i32, Some(20), Some("client-a"))
                        .unwrap(),
                    valid_b
                );
            });
        });
    }

    #[test]
    fn newer_snapshot_retires_zoom_for_click_drag_and_held_pointer_coordinates() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                let snapshot = cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(7.35),
                    )
                    .map(|(id, _)| id)
                    .unwrap();
                let context = ZoomContext {
                    screenshot: ScreenshotContext {
                        frame_revision: 1,
                        snapshot_id: snapshot,
                        window_id: 20,
                        scale: 7.35,
                    },
                    origin_x: 100.0,
                    origin_y: 50.0,
                    scale_inv: 2.0,
                };
                cache
                    .set_zoom(std::process::id() as i32, Some("client-a"), context)
                    .unwrap();

                let click = context.zoom_to_window(1.0, 2.0);
                let drag_from = context.zoom_to_window(3.0, 4.0);
                let held_pointer_to = context.zoom_to_window(5.0, 6.0);
                assert_eq!(
                    (click, drag_from, held_pointer_to),
                    ((102.0, 54.0), (106.0, 58.0), (110.0, 62.0))
                );

                let replacement = cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-b"),
                        Some(1.0),
                    )
                    .map(|(id, _)| id)
                    .unwrap();
                assert!(
                    (cache.window_for_snapshot(std::process::id() as i32, replacement) == Some(20))
                );
                let refusal = cache
                    .zoom(std::process::id() as i32, Some(20), Some("client-a"))
                    .expect_err("all uses of the old zoom image must become stale together");
                assert_eq!(
                    refusal.structured_content.as_ref().unwrap()["code"],
                    "zoom_context_missing"
                );
            });
        });
    }

    #[test]
    fn zoom_context_retires_on_same_session_replacement_and_session_end() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            isolated(|| {
                let cache = SnapshotStore::new();
                let snapshot = cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(7.35),
                    )
                    .map(|(id, _)| id)
                    .unwrap();
                cache
                    .set_zoom(
                        std::process::id() as i32,
                        Some("client-a"),
                        ZoomContext {
                            screenshot: ScreenshotContext {
                                frame_revision: 1,
                                snapshot_id: snapshot,
                                window_id: 20,
                                scale: 7.35,
                            },
                            origin_x: 0.0,
                            origin_y: 0.0,
                            scale_inv: 1.0,
                        },
                    )
                    .unwrap();

                let replacement = cache
                    .publish_for_session(
                        std::process::id() as i32,
                        20,
                        Payload(vec![]),
                        Some("client-a"),
                        Some(1.0),
                    )
                    .map(|(id, _)| id)
                    .unwrap();
                assert!(
                    (cache.window_for_snapshot(std::process::id() as i32, replacement) == Some(20))
                );
                assert!(cache
                    .zoom(std::process::id() as i32, Some(20), Some("client-a"))
                    .is_err());

                let latest = cache
                    .screenshot_context(std::process::id() as i32, Some(20), Some("client-a"))
                    .unwrap();
                cache
                    .set_zoom(
                        std::process::id() as i32,
                        Some("client-a"),
                        ZoomContext {
                            screenshot: latest,
                            origin_x: 0.0,
                            origin_y: 0.0,
                            scale_inv: 1.0,
                        },
                    )
                    .unwrap();
                assert_eq!(cache.retire_session_screenshots("client-a"), 1);
                assert!(cache
                    .zoom(std::process::id() as i32, Some(20), Some("client-a"))
                    .is_err());
            });
        });
    }

    struct DropCounter {
        owner: Weak<SnapshotStore<DropCounter>>,
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
                // Another thread (a session revive hook fanning out over every
                // cache) may hold the lock briefly; only a lock that stays
                // held, as it would by this very thread, is a violation.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while owner.inner.try_lock().is_err() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "native cleanup ran under the storage lock"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn replacement_remove_and_clear_run_drop_outside_lock() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = Arc::new(SnapshotStore::new());
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
        });
    }

    #[test]
    fn window_identity_preserves_high_bits_through_resolution_and_retirement() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let low = 7;
            let high = (1_u64 << 32) | low;
            let first = cache.publish(std::process::id() as i32, low, Payload(vec![10]));
            let second = cache.publish(std::process::id() as i32, high, Payload(vec![20]));
            let token = token_for(second, 0);
            let resolved = cache
                .resolve(
                    std::process::id() as i32,
                    &serde_json::json!({"element_token": &token, "window_id": high}),
                )
                .unwrap();
            assert!(
                matches!(resolved, ResolvedElement::Element { window_id: window, element, .. } if window == high && *element == 20)
            );
            assert!(cache
                .resolve(
                    std::process::id() as i32,
                    &serde_json::json!({"element_token": &token, "window_id": low})
                )
                .is_err());
            let handle = format!("s{second:08x}");
            assert!(cache
            .resolve(std::process::id() as i32, &serde_json::json!({"element_token": &format!("{}:{}", &handle, 0), "window_id": high}))
            .is_ok());
            assert!(cache
            .resolve(std::process::id() as i32, &serde_json::json!({"element_token": &format!("{}:{}", &handle, 0), "window_id": low}))
            .is_err());
            cache.remove(std::process::id() as i32, high);
            assert!(cache
                .resolve(
                    std::process::id() as i32,
                    &serde_json::json!({"element_token": &token, "window_id": Option::<u64>::None})
                )
                .is_err());
            assert!(cache
                .resolve(
                    std::process::id() as i32,
                    &serde_json::json!({"element_token": &token_for(first, 0), "window_id": low})
                )
                .is_ok());
        });
    }

    #[test]
    fn bindings_sharing_a_scope_keep_independent_payload_ownership() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            // The fork keeps a second copy of this test; a shared scope name would
            // let one copy retire the other's runtime stores mid-test.
            let scope = format!("snapshot-binding-ownership-{}", uuid::Uuid::new_v4());
            crate::tool::with_runtime_scope(scope.clone(), || {
                let first = Arc::new(SnapshotStore::new());
                let second = Arc::new(SnapshotStore::new());
                register_runtime_store(&first);
                register_runtime_store(&second);
                let first_id = first.publish(std::process::id() as i32, 7, Payload(vec![10]));
                let second_id = second.publish(std::process::id() as i32, 7, Payload(vec![20]));
                assert!(second
                .resolve(std::process::id() as i32, &serde_json::json!({"element_token": &token_for(first_id, 0), "window_id": Option::<u64>::None}))
                .is_err());
                drop(first);
                let resolved = second
                .resolve(std::process::id() as i32, &serde_json::json!({"element_token": &token_for(second_id, 0), "window_id": Option::<u64>::None}))
                .unwrap();
                assert!(matches!(
                    resolved,
                    ResolvedElement::Element { element, .. } if *element == 20
                ));
                assert!(Arc::ptr_eq(
                    &current_runtime_store::<Payload>().unwrap(),
                    &second
                ));
                retire_runtime_scope(&scope);
            });
        });
    }

    #[test]
    fn recording_discovery_does_not_extend_payload_lifetime() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            // The fork keeps a second copy of this test; a shared scope name would
            // let one copy retire the other's runtime stores mid-test.
            let scope = format!("snapshot-weak-discovery-{}", uuid::Uuid::new_v4());
            crate::tool::with_runtime_scope(scope.clone(), || {
                let cache = Arc::new(SnapshotStore::new());
                let drops = Arc::new(AtomicUsize::new(0));
                cache.publish(
                    std::process::id() as i32,
                    7,
                    DropCounter {
                        owner: Arc::downgrade(&cache),
                        drops: drops.clone(),
                    },
                );
                register_runtime_store(&cache);
                assert_eq!(Arc::strong_count(&cache), 1);
                drop(cache);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert!(current_runtime_store::<DropCounter>().is_none());
                {
                    let caches = runtime_stores().lock().unwrap();
                    assert!(!caches.contains_key(scope.as_str()));
                    if caches.is_empty() {
                        assert_eq!(caches.capacity(), 0);
                    }
                }
                assert_eq!(retire_runtime_scope(&scope), 0);
            });
        });
    }

    #[test]
    fn default_impl_matches_new() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let _cache: SnapshotStore<Payload> = SnapshotStore::default();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element_token::{format_snapshot_id, token_for, LRU_CAP_PER_PID};
    use crate::snapshot_test_support::Payload;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Re-execute only this test to own a real, distinct process on every platform.
    // The environment marker is confined to the child test binary, not production.
    #[test]
    fn live_process_fixture() {
        if std::env::var_os("CUA_SNAPSHOT_STORE_TEST_CHILD").is_some() {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    pub(super) struct LiveProcess(std::process::Child);

    impl LiveProcess {
        pub(super) fn new() -> Self {
            let child = Self(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "snapshot_store::tests::live_process_fixture"])
                    .env("CUA_SNAPSHOT_STORE_TEST_CHILD", "1")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn snapshot identity fixture"),
            );
            assert_ne!(child.0.id(), std::process::id());
            child
        }

        pub(super) fn pid(&mut self) -> i32 {
            assert!(self.0.try_wait().expect("query identity fixture").is_none());
            i32::try_from(self.0.id()).expect("fixture PID fits registry PID")
        }
    }

    impl Drop for LiveProcess {
        fn drop(&mut self) {
            // Kill and reap even during assertion unwinding. The child also has a
            // bounded lifetime if the parent itself is terminated before Drop.
            let _ = self.0.kill();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match self.0.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    result => {
                        panic!("identity fixture did not reap within five seconds: {result:?}")
                    }
                }
            }
        }
    }

    #[test]
    fn pid_for_token_finds_the_owning_process() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let mut other_process = super::tests::LiveProcess::new();
            let pid = std::process::id() as i32;
            let id = cache.publish(pid, 7, Payload(vec![10, 20]));
            cache.publish(other_process.pid(), 8, Payload(vec![1]));
            let args = serde_json::json!({ "element_token": token_for(id, 1) });
            assert_eq!(cache.pid_for_token(&args), Some(pid));
            assert_eq!(cache.pid_for_token(&serde_json::json!({})), None);
            let unknown = serde_json::json!({ "element_token": token_for(id + 1000, 0) });
            assert_eq!(cache.pid_for_token(&unknown), None);
            crate::tool::with_runtime_scope(
                format!("snapshot-other-{}", uuid::Uuid::new_v4()),
                || {
                    assert_eq!(cache.pid_for_token(&args), None);
                },
            );
            // A different store can replace the registry generation while this
            // store still contains its old metadata. Never route on that row.
            let other = SnapshotStore::new();
            other.publish(pid, 7, Payload(vec![30]));
            assert_eq!(cache.pid_for_token(&args), None);
        });
    }

    #[test]
    fn pid_for_token_refuses_retired_session() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let pid = std::process::id() as i32;
            let (id, _) = cache
                .try_publish_for_session(pid, 7, Payload(vec![10]), Some("owner"), None)
                .unwrap()
                .unwrap();
            let args = serde_json::json!({ "element_token": token_for(id, 0) });
            assert_eq!(cache.pid_for_token(&args), Some(pid));
            cache.retire_session_screenshots("owner");
            assert_eq!(cache.pid_for_token(&args), None);
            assert!(cache.resolve(pid, &args).is_err());
        });
    }

    #[test]
    fn publish_then_resolve_returns_projection() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let id = cache.publish(std::process::id() as i32, 7, Payload(vec![10, 20, 30]));
            let result = cache
                .resolve(
                    std::process::id() as i32,
                    &serde_json::json!({ "element_token": token_for(id, 2) }),
                )
                .unwrap();
            assert!(matches!(
                result,
                ResolvedElement::Element { element, .. } if *element == 30
            ));
        });
    }

    #[test]
    fn miss_returns_refusal() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::<Payload>::new();
            assert!(cache
                .resolve(
                    std::process::id() as i32,
                    &serde_json::json!({ "element_token": token_for(0, 0) })
                )
                .is_err());
        });
    }

    #[test]
    fn token_resolution_rejects_conflicting_windows() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let token = token_for(
                cache.publish(std::process::id() as i32, 42, Payload(vec![1])),
                0,
            );
            let mut args = serde_json::json!({ "element_token": token });
            for window_id in [None, Some(42)] {
                if let Some(window_id) = window_id {
                    args["window_id"] = serde_json::json!(window_id);
                }
                assert!(matches!(
                    cache.resolve(std::process::id() as i32, &args).unwrap(),
                    ResolvedElement::Element { window_id: 42, .. }
                ));
            }
            args["window_id"] = serde_json::json!(99);
            let error = cache.resolve(std::process::id() as i32, &args).unwrap_err();
            assert_eq!(
                error.structured_content.unwrap()["refusal"]["code"],
                "conflicting_element_target"
            );
        });
    }

    #[test]
    fn membership_matches_payload_length() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let id = cache.publish(std::process::id() as i32, 99, Payload(vec![1, 2, 3, 4, 5]));
            for index in 0..5 {
                assert!(cache
                    .resolve(
                        std::process::id() as i32,
                        &serde_json::json!({ "element_token": token_for(id, index) })
                    )
                    .is_ok());
            }
            assert!(cache
                .resolve(
                    std::process::id() as i32,
                    &serde_json::json!({ "element_token": token_for(id, 5) })
                )
                .is_err());
        });
    }

    #[test]
    fn semantic_membership_ignores_capture_only_publication() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let mut other_process = super::tests::LiveProcess::new();
            assert!(!cache.contains_window(std::process::id() as i32, 99));
            assert!(!cache.contains_semantic_window(std::process::id() as i32, 99));

            cache
                .publish_capture_for_session(
                    std::process::id() as i32,
                    99,
                    Payload(vec![]),
                    None,
                    Some(1.0),
                )
                .unwrap();
            assert!(cache.contains_window(std::process::id() as i32, 99));
            assert!(!cache.contains_semantic_window(std::process::id() as i32, 99));

            cache.publish(std::process::id() as i32, 99, Payload(vec![]));
            assert!(cache.contains_semantic_window(std::process::id() as i32, 99));
            assert!(!cache.contains_semantic_window(std::process::id() as i32, 100));
            assert!(!cache.contains_semantic_window(other_process.pid(), 99));

            cache.remove(std::process::id() as i32, 99);
            assert!(!cache.contains_window(std::process::id() as i32, 99));
            assert!(!cache.contains_semantic_window(std::process::id() as i32, 99));
        });
    }

    fn token_refusal(cache: &SnapshotStore<Payload>, pid: i32, token: &str) -> serde_json::Value {
        cache
            .resolve(pid, &serde_json::json!({ "element_token": token }))
            .unwrap_err()
            .structured_content
            .unwrap()
    }

    #[test]
    fn malformed_token_is_invalid_not_stale() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            cache.publish(std::process::id() as i32, 1, Payload(vec![0]));
            assert_eq!(
                token_refusal(&cache, std::process::id() as i32, "garbage")["refusal"]["code"],
                "invalid_element_token"
            );
        });
    }

    #[test]
    fn tokens_in_different_pids_dont_collide() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let mut other_process = super::tests::LiveProcess::new();
            let first = cache.publish(std::process::id() as i32, 11, Payload(vec![0]));
            cache.publish(other_process.pid(), 22, Payload(vec![0]));
            assert_eq!(
                token_refusal(&cache, other_process.pid(), &token_for(first, 0))["refusal"]["code"],
                "stale_element_token"
            );
        });
    }

    #[test]
    fn stale_token_names_the_current_snapshots() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let current = cache.publish(std::process::id() as i32, 555, Payload(vec![0]));
            let structured =
                token_refusal(&cache, std::process::id() as i32, &token_for(0xdead, 0));
            assert_eq!(structured["refusal"]["code"], "stale_element_token");
            assert_eq!(
                structured["current_snapshots"],
                serde_json::json!([{ "snapshot_id": format_snapshot_id(current), "window_id": 555 }])
            );
        });
    }

    #[test]
    fn stale_zoom_is_an_explicit_refusal_with_no_delivery() {
        let cache = SnapshotStore::<Payload>::new();
        let structured = cache
            .zoom(7, Some(9), None)
            .unwrap_err()
            .structured_content
            .unwrap();
        // Legacy top-level fields remain for existing consumers.
        assert_eq!(structured["code"], "zoom_context_missing");
        assert_eq!(structured["pid"], 7);
        assert_eq!(structured["window_id"], 9);
        assert_eq!(structured["status"], "refused");
        assert_eq!(structured["refusal"]["code"], "zoom_context_missing");

        let args = serde_json::json!({
            "pid": 7, "window_id": 9, "x": 1, "y": 2, "from_zoom": true,
            "delivery_mode": "foreground",
        });
        let record =
            crate::action_record::ActionExecutionRecord::from_legacy("click", &args, &structured)
                .unwrap();
        assert_eq!(record.effect, crate::action_record::ActionEffect::Refused);
        assert_eq!(record.actual_delivery, None);
        assert_eq!(record.delivered_count, None);
        assert_eq!(record.refusal.unwrap().code, "zoom_context_missing");
    }

    #[test]
    fn clear_then_publish_starts_clean() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let first = cache.publish(std::process::id() as i32, 1, Payload(vec![0]));
            assert_eq!(cache.clear(), 1);
            assert_eq!(cache.clear(), 0);
            assert_eq!(
                token_refusal(&cache, std::process::id() as i32, &token_for(first, 0))["refusal"]
                    ["code"],
                "stale_element_token"
            );
            let second = cache.publish(std::process::id() as i32, 1, Payload(vec![0]));
            assert!(cache
                .resolve(
                    std::process::id() as i32,
                    &serde_json::json!({ "element_token": token_for(second, 0) })
                )
                .is_ok());
        });
    }

    #[test]
    fn missing_token_resolves_to_none() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            assert!(matches!(
                SnapshotStore::<Payload>::new()
                    .resolve(std::process::id() as i32, &serde_json::json!({}))
                    .unwrap(),
                ResolvedElement::None
            ));
        });
    }

    #[test]
    fn runtime_store_discovery_is_weak_and_shared_across_calls() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            crate::tool::with_runtime_scope("token-discovery-test".into(), || {
                let cache = Arc::new(SnapshotStore::<Payload>::new());
                register_runtime_store(&cache);
                assert!(Arc::ptr_eq(
                    &cache,
                    &current_runtime_store::<Payload>().unwrap()
                ));
                drop(cache);
                assert!(current_runtime_store::<Payload>().is_none());
            });
        });
    }

    fn refusal_code(result: ToolResult) -> String {
        result.structured_content.unwrap()["code"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn scale(cache: &SnapshotStore<Payload>, window_id: u64, session: &str) -> Option<f64> {
        cache
            .screenshot_context(std::process::id() as i32, Some(window_id), Some(session))
            .ok()
            .map(|context| context.scale)
    }

    fn zoom_on(snapshot: u32, scale: f64) -> ZoomContext {
        ZoomContext {
            screenshot: ScreenshotContext {
                frame_revision: 1,
                snapshot_id: snapshot,
                window_id: 20,
                scale,
            },
            origin_x: 100.0,
            origin_y: 50.0,
            scale_inv: 2.0,
        }
    }

    #[test]
    fn screenshot_coordinates_never_borrow_another_sessions_latest_transform() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            cache.publish_for_session(
                std::process::id() as i32,
                20,
                Payload(vec![]),
                Some("client-a"),
                Some(7.35),
            );
            assert_eq!(scale(&cache, 20, "client-a"), Some(7.35));

            cache.publish_for_session(
                std::process::id() as i32,
                20,
                Payload(vec![]),
                Some("client-b"),
                Some(1.0),
            );
            assert_eq!(scale(&cache, 20, "client-b"), Some(1.0));
            let refusal = cache
                .screenshot_context(std::process::id() as i32, Some(20), Some("client-a"))
                .expect_err("stale image coordinates must be refused");
            assert_eq!(refusal_code(refusal), "screenshot_context_missing");
        });
    }

    #[test]
    fn window_pixels_need_a_session_screenshot_unless_marked_native() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let session = |extra: serde_json::Value| {
                let mut args = serde_json::json!({ "_session_id": "client-a" });
                args.as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                args
            };
            let refusal = cache
                .screenshot_scale(
                    std::process::id() as i32,
                    Some(20),
                    &session(serde_json::json!({})),
                )
                .expect_err("pixels without a read are refused");
            assert_eq!(refusal_code(refusal), "screenshot_context_missing");
            let native = session(serde_json::json!({ NATIVE_WINDOW_PIXELS_ARG: true }));
            assert_eq!(
                cache
                    .screenshot_scale(std::process::id() as i32, Some(20), &native)
                    .unwrap(),
                1.0
            );

            // Native pixels ignore the session's screenshot scale; other calls use it.
            cache.publish_for_session(
                std::process::id() as i32,
                20,
                Payload(vec![]),
                Some("client-a"),
                Some(2.5),
            );
            assert_eq!(
                cache
                    .screenshot_scale(std::process::id() as i32, Some(20), &native)
                    .unwrap(),
                1.0
            );
            assert_eq!(
                cache
                    .screenshot_scale(
                        std::process::id() as i32,
                        Some(20),
                        &session(serde_json::json!({}))
                    )
                    .unwrap(),
                2.5
            );
            // Only the boolean true marks native pixels.
            let refusal = cache
                .screenshot_scale(
                    std::process::id() as i32,
                    Some(21),
                    &session(serde_json::json!({ NATIVE_WINDOW_PIXELS_ARG: "true" })),
                )
                .expect_err("a non-boolean marker is not native pixels");
            assert_eq!(refusal_code(refusal), "screenshot_context_missing");
        });
    }

    #[test]
    fn screenshot_transforms_are_independent_across_windows() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            cache.publish_for_session(
                std::process::id() as i32,
                20,
                Payload(vec![]),
                Some("client-a"),
                Some(7.35),
            );
            cache.publish_for_session(
                std::process::id() as i32,
                21,
                Payload(vec![]),
                Some("client-b"),
                Some(2.0),
            );
            assert_eq!(scale(&cache, 20, "client-a"), Some(7.35));
            assert_eq!(scale(&cache, 21, "client-b"), Some(2.0));
        });
    }

    #[test]
    fn same_session_latest_snapshot_replaces_or_refuses_older_image_context() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            cache.publish_for_session(
                std::process::id() as i32,
                20,
                Payload(vec![]),
                Some("client-a"),
                Some(7.35),
            );
            cache.publish_for_session(
                std::process::id() as i32,
                20,
                Payload(vec![]),
                Some("client-a"),
                Some(1.0),
            );
            assert_eq!(
                scale(&cache, 20, "client-a"),
                Some(1.0),
                "a newer native capture replaces the older resized frame"
            );

            cache.publish_for_session(
                std::process::id() as i32,
                20,
                Payload(vec![]),
                Some("client-a"),
                None,
            );
            assert_eq!(
                scale(&cache, 20, "client-a"),
                None,
                "a newer tree-only observation retires the older image frame"
            );
        });
    }

    #[test]
    fn window_relative_pixels_require_a_current_snapshot() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::<Payload>::new();
            assert_eq!(scale(&cache, 20, "client-a"), None);
            cache.publish_for_session(
                std::process::id() as i32,
                20,
                Payload(vec![]),
                Some("client-a"),
                Some(2.0),
            );
            cache.remove(std::process::id() as i32, 20);
            assert_eq!(scale(&cache, 20, "client-a"), None);
        });
    }

    #[test]
    fn window_less_screenshot_context_requires_one_agreed_transform() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            assert!(cache
                .screenshot_context(std::process::id() as i32, None, Some("client-a"))
                .is_err());
            cache.publish_for_session(
                std::process::id() as i32,
                20,
                Payload(vec![]),
                Some("client-a"),
                Some(2.0),
            );
            cache.publish_for_session(
                std::process::id() as i32,
                21,
                Payload(vec![]),
                Some("client-a"),
                Some(2.0),
            );
            assert_eq!(
                cache
                    .screenshot_context(std::process::id() as i32, None, Some("client-a"))
                    .unwrap()
                    .scale,
                2.0
            );
            cache.publish_for_session(
                std::process::id() as i32,
                22,
                Payload(vec![]),
                Some("client-a"),
                Some(3.0),
            );
            assert!(cache
                .screenshot_context(std::process::id() as i32, None, Some("client-a"))
                .is_err());
        });
    }

    #[test]
    fn publication_reports_replaced_and_evicted_snapshots() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let (first, invalidated) = cache
                .publish_for_session(std::process::id() as i32, 0, Payload(vec![]), None, None)
                .unwrap();
            assert!(invalidated.is_empty());
            let (second, invalidated) = cache
                .publish_for_session(std::process::id() as i32, 0, Payload(vec![]), None, None)
                .unwrap();
            assert_eq!(invalidated, vec![first]);
            for window_id in 1..LRU_CAP_PER_PID as u64 {
                cache.publish(std::process::id() as i32, window_id, Payload(vec![]));
            }
            let (_, invalidated) = cache
                .publish_for_session(
                    std::process::id() as i32,
                    LRU_CAP_PER_PID as u64,
                    Payload(vec![]),
                    None,
                    None,
                )
                .unwrap();
            assert_eq!(invalidated, vec![second]);
        });
    }

    #[test]
    fn session_retirement_removes_only_snapshots_owned_by_that_session() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            cache.publish_for_session(
                std::process::id() as i32,
                20,
                Payload(vec![]),
                Some("ending"),
                Some(7.35),
            );
            cache.publish_for_session(
                std::process::id() as i32,
                21,
                Payload(vec![]),
                Some("survivor"),
                Some(2.0),
            );
            assert_eq!(cache.retire_session_screenshots("ending"), 1);
            assert_eq!(scale(&cache, 20, "ending"), None);
            assert_eq!(scale(&cache, 21, "survivor"), Some(2.0));
        });
    }

    #[test]
    fn recreating_an_idle_reclaimed_implicit_session_keeps_its_old_tokens_stale() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            use crate::session::{
                begin_session_dispatch, evict_idle_with_prefix, register_scoped_session_end_hook,
                SessionClientKind, SessionTransport,
            };
            let session = format!("snapshot-idle-implicit-{}", std::process::id());
            let cache = Arc::new(SnapshotStore::<Payload>::new());
            let retiring = cache.clone();
            // End hooks are process-global: another test's session end must not
            // retire this store's screenshots and reject its publications.
            let own_session = session.clone();
            let _hook = register_scoped_session_end_hook(move |ended| {
                if ended == own_session.as_str() {
                    retiring.retire_session_screenshots(ended);
                }
            });
            let begin = || {
                begin_session_dispatch(
                    &session,
                    None,
                    &session,
                    true,
                    SessionTransport::McpStdio,
                    SessionClientKind::Mcp,
                )
            };
            let resolve = |token: &str| {
                cache.resolve(
                    std::process::id() as i32,
                    &serde_json::json!({ "element_token": token }),
                )
            };

            let guard = begin().expect("first unnamed call starts the session");
            let (snapshot, _) = cache
                .publish_for_session(
                    std::process::id() as i32,
                    20,
                    Payload(vec![0, 1]),
                    Some(&session),
                    Some(1.0),
                )
                .expect("live session publishes");
            let token = token_for(snapshot, 1);
            drop(guard);
            assert_eq!(
                evict_idle_with_prefix(std::time::Duration::ZERO, &session),
                std::slice::from_ref(&session)
            );

            let guard = begin().expect("next unnamed call recreates the session");
            assert!(resolve(&token).is_err());
            let (fresh, _) = cache
                .publish_for_session(
                    std::process::id() as i32,
                    20,
                    Payload(vec![0, 1]),
                    Some(&session),
                    Some(1.0),
                )
                .expect("recreated session publishes again");
            assert!(matches!(
                resolve(&token_for(fresh, 1)).unwrap(),
                ResolvedElement::Element {
                    window_id: 20,
                    element,
                    ..
                } if *element == 1
            ));
            drop(guard);
            crate::session::end_session(&session);
            crate::session::revive_session(&session);
        });
    }

    #[test]
    fn capture_completing_after_session_end_is_not_published() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let session = format!("snapshot-late-capture-{}", uuid::Uuid::new_v4());
            assert!(crate::session::fire_session_end(&session));
            assert_eq!(
                cache.publish_for_session(
                    std::process::id() as i32,
                    20,
                    Payload(vec![]),
                    Some(&session),
                    Some(7.35)
                ),
                None
            );
            assert_eq!(scale(&cache, 20, &session), None);
        });
    }

    #[test]
    fn zoom_context_is_bound_to_snapshot_session_and_window() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let snapshot = cache
                .publish_for_session(
                    std::process::id() as i32,
                    20,
                    Payload(vec![]),
                    Some("client-a"),
                    Some(7.35),
                )
                .unwrap()
                .0;
            let context = zoom_on(snapshot, 7.35);
            cache
                .set_zoom(std::process::id() as i32, Some("client-a"), context)
                .unwrap();

            assert_eq!(
                cache
                    .zoom(std::process::id() as i32, Some(20), Some("client-a"))
                    .unwrap(),
                context
            );
            assert_eq!(
                cache
                    .zoom(std::process::id() as i32, None, Some("client-a"))
                    .unwrap(),
                context
            );
            assert_eq!(context.zoom_to_window(3.0, 4.0), (106.0, 58.0));
            for (window_id, session) in [(20, "client-b"), (21, "client-a")] {
                assert_eq!(
                    refusal_code(
                        cache
                            .zoom(std::process::id() as i32, Some(window_id), Some(session))
                            .unwrap_err()
                    ),
                    "zoom_context_missing"
                );
            }
        });
    }

    #[test]
    fn window_only_screenshot_lookup_requires_one_current_owned_snapshot() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let mut other_process = super::tests::LiveProcess::new();
            let first = cache
                .publish_for_session(
                    std::process::id() as i32,
                    20,
                    Payload(vec![]),
                    Some("client-a"),
                    Some(2.0),
                )
                .unwrap()
                .0;
            assert_eq!(
                cache
                    .screenshot_context_for_zoom(None, 20, Some("client-a"))
                    .unwrap(),
                (
                    std::process::id() as i32,
                    ScreenshotContext {
                        frame_revision: 1,
                        snapshot_id: first,
                        window_id: 20,
                        scale: 2.0,
                    }
                )
            );
            assert_eq!(
                cache
                    .screenshot_context_for_zoom(
                        Some(std::process::id() as i32),
                        20,
                        Some("client-a")
                    )
                    .unwrap()
                    .0,
                std::process::id() as i32
            );
            assert!(cache
                .screenshot_context_for_zoom(None, 20, Some("client-b"))
                .is_err());

            cache.publish_for_session(
                other_process.pid(),
                20,
                Payload(vec![]),
                Some("client-a"),
                Some(1.0),
            );
            assert!(cache
                .screenshot_context_for_zoom(None, 20, Some("client-a"))
                .is_err());
        });
    }

    #[test]
    fn late_zoom_completion_cannot_replace_newer_valid_context() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let snapshot_a = cache
                .publish_for_session(
                    std::process::id() as i32,
                    20,
                    Payload(vec![]),
                    Some("client-a"),
                    Some(2.0),
                )
                .unwrap()
                .0;
            let snapshot_b = cache
                .publish_for_session(
                    std::process::id() as i32,
                    20,
                    Payload(vec![]),
                    Some("client-a"),
                    Some(1.0),
                )
                .unwrap()
                .0;
            let valid_b = zoom_on(snapshot_b, 1.0);
            cache
                .set_zoom(std::process::id() as i32, Some("client-a"), valid_b)
                .unwrap();
            assert!(cache
                .set_zoom(
                    std::process::id() as i32,
                    Some("client-a"),
                    zoom_on(snapshot_a, 2.0)
                )
                .is_err());
            assert_eq!(
                cache
                    .zoom(std::process::id() as i32, Some(20), Some("client-a"))
                    .unwrap(),
                valid_b
            );
        });
    }

    #[test]
    fn newer_snapshot_or_session_end_retires_zoom() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let snapshot = cache
                .publish_for_session(
                    std::process::id() as i32,
                    20,
                    Payload(vec![]),
                    Some("client-a"),
                    Some(7.35),
                )
                .unwrap()
                .0;
            cache
                .set_zoom(
                    std::process::id() as i32,
                    Some("client-a"),
                    zoom_on(snapshot, 7.35),
                )
                .unwrap();
            cache.publish_for_session(
                std::process::id() as i32,
                20,
                Payload(vec![]),
                Some("client-b"),
                Some(1.0),
            );
            assert_eq!(
                refusal_code(
                    cache
                        .zoom(std::process::id() as i32, Some(20), Some("client-a"))
                        .unwrap_err()
                ),
                "zoom_context_missing"
            );

            let latest = cache
                .publish_for_session(
                    std::process::id() as i32,
                    20,
                    Payload(vec![]),
                    Some("client-a"),
                    Some(1.0),
                )
                .unwrap()
                .0;
            cache
                .set_zoom(
                    std::process::id() as i32,
                    Some("client-a"),
                    zoom_on(latest, 1.0),
                )
                .unwrap();
            assert_eq!(cache.retire_session_screenshots("client-a"), 1);
            assert!(cache
                .zoom(std::process::id() as i32, Some(20), Some("client-a"))
                .is_err());
        });
    }

    struct DropCounter {
        owner: Weak<SnapshotStore<DropCounter>>,
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
                    owner.inner.try_lock().is_ok(),
                    "native cleanup ran under the storage lock"
                );
            }
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn session_retirement_drops_payload_outside_lock() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = Arc::new(SnapshotStore::new());
            let drops = Arc::new(AtomicUsize::new(0));
            cache.publish_for_session(
                std::process::id() as i32,
                20,
                DropCounter {
                    owner: Arc::downgrade(&cache),
                    drops: drops.clone(),
                },
                Some("ending"),
                Some(2.0),
            );
            assert_eq!(cache.retire_session_screenshots("ending"), 1);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(cache.inner.lock().unwrap().snapshots.is_empty());
        });
    }

    #[test]
    fn replacement_remove_and_clear_run_drop_outside_lock() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = Arc::new(SnapshotStore::new());
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
        });
    }

    #[test]
    fn window_identity_preserves_high_bits_through_resolution_and_retirement() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let cache = SnapshotStore::new();
            let low = 7;
            let high = (1_u64 << 32) | low;
            let first = cache.publish(std::process::id() as i32, low, Payload(vec![10]));
            let second = cache.publish(std::process::id() as i32, high, Payload(vec![20]));
            let token = serde_json::json!({ "element_token": token_for(second, 0) });
            assert!(matches!(
                cache.resolve(std::process::id() as i32, &token).unwrap(),
                ResolvedElement::Element { window_id, element, .. } if window_id == high && *element == 20
            ));
            cache.remove(std::process::id() as i32, high);
            assert!(cache.resolve(std::process::id() as i32, &token).is_err());
            assert!(matches!(
                cache
                    .resolve(
                        std::process::id() as i32,
                        &serde_json::json!({ "element_token": token_for(first, 0) })
                    )
                    .unwrap(),
                ResolvedElement::Element { window_id: 7, .. }
            ));
        });
    }

    #[test]
    fn bindings_sharing_a_scope_keep_independent_payload_ownership() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            // The fork keeps a second copy of this test; a shared scope name would
            // let one copy retire the other's runtime stores mid-test.
            let scope = format!("snapshot-binding-ownership-{}", uuid::Uuid::new_v4());
            crate::tool::with_runtime_scope(scope.clone(), || {
                let first = Arc::new(SnapshotStore::new());
                let second = Arc::new(SnapshotStore::new());
                register_runtime_store(&first);
                register_runtime_store(&second);
                let first_id = first.publish(std::process::id() as i32, 7, Payload(vec![10]));
                let second_id = second.publish(std::process::id() as i32, 7, Payload(vec![20]));
                assert!(second
                    .resolve(
                        std::process::id() as i32,
                        &serde_json::json!({ "element_token": token_for(first_id, 0) })
                    )
                    .is_err());
                drop(first);
                let resolved = second
                    .resolve(
                        std::process::id() as i32,
                        &serde_json::json!({ "element_token": token_for(second_id, 0) }),
                    )
                    .unwrap();
                assert!(matches!(
                    resolved,
                    ResolvedElement::Element { element, .. } if *element == 20
                ));
                assert!(Arc::ptr_eq(
                    &current_runtime_store::<Payload>().unwrap(),
                    &second
                ));
                retire_runtime_scope(&scope);
            });
        });
    }

    #[test]
    fn recording_discovery_does_not_extend_payload_lifetime() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            // The fork keeps a second copy of this test; a shared scope name would
            // let one copy retire the other's runtime stores mid-test.
            let scope = format!("snapshot-weak-discovery-{}", uuid::Uuid::new_v4());
            crate::tool::with_runtime_scope(scope.clone(), || {
                let cache = Arc::new(SnapshotStore::new());
                let drops = Arc::new(AtomicUsize::new(0));
                cache.publish(
                    std::process::id() as i32,
                    7,
                    DropCounter {
                        owner: Arc::downgrade(&cache),
                        drops: drops.clone(),
                    },
                );
                register_runtime_store(&cache);
                assert_eq!(Arc::strong_count(&cache), 1);
                drop(cache);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert!(current_runtime_store::<DropCounter>().is_none());
                {
                    let caches = runtime_stores().lock().unwrap();
                    assert!(!caches.contains_key(scope.as_str()));
                    if caches.is_empty() {
                        assert_eq!(caches.capacity(), 0);
                    }
                }
                assert_eq!(retire_runtime_scope(&scope), 0);
            });
        });
    }

    #[test]
    fn default_impl_matches_new() {
        crate::tool::with_runtime_scope(format!("snapshot-test-{}", uuid::Uuid::new_v4()), || {
            let _cache: SnapshotStore<Payload> = SnapshotStore::default();
        });
    }
}

#[cfg(test)]
mod refresh_regressions {
    use super::*;
    use crate::snapshot_test_support::Payload;
    fn isolated(f: impl FnOnce()) {
        crate::tool::with_runtime_scope(format!("refresh-{}", uuid::Uuid::new_v4()), f);
    }
    fn publish(cache: &SnapshotStore<Payload>, owner: &str) -> u32 {
        cache
            .try_publish_sized_for_session(
                std::process::id() as i32,
                81,
                Payload(vec![17]),
                Some(owner),
                Some(2.0),
                Some((100.0, 80.0)),
            )
            .unwrap()
            .unwrap()
            .0
    }
    #[test]
    fn refresh_retains_native_payload_and_permit_but_rejects_old_frame_and_zoom() {
        isolated(|| {
            let cache = SnapshotStore::new();
            let pid = std::process::id() as i32;
            let id = publish(&cache, "owner");
            let identity = cache.identity_for_snapshot(pid, id).unwrap();
            let permit = Arc::new(
                element_token::global()
                    .try_acquire_mutation(identity)
                    .unwrap(),
            );
            let payload = permit.payload::<Payload>().unwrap();
            let old = cache
                .screenshot_context(pid, Some(81), Some("owner"))
                .unwrap();
            let binding = cache.frame_binding(pid, Some("owner"), old).unwrap();
            let zoom = ZoomContext {
                screenshot: old,
                origin_x: 0.0,
                origin_y: 0.0,
                scale_inv: 1.0,
            };
            cache.set_zoom(pid, Some("owner"), zoom).unwrap();
            let first = cache
                .screenshot_refresh_ticket(pid, 81, Some("owner"))
                .unwrap();
            let racing = cache
                .screenshot_refresh_ticket(pid, 81, Some("owner"))
                .unwrap();
            let fresh = cache
                .refresh_screenshot_for_session(first, 2.0, Some((100.0, 80.0)))
                .unwrap()
                .unwrap();
            assert_eq!(fresh.snapshot_id, id);
            assert_eq!(fresh.frame_revision, old.frame_revision + 1);
            assert_eq!(cache.identity_for_snapshot(pid, id), Some(identity));
            assert!(Arc::ptr_eq(&payload, &permit.payload::<Payload>().unwrap()));
            assert_eq!(permit.identity(), identity);
            assert!(!binding.is_current());
            assert!(cache
                .with_valid_screenshot_context(pid, Some("owner"), old, || panic!(
                    "old PNG published"
                ))
                .is_err());
            assert_eq!(
                cache
                    .set_zoom(pid, Some("owner"), zoom)
                    .unwrap_err()
                    .structured_content
                    .unwrap()["code"],
                "zoom_context_missing"
            );
            assert_eq!(
                cache
                    .zoom(pid, Some(81), Some("owner"))
                    .unwrap_err()
                    .structured_content
                    .unwrap()["code"],
                "zoom_context_missing"
            );
            assert!(matches!(
                cache.refresh_screenshot_for_session(racing, 2.0, None),
                Err(RegistryError::NotCurrent)
            ));
            assert!(cache
                .resolve(
                    pid,
                    &serde_json::json!({"element_token":element_token::token_for(id,0)})
                )
                .is_ok());
        });
    }
    #[test]
    fn refresh_refuses_foreign_retired_replaced_and_closed_authority() {
        isolated(|| {
            let cache = Arc::new(SnapshotStore::new());
            register_runtime_store(&cache);
            let pid = std::process::id() as i32;
            publish(&cache, "owner");
            assert!(matches!(
                cache.screenshot_refresh_ticket(pid, 81, Some("foreign")),
                Err(RegistryError::NotCurrent)
            ));
            let old = cache
                .screenshot_refresh_ticket(pid, 81, Some("owner"))
                .unwrap();
            publish(&cache, "owner");
            assert!(matches!(
                cache.refresh_screenshot_for_session(old, 1.0, None),
                Err(RegistryError::NotCurrent)
            ));
            let old = cache
                .screenshot_refresh_ticket(pid, 81, Some("owner"))
                .unwrap();
            cache.retire_session_screenshots("owner");
            assert!(matches!(
                cache.refresh_screenshot_for_session(old, 1.0, None),
                Err(RegistryError::NotCurrent)
            ));
            assert!(matches!(
                cache.screenshot_refresh_ticket(pid, 81, Some("owner")),
                Err(RegistryError::NotCurrent)
            ));
            retire_runtime_scope(&current_runtime_scope());
            assert!(matches!(
                cache.screenshot_refresh_ticket(pid, 82, None),
                Err(RegistryError::NotCurrent)
            ));
        });
    }
    #[test]
    fn empty_refresh_fallback_keeps_pre_image_lifecycle_ticket() {
        isolated(|| {
            let cache = SnapshotStore::new();
            let pid = std::process::id() as i32;
            let ticket = cache
                .screenshot_refresh_ticket(pid, 81, Some("owner"))
                .unwrap();
            let other = SnapshotStore::<Payload>::new();
            assert!(matches!(
                other.refresh_screenshot_for_session(ticket.clone(), 1.0, None),
                Err(RegistryError::NotCurrent)
            ));
            assert!(cache
                .refresh_screenshot_for_session(ticket.clone(), 1.0, None)
                .unwrap()
                .is_none());
            let competing = element_token::global()
                .prepare_current(pid, 81, 1, Arc::new(Payload(vec![2])))
                .unwrap();
            element_token::global()
                .publish(competing, Duration::ZERO)
                .unwrap();
            assert!(matches!(
                cache.try_publish_capture_for_refresh(ticket.clone(), Payload(vec![]), 1.0),
                Err(RegistryError::NotCurrent)
            ));
            cache.clear();
            assert!(matches!(
                cache.try_publish_capture_for_refresh(ticket, Payload(vec![]), 1.0),
                Err(RegistryError::NotCurrent)
            ));
        });
    }
    #[test]
    fn refresh_fallback_is_compatible_but_overflow_and_stale_lane_stay_closed() {
        isolated(|| {
            let cache = SnapshotStore::new();
            let pid = std::process::id() as i32;
            let ticket = cache
                .screenshot_refresh_ticket(pid, 81, Some("owner"))
                .unwrap();
            assert!(cache
                .refresh_screenshot_for_session(ticket.clone(), 1.0, None)
                .unwrap()
                .is_none());
            let id = cache
                .try_publish_capture_for_refresh(ticket, Payload(vec![]), 1.0)
                .unwrap()
                .unwrap()
                .0;
            assert_eq!(
                cache
                    .screenshot_context(pid, Some(81), Some("owner"))
                    .unwrap()
                    .snapshot_id,
                id
            );
            publish(&cache, "owner");
            let key = element_token::global()
                .lane_key_for_current_runtime(pid, 81)
                .unwrap();
            assert!(element_token::global().mark_stale(&key));
            assert!(matches!(
                cache.screenshot_refresh_ticket(pid, 81, Some("owner")),
                Err(RegistryError::NotCurrent)
            ));
            for index in 0..=RETIRED_SCREENSHOT_CAPACITY {
                cache.retire_session_screenshots(&format!("retired-{index}"));
            }
            assert!(matches!(
                cache.screenshot_refresh_ticket(pid, 82, None),
                Err(RegistryError::NotCurrent)
            ));
        });
    }

    #[test]
    fn tree_only_inheritance_requires_same_owner_known_size_and_retires_old_frame() {
        isolated(|| {
            let cache = SnapshotStore::new();
            let pid = std::process::id() as i32;
            let id = publish(&cache, "owner");
            let old = cache
                .screenshot_context(pid, Some(81), Some("owner"))
                .unwrap();
            let frame = cache.frame_binding(pid, Some("owner"), old).unwrap();
            let new = cache
                .try_publish_sized_for_session(
                    pid,
                    81,
                    Payload(vec![22]),
                    Some("owner"),
                    None,
                    Some((100.1, 80.0)),
                )
                .unwrap()
                .unwrap()
                .0;
            assert_ne!(new, id);
            assert_eq!(
                cache
                    .screenshot_context(pid, Some(81), Some("owner"))
                    .unwrap()
                    .scale,
                2.0
            );
            assert!(!frame.is_current());
            assert!(cache
                .resolve(
                    pid,
                    &serde_json::json!({"element_token":element_token::token_for(id,0)})
                )
                .is_err());
            cache
                .try_publish_sized_for_session(
                    pid,
                    81,
                    Payload(vec![22]),
                    Some("owner"),
                    None,
                    Some((100.5, 80.0)),
                )
                .unwrap();
            // Size is the new tree's known size; the inherited image's dimensions
            // must remain its original dimensions, never drift by repeated reads.
            assert!(cache
                .screenshot_context(pid, Some(81), Some("owner"))
                .is_err());
            publish(&cache, "owner");
            cache
                .try_publish_sized_for_session(
                    pid,
                    81,
                    Payload(vec![22]),
                    Some("foreign"),
                    None,
                    Some((100.0, 80.0)),
                )
                .unwrap();
            assert!(cache
                .screenshot_context(pid, Some(81), Some("foreign"))
                .is_err());
        });
    }
    struct Sparse;
    impl SnapshotPayload for Sparse {
        type Element = usize;
        fn len(&self) -> usize {
            12
        }
        fn contains_index(&self, row: usize) -> bool {
            matches!(row, 0 | 11)
        }
        fn retain(&self, row: usize) -> Option<usize> {
            matches!(row, 0 | 11).then_some(row)
        }
    }
    #[test]
    fn bare_rows_pin_sparse_membership_exact_window_and_do_not_retarget_queued_args() {
        isolated(|| {
            let cache = Arc::new(SnapshotStore::new());
            register_runtime_store(&cache);
            let pid = std::process::id() as i32;
            let id = cache.publish(pid, 81, Sparse);
            let mut args = serde_json::json!({"pid":pid,"element_token":" +011 "});
            canonicalize_bare_row(&mut args).unwrap();
            assert_eq!(args["element_token"], element_token::token_for(id, 11));
            assert_eq!(args["window_id"], 81);
            let mut hole = serde_json::json!({"pid":pid,"element_token":"3"});
            assert_eq!(
                canonicalize_bare_row(&mut hole)
                    .unwrap_err()
                    .structured_content
                    .unwrap()["refusal"]["code"],
                "invalid_element_token"
            );
            cache.publish(pid, 82, Sparse);
            let mut ambiguous = serde_json::json!({"pid":pid,"element_token":"0"});
            assert!(canonicalize_bare_row(&mut ambiguous).is_err());
            cache.publish(pid, 81, Sparse);
            canonicalize_bare_row(&mut args).unwrap();
            assert_eq!(args["element_token"], element_token::token_for(id, 11));
            assert_eq!(
                cache
                    .resolve(pid, &args)
                    .unwrap_err()
                    .structured_content
                    .unwrap()["refusal"]["code"],
                "stale_element_token"
            );
        });
    }
}
