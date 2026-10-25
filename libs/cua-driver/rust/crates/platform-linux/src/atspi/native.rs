//! Native AT-SPI access over D-Bus via the `atspi` crate (zbus).
//!
//! Replaces the previous `python3 -c "import pyatspi; ..."` subprocess bridge:
//! no Python, `pyatspi`, or GObject-introspection typelibs are needed at
//! runtime. The zbus calls are async, so each public entry point drives a
//! small shared Tokio runtime via `block_on` (callers already invoke these
//! from `cua_driver_core::blocking::spawn`, so blocking here is safe).
//!
//! Element indices match the markdown produced by [`walk_tree`]: a depth-first,
//! pre-order traversal of the target application's windows, numbering the
//! nodes accepted by the shared [`is_indexable`] capability predicate.
//! `perform_action`, `set_value`, and `get_element_bounds` index into that same
//! ordered set.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::proxy_ext::ProxyExt;
use atspi::{CoordType, Interface, State, StateSet};
use atspi_connection::AccessibilityConnection;

use super::identity::{
    exact_window_element_index, unique_observed_identity_position, verify_observed_frame_ancestry,
    ObjectAddress, ObservedIdentityCandidate,
};
use super::{AtspiIdentity, AtspiNode};

// Presence hit-testing is read-only and does not redirect mutation authority.
pub mod hit;

// Flatpak apps register on the accessibility bus through their sandbox's proxy.
mod sandbox_peer;
mod tab_documents;

// Point-owner redirection is deferred: it can substitute another native object.

/// Per-call D-Bus timeout: a single unresponsive accessible (common in large,
/// lazily-built trees like Chromium's) must not stall the whole walk.
const CALL_TIMEOUT: Duration = Duration::from_secs(3);
/// Overall budget for one tree walk / operation.
const OP_TIMEOUT: Duration = Duration::from_secs(25);
/// Startup may run before `serve` binds its socket or MCP reads stdin. A
/// reachable but wedged accessibility bus must not hold either entry point
/// forever. Initialization remains owned after this readiness budget so a
/// late registry reply can still establish the listener; shutdown cancels and
/// awaits it.
const LISTENER_STARTUP_TIMEOUT: Duration = Duration::from_secs(3);

tokio::task_local! {
    /// Wall-clock deadline for the AT-SPI operation currently running on the
    /// runtime. Every [`call`] clamps its per-call timeout to what is left of
    /// it, so a 1 s tool budget really is 1 s even when a single node would
    /// otherwise be allowed the full [`CALL_TIMEOUT`]. Set via [`bounded_for`]
    /// / [`walk_tree_bounded_with_timeout`]; absent for legacy callers.
    static OP_DEADLINE: tokio::time::Instant;
    /// Set by the snapshot walk ([`walk_tree_bounded_with_timeout`]) only:
    /// receives the reason when the application lookup could not prove
    /// absence (a registry or peer call failed or timed out), so the walk can
    /// retry instead of treating it as "no application". Other callers keep
    /// their `Ok(None)` (type_text falls back to XTEST on it).
    static REGISTRY_INCOMPLETE: std::cell::Cell<Option<&'static str>>;
    /// Set for a snapshot walk ([`collect_visited_bounded_opts`]) of an
    /// application admitted through its Flatpak sandbox's proxy: every object
    /// the walk addresses must then be on that proxy's own connection (see
    /// [`sandbox_gate_allows`]). Unset (`None`) for every other walk.
    static SANDBOX_GATE: std::cell::RefCell<Option<SandboxGate>>;
}

/// The proxy a sandboxed walk is confined to, and the verdicts already
/// reached for the unique bus names it has seen.
struct SandboxGate {
    proxy: u32,
    verdicts: std::collections::HashMap<String, bool>,
}

/// Whether the current walk may address an object on bus `name`. Always true
/// outside a sandboxed walk. Inside one, only a unique name whose connection
/// belongs to the sandbox proxy passes: a well-known name can move to another
/// connection between a check and its use, and a unique name cannot.
async fn sandbox_gate_allows(conn: &atspi::zbus::Connection, name: &str) -> bool {
    let gate = SANDBOX_GATE.try_with(|gate| {
        gate.borrow()
            .as_ref()
            .map(|gate| (gate.proxy, gate.verdicts.get(name).copied()))
    });
    let (proxy, cached) = match gate {
        Ok(Some(gate)) => gate,
        // Outside a sandboxed walk (or outside any walk scope).
        Ok(None) | Err(_) => return true,
    };
    if let Some(verdict) = cached {
        return verdict;
    }
    let owner = if name.starts_with(':') {
        match atspi::zbus::fdo::DBusProxy::new(conn).await {
            Ok(dbus) => bus_owner_pid(&dbus, name).await,
            Err(_) => None,
        }
    } else {
        None
    };
    let verdict = sandbox_peer::addressable(name, owner, proxy);
    let _ = SANDBOX_GATE.try_with(|gate| {
        if let Some(gate) = gate.borrow_mut().as_mut() {
            gate.verdicts.insert(name.to_owned(), verdict);
        }
    });
    verdict
}

fn sandbox_refusal(name: &str) -> anyhow::Error {
    anyhow!("sandbox_foreign_object: {name} is not on the Flatpak sandbox proxy's connection")
}

/// Time left on the current operation's deadline, or [`CALL_TIMEOUT`] when no
/// deadline is in scope.
fn remaining_budget() -> Duration {
    OP_DEADLINE
        .try_with(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
        .unwrap_or(CALL_TIMEOUT)
}

/// True once the current operation's deadline has passed (never for callers
/// that did not set one).
fn deadline_passed() -> bool {
    OP_DEADLINE
        .try_with(|deadline| tokio::time::Instant::now() >= *deadline)
        .unwrap_or(false)
}

/// Run `fut` with [`CALL_TIMEOUT`] (clamped to the operation deadline when one
/// is in scope); `None` on timeout so the caller can skip the node and keep
/// walking rather than blocking forever.
async fn call<T>(fut: impl std::future::Future<Output = T>) -> Option<T> {
    let budget = remaining_budget().min(CALL_TIMEOUT);
    if budget.is_zero() {
        return None;
    }
    tokio::time::timeout(budget, fut).await.ok()
}

/// Run a *synchronous, blocking* closure (typically a raw X11 round-trip via
/// `x11rb::RustConnection`, which has no built-in socket timeout) off the
/// current task and bound how long the caller waits for it.
///
/// This exists because `tokio::time::timeout` around an `.await` only helps
/// when the wrapped future actually yields control back to the runtime at
/// its await points; a plain synchronous call made directly inside an async
/// fn (no `.await` inside it) blocks the executor thread for its full
/// duration regardless of any `timeout`/`timeout_at` wrapped around the
/// *outer* future, because that outer future never gets polled again until
/// the blocking call returns. `get_window_state`'s bounds phase hit exactly
/// this on LibreOffice: `window_to_screen_offset`/`x11_window_origin` open a
/// fresh X11 connection and block on `GetGeometry`/`TranslateCoordinates`
/// replies with no timeout, and when the X server is slow to answer (busy
/// servicing LibreOffice's own heavy repaint traffic) that single call could
/// run for many seconds — well past `timeout_ms` — with none of the AT-SPI
/// D-Bus deadline machinery able to see or bound it (#42).
///
/// `spawn_blocking` moves the closure to a dedicated blocking-pool thread so
/// the timeout here can make the *caller* proceed with `None`; the orphaned
/// thread may still be blocked on the X server afterward (there is no way to
/// cancel a live X11 socket read), but that no longer holds up the walk or
/// consumes the operation's wall-clock budget.
async fn bounded_blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let budget = remaining_budget().min(CALL_TIMEOUT);
    if budget.is_zero() {
        return None;
    }
    match tokio::time::timeout(budget, cua_driver_core::blocking::spawn(f)).await {
        Ok(Ok(value)) => Some(value),
        _ => None,
    }
}

async fn before_snapshot_deadline<T>(
    deadline: tokio::time::Instant,
    work: impl std::future::Future<Output = T>,
) -> std::result::Result<T, tokio::time::error::Elapsed> {
    tokio::time::timeout_at(deadline, work).await
}

fn push_action_name_slot(actions: &mut Vec<String>, name: Option<String>) {
    actions.push(name.unwrap_or_default());
}

async fn live_action_names(action: &atspi::proxy::action::ActionProxy<'_>) -> Result<Vec<String>> {
    let count = call(action.n_actions())
        .await
        .and_then(|result| result.ok())
        .ok_or_else(|| anyhow!("AT-SPI action count lookup timed out"))?;
    let mut names = Vec::with_capacity(count as usize);
    for index in 0..count {
        let name = call(action.get_name(index))
            .await
            .and_then(|result| result.ok())
            .unwrap_or_default();
        names.push(name);
    }
    Ok(names)
}

fn require_affirmative_ack(accepted: bool, operation: &str) -> Result<()> {
    if !accepted {
        anyhow::bail!("{operation} returned false");
    }
    Ok(())
}

async fn select_live_activation(
    action: &atspi::proxy::action::ActionProxy<'_>,
    role: &str,
    context: &str,
) -> Result<(i32, String)> {
    // Native action indexes are not stable authority. Re-read the complete
    // vector at the mutation boundary and select only by live semantics.
    let names = live_action_names(action).await?;
    let chosen = activation_index(role, &names)
        .ok_or_else(|| anyhow!("{context} has no live safe activation action"))?;
    Ok((chosen as i32, names[chosen].clone()))
}

async fn invoke_live_activation(
    action: &atspi::proxy::action::ActionProxy<'_>,
    role: &str,
    context: &str,
) -> Result<String> {
    let (chosen, selected) = select_live_activation(action, role, context).await?;
    let accepted = action
        .do_action(chosen)
        .await
        .map_err(|error| anyhow!("doAction failed: {error}"))?;
    require_affirmative_ack(accepted, &format!("{context} live action {selected:?}"))?;
    Ok(selected)
}

async fn invoke_exact_live_activation(
    action: &atspi::proxy::action::ActionProxy<'_>,
    conn: &AccessibilityConnection,
    target_proof: &crate::wayland::ExactTargetProof,
    element_key: u64,
    role: &str,
    context: &str,
    point: Option<(i32, i32, CoordType, i32, i32)>,
) -> Result<String> {
    let (chosen, selected) = select_live_activation(action, role, context).await?;

    // Re-walk after the action-vector round trips. This catches a retained
    // object that was reparented into a same-process sibling while its proxy was
    // being resolved; a compositor identity check alone cannot see that move.
    let pid = target_proof.pid();
    let window_id = target_proof.window_id();
    let Collected {
        visited,
        scoped_frame,
        ..
    } = collect_visited_bounded(conn, pid, window_id, None, None)
        .await?
        .ok_or_else(|| anyhow!("exact target disappeared before mutation"))?;
    let scoped_frame = scoped_frame
        .ok_or_else(|| anyhow!("could not revalidate exact window ancestry before mutation"))?;
    let indexed_elements = visited
        .iter()
        .filter(|node| is_indexable(node))
        .map(|node| (node.frame_ordinal, node.element_key))
        .collect::<Vec<_>>();
    exact_window_element_index(&indexed_elements, scoped_frame, element_key, window_id)?;

    if let Some((screen_x, screen_y, coord, ox, oy)) = point {
        let web_document_origin =
            web_document_origin_for_visited(&visited, pid, window_id, Some(scoped_frame))
                .await
                .unwrap_or((0, 0));
        let action_nodes = visited
            .iter()
            .filter(|node| node.frame_ordinal == scoped_frame && is_indexable(node))
            .collect::<Vec<_>>();
        let mut frames = Vec::new();
        for (index, node) in action_nodes.iter().enumerate() {
            if !node.has_component {
                continue;
            }
            let proxies = node
                .acc
                .proxies()
                .await
                .map_err(|error| anyhow!("final hit-test proxies unavailable: {error}"))?;
            let component = proxies
                .component()
                .await
                .map_err(|error| anyhow!("final hit-test component unavailable: {error}"))?;
            let (x, y, w, h) = component
                .get_extents(coord)
                .await
                .map_err(|error| anyhow!("final hit-test extents unavailable: {error}"))?;
            if x == i32::MIN || y == i32::MIN || w <= 1 || h <= 1 {
                continue;
            }
            let (document_x, document_y) = if node.in_web_doc {
                web_document_origin
            } else {
                (0, 0)
            };
            frames.push((
                index,
                x + ox + document_x,
                y + oy + document_y,
                w as u32,
                h as u32,
                is_passive_role(&node.role),
            ));
        }
        let keys = action_nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (index, node.element_key))
            .collect::<Vec<_>>();
        if selected_key_at_point(&frames, &keys, screen_x, screen_y) != Some(element_key) {
            anyhow::bail!("screen-point recipient changed at final mutation boundary");
        }
    }

    // The AT-SPI ancestry/recipient walk is now final. Revalidate the immutable
    // compositor epoch/incarnation proof after it, with no intervening await
    // before DoAction. A recyclable pid/xid correlation is not authority.
    let proof_for_action = target_proof.clone();
    cua_driver_core::blocking::spawn(move || {
        crate::wayland::validate_exact_target(&proof_for_action)
    })
    .await
    .map_err(|error| anyhow!("exact target validation worker failed: {error}"))??;

    let accepted = action
        .do_action(chosen)
        .await
        .map_err(|error| anyhow!("doAction failed: {error}"))?;
    require_affirmative_ack(accepted, &format!("{context} live action {selected:?}"))?;
    Ok(selected)
}

fn normalized_action_name(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

async fn live_named_action_index(
    action: &atspi::proxy::action::ActionProxy<'_>,
    wanted: &[&str],
) -> Result<Option<(i32, String)>> {
    let names = live_action_names(action).await?;
    Ok(names.into_iter().enumerate().find_map(|(index, name)| {
        wanted
            .iter()
            .any(|candidate| *candidate == normalized_action_name(&name))
            .then_some((index as i32, name))
    }))
}

/// Drive an AT-SPI op `work` on the runtime, bounded by [`OP_TIMEOUT`].
///
/// Individual interface calls are each bounded by [`call`], and `app_for_pid` /
/// `collect_visited` carry their own deadlines — but not every internal await is
/// wrapped (e.g. the EditableText writes in `write_into_editable`, the proxy
/// builds in `app_for_pid`), and an app that holds a modal grab can leave one of
/// those unwrapped round-trips pending indefinitely. `walk_tree` already guards
/// itself this way; this helper applies the same backstop to every other public
/// entry point so a modal/wedged app can never hang the caller (the daemon, an
/// MCP client) past OP_TIMEOUT (#1936). On timeout it yields `on_timeout` — the
/// graceful "couldn't complete" value, so e.g. `type_text` falls back to XTEST.
fn bounded<T>(
    work: impl std::future::Future<Output = Result<T>>,
    on_timeout: impl FnOnce() -> Result<T>,
) -> Result<T> {
    bounded_for(OP_TIMEOUT, work, on_timeout)
}

/// [`bounded`] with an explicit budget. The budget is also installed as the
/// operation deadline, so every inner [`call`] shortens itself to what is
/// left and the walk returns *partial* results before the outer timeout fires
/// (the outer timeout is a backstop for un-wrapped awaits only).
fn bounded_for<T>(
    budget: Duration,
    work: impl std::future::Future<Output = Result<T>>,
    on_timeout: impl FnOnce() -> Result<T>,
) -> Result<T> {
    runtime().block_on(async move {
        let deadline = tokio::time::Instant::now() + budget;
        let backstop = deadline + Duration::from_millis(500);
        match tokio::time::timeout_at(backstop, OP_DEADLINE.scope(deadline, work)).await {
            Ok(r) => r,
            Err(_) => on_timeout(),
        }
    })
}

/// Budget for AT-SPI queries that run *inside* an input action (hit-testing a
/// pixel click, classifying the focused widget, locating an editable). These
/// must never stall the action on a slow app: a miss simply falls through to
/// the next delivery rung.
pub(crate) const INPUT_QUERY_BUDGET: Duration = Duration::from_millis(1500);
/// Budget for resolving an element index against a fresh walk when no cached
/// snapshot answers it. Larger than [`INPUT_QUERY_BUDGET`] because the caller
/// named a specific element and a miss is an error, not a fallback.
pub(crate) const INDEX_RESOLVE_BUDGET: Duration = Duration::from_secs(6);

/// Emit a one-line diagnostic to stderr when `CUA_ATSPI_DEBUG` is set. The
/// driver's stderr is surfaced in the test logs, so this is how we see what the
/// native walk actually found in CI.
fn dbg_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CUA_ATSPI_DEBUG").is_some())
}
macro_rules! dlog {
    ($($arg:tt)*) => {
        if dbg_enabled() { eprintln!("[cua-atspi] {}", format!($($arg)*)); }
    };
}

// All async resources belong to a restartable generation, not process statics.
mod lifecycle;
use lifecycle::{active_descendant_map, focus_map, runtime, shared_connection};
pub use lifecycle::{ensure_listener_active, restart, shutdown};

/// The accessible that currently holds keyboard focus inside `pid`'s
/// application, resolved from the focus-event log (no tree walk). `None` when
/// no focus event was seen for the app or the recorded object no longer
/// reports `Focused`.
async fn focused_accessible_via_events<'a>(
    conn: &AccessibilityConnection,
    pid: u32,
) -> Option<(AccessibleProxy<'a>, ObjectRef)> {
    let app = app_for_pid(conn, pid).await.ok()??;
    let bus = app.inner().destination().to_string();
    let path = focus_map().lock().unwrap().get(&bus).cloned()?;
    let oref = RawObjectRef {
        name: bus.clone(),
        path: path.clone(),
    };
    let acc = call(accessible_for(conn, &oref)).await?.ok()?;
    let state = call(acc.get_state()).await?.ok()?;
    if !state.contains(State::Focused) {
        dlog!("focus log for {bus} is stale ({path} no longer focused)");
        return None;
    }
    Some((acc, ObjectRef { bus, path }))
}

/// A node discovered during the pre-order walk, with its proxy retained so the
/// per-index operations can act on it without re-walking the tree.
/// The `value` of one walked node: its Value-interface value, else, for an
/// editable text widget without a Value interface, its non-empty Text
/// content (the typed string). A SpinButton implements Text and Value, so it
/// keeps its numeric value. Callers pass `has_editable: false` for password
/// fields.
fn editable_text_value(
    has_editable: bool,
    has_value: bool,
    value: Option<String>,
    text_content: &str,
) -> Option<String> {
    if value.is_some() || has_value || !has_editable || text_content.is_empty() {
        return value;
    }
    Some(text_content.to_owned())
}

#[cfg(test)]
mod editable_text_value_tests {
    use super::editable_text_value;

    #[test]
    fn editable_text_reports_its_content_as_value() {
        assert_eq!(
            editable_text_value(true, false, None, "jev-use native note").as_deref(),
            Some("jev-use native note")
        );
    }

    #[test]
    fn empty_or_non_editable_text_has_no_value() {
        assert_eq!(editable_text_value(true, false, None, ""), None);
        assert_eq!(
            editable_text_value(false, false, None, "static label"),
            None
        );
    }

    #[test]
    fn value_interface_wins() {
        assert_eq!(
            editable_text_value(true, true, Some("3".into()), "3.0").as_deref(),
            Some("3")
        );
        assert_eq!(editable_text_value(true, true, None, "3.0"), None);
    }
}

struct Visited<'a> {
    depth: usize,
    role: String,
    /// Display text: the accessible `name`, or — for editable/text widgets that
    /// expose no name — the Text-interface content (where typed text lives).
    name: String,
    /// AT-SPI `Description`, read for controls only: Qt publishes a control's
    /// tooltip there ("Play"/"Pause", "Look in"), GTK its tooltip text.
    description: String,
    value: Option<String>,
    checked: Option<bool>,
    enabled: Option<bool>,
    selected: Option<bool>,
    selectable: bool,
    /// False when the toolkit's state set is known and marks the widget
    /// neither `Showing` nor `Visible`: it exists in the tree but is hidden
    /// (a widget on an unmapped page). Unknown state sets count as showing.
    showing: bool,
    /// `Visible` but not `Showing`: listed and indexed, but not on screen
    /// right now (GTK3 drops `Showing` for a control scrolled out of its
    /// viewport, and for a revealer child such as Nautilus' hover-only eject
    /// buttons). The tree says so, so the caller can scroll it into view or
    /// prefer an on-screen control.
    offscreen: bool,
    /// Children this walk deliberately did not enumerate: a closed menu's
    /// items, or a background tab document's contents. Background roots remain
    /// in the reading with an explicit skipped-contents note.
    collapsed_children: Option<usize>,
    actions: Vec<String>,
    has_editable: bool,
    has_value: bool,
    has_component: bool,
    focused: bool,
    /// True when an ancestor is a web document (e.g. role "document web"),
    /// i.e. this node is page content rather than browser chrome.
    in_web_doc: bool,
    /// True when this node is exported by a separate WebKit WebProcess bus.
    /// Chromium keeps its document on the application's ordinary AT-SPI bus,
    /// where descendant Window extents already include the document origin.
    on_web_process_bus: bool,
    /// Stable identity of the AT-SPI bus/object path for action-time binding.
    element_key: u64,
    /// Stable identity of the owning top-level accessible window.
    top_level_key: u64,
    /// Position of that frame/window in application child order.
    frame_ordinal: usize,
    /// D-Bus identity (bus name + object path) of this accessible, so a later
    /// per-index action can re-open it directly from a cached snapshot instead
    /// of re-walking the application.
    object_ref: ObjectRef,
    identity: Option<AtspiIdentity>,
    acc: AccessibleProxy<'a>,
}

/// Owned D-Bus identity of an accessible object: `(bus name, object path)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectRef {
    pub bus: String,
    pub path: String,
}

/// Why a tree walk stopped before exhausting the application's tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Truncation {
    /// Machine-readable reason: `timeout`, `node_budget`, `app_unresponsive`,
    /// `app_lookup_timeout`.
    pub reason: &'static str,
    /// Nodes fully visited before the walk stopped.
    pub visited: usize,
    /// Nodes discovered but not yet visited when the walk stopped (lower bound
    /// on what is missing — their descendants are unknown).
    pub pending: usize,
}

/// Result of one bounded collection pass.
struct Collected<'a> {
    visited: Vec<Visited<'a>>,
    scoped_frame: Option<usize>,
    truncation: Option<Truncation>,
}

/// Role names that denote embedded web/document content. An editable beneath
/// one of these is page content (the field a user means when typing into a
/// background browser) rather than browser chrome like the address bar.
fn is_document_role(role: &str) -> bool {
    let r = role.to_ascii_lowercase();
    r.contains("document") || r == "embedded"
}

fn is_web_process_bus(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("webkit") || name.contains("webprocess")
}

/// Build an `AccessibleProxy` for an arbitrary (bus name, path) in the tree.
/// Uses owned `String`s for destination/path. zbus clones the connection into
/// the proxy, so retained targets own their handles rather than borrowing a
/// process-lifetime connection. Shutdown explicitly closes that shared socket.
///
/// `cache_properties(No)` is load-bearing, not just an optimization: with the
/// zbus default (`Lazily`) the first property read on the proxy — our
/// `acc.name()` during the walk — makes zbus issue
/// `org.freedesktop.DBus.Properties.GetAll` (one argument: the interface name)
/// to warm the cache. Qt5's AT-SPI bridge (`AtSpiAdaptor::handleMessage`)
/// assumes every Properties call is `Get`/`Set` and unconditionally reads
/// `message.arguments().at(1)`; for a one-argument `GetAll` that index is out
/// of range, so the following `QVariant::toString()` dereferences garbage and
/// the Qt5 app *segfaults* (observed crash: `libQt5Core` via
/// `AtSpiAdaptor::handleMessage`). Qt6's bridge handles `GetAll`, which is why
/// only Qt5 crashed. Forcing `No` makes zbus issue per-property `Get` calls
/// (two arguments) instead, which Qt5 handles correctly — so the Qt5 window can
/// be walked and written without killing the app. Other toolkits are
/// unaffected (they already tolerate `GetAll`), and the sub-interface proxies
/// from `proxies()` already use `CacheProperties::No`.
/// Build the proxy on the shared bus connection, never the peer socket.
async fn accessible_via_bus<'a>(
    conn: &AccessibilityConnection,
    oref: &RawObjectRef,
) -> Result<AccessibleProxy<'a>> {
    if !sandbox_gate_allows(conn.connection(), &oref.name).await {
        return Err(sandbox_refusal(&oref.name));
    }
    // Do not eagerly open every application's optional P2P accessibility
    // socket: one stale peer can block initialization for the whole driver.
    // Object references already carry an exact bus destination, so build the
    // central AT-SPI-bus proxy explicitly and let the per-call/operation
    // deadlines below fail closed for an unresponsive application.
    AccessibleProxy::builder(conn.connection())
        .cache_properties(atspi::zbus::proxy::CacheProperties::No)
        .destination(oref.name.clone())
        .map_err(|e| anyhow!("bad a11y destination: {e}"))?
        .path(oref.path.clone())
        .map_err(|e| anyhow!("bad a11y path: {e}"))?
        .build()
        .await
        .map_err(|e| anyhow!("AccessibleProxy build failed: {e}"))
}

async fn accessible_for<'a>(
    conn: &AccessibilityConnection,
    oref: &RawObjectRef,
) -> Result<AccessibleProxy<'a>> {
    // Always the session a11y bus. WebKitGTK's late WebProcess children use a
    // well-known name that only the explicit bus builder can address, and the
    // crate's peer-to-peer route is disabled (see Cargo.toml).
    accessible_via_bus(conn, oref).await
}

/// AT-SPI's `(so)` object references are documented as unique bus names, but
/// WebKitGTK uses its well-known WebProcess name for the embedded web tree.
/// Keep the wire values as strings while walking so zbus does not reject that
/// validly addressable well-known name before we can call it.
#[derive(Clone, Debug)]
struct RawObjectRef {
    name: String,
    path: String,
}

fn element_key_for_object(object: &RawObjectRef) -> u64 {
    // Stable FNV-1a over the exact AT-SPI destination + object path. Unlike the
    // snapshot ordinal, this survives unrelated tree insertions; a collision is
    // still detected by key-based operations before any mutation.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in object
        .name
        .as_bytes()
        .iter()
        .copied()
        .chain(std::iter::once(0))
        .chain(object.path.as_bytes().iter().copied())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

impl RawObjectRef {
    fn from_atspi(oref: &atspi::ObjectRefOwned) -> Option<Self> {
        Some(Self {
            name: oref.name_as_str()?.to_owned(),
            path: oref.path_as_str().to_owned(),
        })
    }
}

/// Containers with more children than this are not expanded. LibreOffice
/// Calc's spreadsheet accessible reports the whole sheet (rows x columns, in
/// the billions); a `GetChildren` on it makes the application enumerate every
/// cell, which stalls its accessibility bridge for minutes and wedges every
/// later call from this connection behind it. pyatspi/Orca never enumerate
/// such containers wholesale either. A caller-supplied `max_elements` above
/// this raises the cap (the walk cannot exceed the budget anyway).
pub(crate) const CHILD_ENUMERATION_CAP: usize = 2048;

/// Accessible.ChildCount: one cheap property read that tells a leaf (skip the
/// GetChildren round-trip entirely) from a container that must not be expanded.
async fn raw_child_count(conn: &atspi::zbus::Connection, oref: &RawObjectRef) -> Result<i32> {
    if !sandbox_gate_allows(conn, &oref.name).await {
        return Err(sandbox_refusal(&oref.name));
    }
    // `cache_properties(No)` is load-bearing (see `accessible_via_bus`): a
    // zbus proxy with the default lazy cache answers this one `ChildCount`
    // read with `Properties.GetAll`, which segfaults Qt5's AT-SPI bridge
    // (VLC died on the first get_window_state walk).
    let proxy = atspi::zbus::proxy::Builder::<atspi::zbus::Proxy<'_>>::new(conn)
        .cache_properties(atspi::zbus::proxy::CacheProperties::No)
        .destination(oref.name.as_str())
        .map_err(|e| anyhow!("bad a11y destination: {e}"))?
        .path(oref.path.as_str())
        .map_err(|e| anyhow!("bad a11y path: {e}"))?
        .interface("org.a11y.atspi.Accessible")
        .map_err(|e| anyhow!("bad a11y interface: {e}"))?
        .build()
        .await
        .map_err(|e| anyhow!("Accessible proxy unavailable: {e}"))?;
    proxy
        .get_property::<i32>("ChildCount")
        .await
        .map_err(|e| anyhow!("Accessible.ChildCount failed: {e}"))
}

/// Pin well-known names to their current unique owner. A missing owner is
/// discovery-only: indexed clicks cannot use an unproven persistent identity.
async fn identity_ref(
    conn: &AccessibilityConnection,
    raw: &RawObjectRef,
    owners: &mut std::collections::HashMap<String, Option<String>>,
) -> Option<RawObjectRef> {
    let owner = if raw.name.starts_with(':') {
        raw.name.clone()
    } else if let Some(owner) = owners.get(&raw.name) {
        owner.clone()?
    } else {
        let owner = async {
            let bus = atspi::zbus::fdo::DBusProxy::new(conn.connection())
                .await
                .ok()?;
            let name = atspi::zbus::names::BusName::try_from(raw.name.as_str()).ok()?;
            call(bus.get_name_owner(name))
                .await?
                .ok()
                .map(|name| name.to_string())
        }
        .await;
        owners.insert(raw.name.clone(), owner.clone());
        owner?
    };
    Some(RawObjectRef {
        name: owner,
        path: raw.path.clone(),
    })
}

/// Read Accessible.GetChildren without deserializing the bus-name field as a
/// `UniqueName`. WebKitGTK's embedded WebProcess exposes a well-known name
/// containing a UUID; D-Bus can address it, but the stricter AT-SPI wrapper
/// rejects it as an invalid unique name.
async fn raw_children(
    conn: &atspi::zbus::Connection,
    oref: &RawObjectRef,
) -> Result<Vec<RawObjectRef>> {
    if !sandbox_gate_allows(conn, &oref.name).await {
        return Err(sandbox_refusal(&oref.name));
    }
    let proxy = atspi::zbus::Proxy::new(
        conn,
        oref.name.as_str(),
        oref.path.as_str(),
        "org.a11y.atspi.Accessible",
    )
    .await
    .map_err(|e| anyhow!("Accessible proxy unavailable: {e}"))?;
    let refs: Vec<(String, atspi::zbus::zvariant::OwnedObjectPath)> = proxy
        .call("GetChildren", &())
        .await
        .map_err(|e| anyhow!("Accessible.GetChildren failed: {e}"))?;
    Ok(refs
        .into_iter()
        .map(|(name, path)| RawObjectRef {
            name,
            path: path.to_string(),
        })
        .collect())
}

/// Resolve the process id behind an application accessible's D-Bus name.
async fn pid_of(
    dbus: &atspi::zbus::fdo::DBusProxy<'_>,
    oref: &atspi::ObjectRefOwned,
) -> Option<u32> {
    let bus = atspi::zbus::names::BusName::try_from(oref.name_as_str()?.to_owned()).ok()?;
    dbus.get_connection_unix_process_id(bus).await.ok()
}

/// How a registry peer stands for the target process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    /// The peer is the target process itself.
    Exact,
    /// The peer is the proxy proven to serve the target's own Flatpak
    /// sandbox (its pid). Every client in that sandbox shares it, so it proves
    /// sandbox membership, not which process registered.
    SandboxProxy(u32),
}

/// Keep the first matching application as a compatibility fallback, but allow
/// a later registration with a real child tree to win. Some Qt processes
/// publish an empty application object before their populated one (#2678,
/// #2706).
struct ApplicationSelection<T> {
    target_pid: u32,
    /// For a Flatpak target: the proof a registry peer must pass to stand in
    /// for it, since its applications register through the sandbox's proxy.
    sandbox: Option<sandbox_peer::SandboxPeerProof>,
    exact_registrations: usize,
    proxy_registrations: usize,
    fallback: Option<T>,
    populated: Vec<T>,
}

impl<T> ApplicationSelection<T> {
    fn new(target_pid: u32) -> Self {
        Self {
            target_pid,
            sandbox: None,
            exact_registrations: 0,
            proxy_registrations: 0,
            fallback: None,
            populated: Vec::new(),
        }
    }

    fn with_sandbox(mut self, sandbox: Option<sandbox_peer::SandboxPeerProof>) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// The exact pid, or the proxy proven to serve the target's own Flatpak
    /// sandbox. Every other peer is someone else's application.
    fn classify(&self, candidate_pid: Option<u32>) -> Option<Admission> {
        match candidate_pid {
            Some(pid) if pid == self.target_pid => Some(Admission::Exact),
            Some(pid) => self
                .sandbox
                .as_ref()
                .is_some_and(|proof| proof.admits(pid))
                .then_some(Admission::SandboxProxy(pid)),
            None => None,
        }
    }

    #[cfg(test)]
    fn matches_pid(&self, candidate_pid: Option<u32>) -> bool {
        self.classify(candidate_pid).is_some()
    }

    /// Classify a registry peer and count its registration, whether or not
    /// the application then answers. A proxy proves only that the
    /// registration came from inside the target's sandbox, so selection
    /// through it requires it to be the only registration there is.
    fn admit(&mut self, candidate_pid: Option<u32>) -> Option<Admission> {
        let admission = self.classify(candidate_pid)?;
        match admission {
            Admission::Exact => self.exact_registrations += 1,
            Admission::SandboxProxy(_) => self.proxy_registrations += 1,
        }
        Some(admission)
    }

    /// Retain every populated exact-PID candidate so resolution can reject an
    /// ambiguous registry instead of silently choosing whichever entry sorted
    /// first. The first childless candidate remains the compatibility fallback
    /// for applications that genuinely expose no top-level accessibles.
    fn consider_matching(&mut self, candidate: T, has_children: bool) {
        if has_children {
            self.populated.push(candidate);
        } else if self.fallback.is_none() {
            self.fallback = Some(candidate);
        }
    }

    fn into_selected(mut self) -> std::result::Result<Option<T>, usize> {
        let registrations = self.exact_registrations + self.proxy_registrations;
        if self.proxy_registrations > 0 && registrations > 1 {
            return Err(registrations);
        }
        match self.populated.len() {
            0 => Ok(self.fallback),
            1 => Ok(self.populated.pop()),
            count => Err(count),
        }
    }
}

/// What a registry scan found for one target process.
enum AppLookup<'a> {
    Found(ResolvedApp<'a>),
    /// The registry answered in full and no application belongs to the process.
    Absent,
    /// The registry or a peer did not answer, so absence is not proven.
    Incomplete(&'static str),
}

/// An application accessible plus the top-level children its selection probe
/// already fetched (`None` when that probe timed out or failed), so callers do
/// not pay a second `GetChildren` round-trip — on an application that never
/// answers it, that was a second full CALL_TIMEOUT.
struct ResolvedApp<'a> {
    app: AccessibleProxy<'a>,
    children: Option<Vec<atspi::ObjectRefOwned>>,
    /// The sandbox proxy's pid when the application was admitted through it;
    /// every object walked from it must then live on that proxy's connection.
    sandbox_proxy: Option<u32>,
}

/// Locate the application accessible whose backing process is `pid` (exact
/// pid only: focus and hit-testing callers walk objects without the sandbox
/// ownership checks the snapshot walk applies).
async fn app_for_pid<'a>(
    conn: &AccessibilityConnection,
    pid: u32,
) -> Result<Option<AccessibleProxy<'a>>> {
    Ok(match resolve_app_for_pid(conn, pid, false).await? {
        AppLookup::Found(resolved) => Some(resolved.app),
        AppLookup::Absent | AppLookup::Incomplete(_) => None,
    })
}

/// Best-effort executable/process name for `pid`, read from `/proc/<pid>/comm`
/// (Linux only; falls back to `None` on any error, e.g. the process already
/// exited or `/proc` is unavailable in a sandbox).
fn process_name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `admit_sandbox`: also accept the proxy proven to serve `pid`'s Flatpak
/// sandbox (see [`sandbox_peer`]), for callers that enforce its ownership
/// rules on every object they walk.
async fn resolve_app_for_pid<'a>(
    conn: &AccessibilityConnection,
    pid: u32,
    admit_sandbox: bool,
) -> Result<AppLookup<'a>> {
    let zconn = conn.connection();
    // Every AT-SPI round-trip below can block on an app whose main loop isn't
    // servicing D-Bus — most commonly one holding a modal grab (an "Add/Edit/
    // Preferences" dialog). Without a bound, `GetConnectionUnixProcessID` stalls
    // on the zbus default (~25s) per such app, which made get_window_state and
    // type_text hang on real apps (#1936). Bound each step with CALL_TIMEOUT and
    // skip/return instead of stalling — for type_text this returns fast so the
    // tool falls back to XTEST, which still types into the focused dialog field.
    let root = match call(conn.root_accessible_on_registry()).await {
        Some(Ok(r)) => r,
        Some(Err(e)) => return Err(anyhow!("registry root unavailable: {e}")),
        None => {
            dlog!("registry root lookup timed out");
            return Ok(AppLookup::Incomplete("registry_root_timeout"));
        }
    };
    let dbus = atspi::zbus::fdo::DBusProxy::new(zconn)
        .await
        .map_err(|e| anyhow!("DBus proxy unavailable: {e}"))?;

    let apps = match call(root.get_children()).await {
        Some(Ok(apps)) => apps,
        Some(Err(error)) => {
            dlog!("registry get_children failed: {error:#}");
            return Ok(AppLookup::Incomplete("registry_children_failed"));
        }
        None => {
            dlog!("registry get_children timed out");
            return Ok(AppLookup::Incomplete("registry_children_timeout"));
        }
    };
    dlog!(
        "registry root has {} application(s); seeking pid {pid}",
        apps.len()
    );
    // Only the exact PID on the accessibility bus, or the proxy proven to serve
    // its own Flatpak sandbox, can supply this tree.
    let sandbox = admit_sandbox
        .then(|| sandbox_peer::SandboxPeerProof::for_target(pid))
        .flatten();
    let mut selection = ApplicationSelection::new(pid).with_sandbox(sandbox.clone());
    let mut incomplete = None;

    for child in apps {
        // A modal-grabbed app can't answer the pid query; skip it after
        // CALL_TIMEOUT rather than blocking the whole walk on it.
        let cpid = match call(pid_of(&dbus, &child)).await {
            Some(Some(pid)) => pid,
            // A failed or timed-out lookup leaves this peer's owner unknown: it
            // may be the target, so absence is no longer proven.
            Some(None) | None => {
                dlog!(
                    "  pid_of failed or timed out for bus={:?}, skipping",
                    child.name_as_str()
                );
                incomplete = Some("peer_pid_unknown");
                continue;
            }
        };
        dlog!("  app bus={:?} pid={cpid}", child.name_as_str());
        let Some(admission) = selection.admit(Some(cpid)) else {
            continue;
        };
        if let Admission::SandboxProxy(proxy) = admission {
            dlog!("  peer pid {proxy} is pid {pid}'s Flatpak accessibility proxy");
        }
        let sandbox_proxy = match admission {
            Admission::Exact => None,
            Admission::SandboxProxy(proxy) => Some(proxy),
        };
        let child = match RawObjectRef::from_atspi(&child) {
            Some(child) => child,
            None => continue,
        };
        let app = match call(accessible_for(conn, &child)).await {
            Some(Ok(app)) => app,
            Some(Err(error)) => {
                dlog!("  accessible_for failed for pid {pid}: {error:#}");
                incomplete = Some("application_unanswered");
                continue;
            }
            None => {
                dlog!("  accessible_for timed out for pid {pid}");
                incomplete = Some("application_unanswered");
                continue;
            }
        };
        let children = match call(app.get_children()).await {
            Some(Ok(children)) => Some(children),
            Some(Err(error)) => {
                dlog!("  get_children failed for pid {pid}: {error:#}");
                None
            }
            None => {
                dlog!("  get_children timed out for pid {pid}");
                None
            }
        };
        let has_children = children.as_ref().is_some_and(|c| !c.is_empty());
        dlog!("  matching app has_children={has_children}");
        selection.consider_matching(
            ResolvedApp {
                app,
                children,
                sandbox_proxy,
            },
            has_children,
        );
    }
    let selected = match selection.into_selected() {
        Ok(Some(app)) => app,
        Ok(None) => {
            return Ok(match incomplete {
                Some(reason) => AppLookup::Incomplete(reason),
                None => AppLookup::Absent,
            })
        }
        Err(count) => {
            return Err(anyhow!(
                "ambiguous AT-SPI application selection for pid {pid}: \
                 {count} application accessibles matched"
            ))
        }
    };
    if selected.sandbox_proxy.is_some() {
        // A proxy proves only that the registration came from inside the
        // sandbox. It stands for the target only when it was provably the one
        // registration there (no peer left unanswered) and no other process in
        // that sandbox owns a window it could belong to.
        if let Some(reason) = incomplete {
            return Ok(AppLookup::Incomplete(reason));
        }
        // Only the GNOME Shell helper's own window list is complete enough to
        // prove this: it lists every window actor, titled or not, with its
        // pid, and parses all or nothing. It runs off this task under the
        // walk's deadline. Elsewhere (X11, Sway, KWin, Hyprland), or when the
        // helper is missing, slow or fails, a sandbox registration is refused.
        let windows = if crate::wayland::is_wayland() {
            bounded_blocking(|| crate::wayland::shell_helper::list_windows(None))
                .await
                .flatten()
        } else {
            None
        };
        let shared = match (sandbox.as_ref(), windows) {
            (Some(proof), Some(windows)) => {
                !proof.owns_its_sandbox_windows(windows.iter().map(|window| window.pid))
            }
            _ => true,
        };
        if shared {
            return Err(anyhow!(
                "flatpak_sandbox_shared: pid {pid}'s sandbox has another window owner, or its \
                 windows could not be listed; its accessibility registration cannot be \
                 attributed to pid {pid}"
            ));
        }
    }
    Ok(AppLookup::Found(selected))
}

/// The pid behind a unique bus name on the accessibility bus.
async fn bus_owner_pid(dbus: &atspi::zbus::fdo::DBusProxy<'_>, unique_name: &str) -> Option<u32> {
    let bus = atspi::zbus::names::BusName::try_from(unique_name.to_owned()).ok()?;
    call(dbus.get_connection_unix_process_id(bus)).await?.ok()
}

/// Depth-first, pre-order walk of an application's windows. Mirrors the old
/// pyatspi `walk`/`collect` traversal so element indices stay stable.
#[allow(dead_code)]
async fn collect_visited<'a>(
    conn: &AccessibilityConnection,
    pid: u32,
) -> Result<Option<Vec<Visited<'a>>>> {
    collect_visited_bounded(conn, pid, 0, None, None)
        .await
        .map(|walked| walked.map(|collected| collected.visited))
}

/// Screen-space distance between an AT-SPI frame's extents and a native
/// window's geometry. Lower is a better correspondence; `None` when the frame
/// reports no usable extents.
fn frame_geometry_distance(
    frame: (i32, i32, i32, i32),
    window: &crate::x11::WindowInfo,
) -> Option<u64> {
    let (fx, fy, fw, fh) = frame;
    if fw <= 0 || fh <= 0 {
        return None;
    }
    let dx = i64::from(fx) - i64::from(window.x);
    let dy = i64::from(fy) - i64::from(window.y);
    let dw = i64::from(fw) - i64::from(window.width);
    let dh = i64::from(fh) - i64::from(window.height);
    Some(dx.unsigned_abs() + dy.unsigned_abs() + dw.unsigned_abs() + dh.unsigned_abs())
}

/// Server-side decorations offset a frame's reported origin from the native
/// window's outer geometry, so an exact match is not required. The correlation
/// must still be unambiguous: the best candidate has to be within this budget
/// AND beat the runner-up by [`FRAME_MATCH_MARGIN_PX`].
const FRAME_MATCH_TOLERANCE_PX: u64 = 160;

/// How decisively the best frame must beat the second-best. Two windows of
/// genuinely similar geometry are not disambiguated by this heuristic, and a
/// caller that needs proof of window identity must get a refusal instead of a
/// coin flip.
const FRAME_MATCH_MARGIN_PX: u64 = 24;

/// Pick the unique application top-level that corresponds to native window
/// `xid`, or `None` when the correspondence cannot be proven.
///
/// AT-SPI publishes one application per process: every window of a multi-window
/// app shares a single tree, and the protocol exposes no window handle to join
/// on. Geometry is the available bridge — `Component.GetExtents` in screen
/// coordinates against the X11 outer geometry the caller already named. This
/// refuses ties rather than guessing, because callers use the result to decide
/// which window they are about to act inside.
/// An AT-SPI frame ordinal and its screen extents (x, y, width, height).
type FrameExtents = (usize, (i32, i32, i32, i32));

fn correlate_frame_to_window(
    candidates: &[FrameExtents],
    window: &crate::x11::WindowInfo,
) -> Option<usize> {
    let mut scored: Vec<(u64, usize)> = candidates
        .iter()
        .filter_map(|(ordinal, extents)| {
            frame_geometry_distance(*extents, window).map(|distance| (distance, *ordinal))
        })
        .collect();
    scored.sort_by_key(|(distance, ordinal)| (*distance, *ordinal));
    let (best_distance, best_ordinal) = *scored.first()?;
    if best_distance > FRAME_MATCH_TOLERANCE_PX {
        return None;
    }
    if let Some((runner_up, _)) = scored.get(1) {
        if runner_up.saturating_sub(best_distance) < FRAME_MATCH_MARGIN_PX {
            return None;
        }
    }
    Some(best_ordinal)
}

#[derive(Debug)]
struct HyprlandFrame {
    ordinal: usize,
    title: String,
    browser_root: bool,
    size: Option<(i32, i32)>,
}

fn chromium_title_matches(accessible: &str, native: &str) -> bool {
    if native.is_empty() {
        return false;
    }
    accessible == native
        || accessible
            .strip_prefix(native)
            .and_then(|suffix| suffix.strip_prefix(" - "))
            .is_some_and(|profile| {
                !profile.trim().is_empty() && !profile.chars().any(char::is_control)
            })
}

fn correlate_hyprland_frame(
    frames: &[HyprlandFrame],
    clients: &[(&str, u32, u32)],
    target: usize,
) -> Option<usize> {
    let &(title, _, _) = clients.get(target)?;
    if title.is_empty() || clients.iter().filter(|client| client.0 == title).count() != 1 {
        return None;
    }
    let matches = |frame: &HyprlandFrame, client: &(&str, u32, u32)| {
        frame.title == client.0
            || (frame.browser_root
                && frame.size.is_some_and(|(width, height)| {
                    width > 0
                        && height > 0
                        && i64::from(width) == i64::from(client.1)
                        && i64::from(height) == i64::from(client.2)
                })
                && chromium_title_matches(&frame.title, client.0))
    };
    let mut candidates = frames
        .iter()
        .filter(|frame| matches(frame, &clients[target]));
    let frame = candidates.next()?;
    if candidates.next().is_some()
        || clients
            .iter()
            .filter(|client| matches(frame, client))
            .count()
            != 1
    {
        return None;
    }
    Some(frame.ordinal)
}

#[cfg(test)]
mod hyprland_frame_tests {
    use super::*;

    fn frame(title: &str) -> HyprlandFrame {
        HyprlandFrame {
            ordinal: 7,
            title: title.into(),
            browser_root: true,
            size: Some((800, 600)),
        }
    }

    #[test]
    fn binds_unique_chromium_profile_title() {
        assert_eq!(
            correlate_hyprland_frame(
                &[frame("Page - Google Chrome - Cua Test")],
                &[("Page - Google Chrome", 800, 600), ("Other", 800, 600)],
                0,
            ),
            Some(7)
        );
    }

    #[test]
    fn rejects_generic_prefixes_unknown_roots_and_wrong_sizes() {
        assert!(!chromium_title_matches("Pageant", "Page"));
        assert!(!chromium_title_matches("Page - ", "Page"));
        let mut candidate = frame("Page - Google Chrome - Cua Test");
        candidate.browser_root = false;
        assert_eq!(
            correlate_hyprland_frame(&[candidate], &[("Page - Google Chrome", 800, 600)], 0),
            None
        );
        assert_eq!(
            correlate_hyprland_frame(
                &[frame("Page - Google Chrome - Cua Test")],
                &[("Page - Google Chrome", 801, 600)],
                0,
            ),
            None
        );
    }

    #[test]
    fn rejects_ambiguous_native_and_accessibility_candidates() {
        assert_eq!(
            correlate_hyprland_frame(
                &[frame("Page - Google Chrome - Cua Test")],
                &[
                    ("Page - Google Chrome", 800, 600),
                    ("Page - Google Chrome - Cua Test", 800, 600),
                ],
                0,
            ),
            None
        );
        assert_eq!(
            correlate_hyprland_frame(
                &[
                    frame("Page - Google Chrome"),
                    frame("Page - Google Chrome - Cua Test"),
                ],
                &[("Page - Google Chrome", 800, 600)],
                0,
            ),
            None
        );
    }

    #[test]
    fn preserves_exact_non_chromium_matching() {
        let mut candidate = frame("Editor");
        candidate.browser_root = false;
        candidate.size = None;
        assert_eq!(
            correlate_hyprland_frame(&[candidate], &[("Editor", 800, 600)], 0),
            Some(7)
        );
        assert_eq!(
            correlate_hyprland_frame(&[frame("")], &[("", 800, 600)], 0),
            None
        );
    }
}

/// Resolve native window `xid` to the ordinal of the application top-level that
/// renders it, or `None` when that cannot be proven. `None` means the walk stays
/// application-wide: callers that merely want a tree carry on, and callers that
/// need window identity must refuse.
async fn resolve_window_frame(
    conn: &AccessibilityConnection,
    pid: u32,
    xid: u64,
    seeds: &[RawObjectRef],
) -> Option<usize> {
    if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
        let windows = crate::wayland::hyprland::list_windows().ok()?;
        let owned: Vec<_> = windows.iter().filter(|window| window.pid == pid).collect();
        let target = owned.iter().position(|window| window.address == xid)?;
        let chromium = std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .is_some_and(|name| crate::browser_platform::is_chromium_identity(&name));
        let mut frames = Vec::new();
        for (ordinal, oref) in seeds.iter().enumerate() {
            let acc = call(accessible_for(conn, oref)).await?.ok()?;
            let role = call(acc.get_role_name()).await?.ok()?;
            if !matches!(
                role.as_str(),
                "frame" | "window" | "dialog" | "alert" | "file chooser"
            ) {
                continue;
            }
            let title = call(acc.name()).await?.ok()?;
            let browser_root = if chromium {
                call(acc.get_attributes())
                    .await?
                    .ok()?
                    .get("class")
                    .is_some_and(|class| class == "BrowserRootView")
            } else {
                false
            };
            let size = if browser_root {
                let proxies = call(acc.proxies()).await?.ok()?;
                let component = call(proxies.component()).await?.ok()?;
                let (_, _, width, height) =
                    call(component.get_extents(CoordType::Screen)).await?.ok()?;
                Some((width, height))
            } else {
                None
            };
            frames.push(HyprlandFrame {
                ordinal,
                title,
                browser_root,
                size,
            });
        }
        let clients = owned
            .iter()
            .map(|window| (window.title.as_str(), window.width, window.height))
            .collect::<Vec<_>>();
        return correlate_hyprland_frame(&frames, &clients, target);
    }
    if seeds.len() == 1 {
        // One top-level: on X11, and on compositors without a GNOME Shell,
        // the caller's window is the only thing this application could be
        // showing, and no geometry round-trip can make that more certain.
        // GNOME's AT-SPI tree can omit a window the Shell has mapped, so
        // there the Shell must vouch for it (`lone_top_level_renders`).
        let proven = !crate::wayland::is_wayland()
            || bounded_blocking(move || lone_top_level_proves_window(pid, xid))
                .await
                .unwrap_or(false);
        if proven {
            return Some(0);
        }
    }
    let windows = if crate::wayland::is_wayland() {
        crate::wayland::list_windows_dispatch(Some(pid))
    } else {
        crate::x11::list_windows(Some(pid))
    };
    let window = windows.into_iter().find(|candidate| candidate.xid == xid)?;
    let popup = !crate::wayland::is_wayland() && crate::input::popup_window_info(xid).is_some();
    let mut candidates: Vec<(usize, (i32, i32, i32, i32))> = Vec::new();
    for (ordinal, oref) in seeds.iter().enumerate() {
        let Some(Ok(acc)) = call(accessible_for(conn, oref)).await else {
            continue;
        };
        // Menus, tooltips and other transients are top-level accessibles too;
        // only real windows can correspond to a managed native window id.
        let role = match call(acc.get_role_name()).await {
            Some(Ok(role)) => role,
            _ => continue,
        };
        let real_window = matches!(
            role.as_str(),
            "frame" | "window" | "dialog" | "alert" | "file chooser"
        );
        let popup_toplevel = matches!(role.as_str(), "window" | "menu" | "popup menu");
        if !(real_window || (popup && popup_toplevel)) {
            continue;
        }
        let Some(Ok(proxies)) = call(acc.proxies()).await else {
            continue;
        };
        let Some(Ok(component)) = call(proxies.component()).await else {
            continue;
        };
        if let Some(Ok(extents)) = call(component.get_extents(CoordType::Screen)).await {
            candidates.push((ordinal, extents));
        }
    }
    let resolved = correlate_frame_to_window(&candidates, &window);
    if resolved.is_none() {
        dlog!(
            "could not correlate xid {xid} to one of pid {pid}'s {} top-level frame(s); \
             walk stays application-scoped",
            candidates.len()
        );
    }
    resolved
}

/// `collect_visited` with caller-supplied caps.
/// - `max_elements = None` keeps the historical 5 000-node budget.
/// - `max_depth = None` keeps depth uncapped (the historical behaviour);
///   `Some(d)` skips enqueueing children whose depth would exceed `d`.
///
/// Issue #22865: caps protect against Electron / large web apps that produce
/// 10k+ element trees and blow context windows.
async fn collect_visited_bounded<'a>(
    conn: &AccessibilityConnection,
    pid: u32,
    xid: u64,
    max_elements: Option<usize>,
    max_depth: Option<usize>,
) -> Result<Option<Collected<'a>>> {
    collect_visited_bounded_opts(conn, pid, xid, max_elements, max_depth, false).await
}

/// [`collect_visited_bounded`] with `walk_closed_menus`: a targeted walk
/// (invoke_menu resolving a path segment by segment) descends into menus
/// the snapshot walk keeps collapsed, since a toolkit that never marks its
/// entries SHOWING (gail) cannot show the walk that a `doAction` opened one.
async fn collect_visited_bounded_opts<'a>(
    conn: &AccessibilityConnection,
    pid: u32,
    xid: u64,
    max_elements: Option<usize>,
    max_depth: Option<usize>,
    walk_closed_menus: bool,
) -> Result<Option<Collected<'a>>> {
    // Each walk gets its own gate scope, closed until a sandbox admission
    // opens it, so no other walk inherits it.
    SANDBOX_GATE
        .scope(
            std::cell::RefCell::new(None),
            collect_visited_in_gate(conn, pid, xid, max_elements, max_depth, walk_closed_menus),
        )
        .await
}

async fn collect_visited_in_gate<'a>(
    conn: &AccessibilityConnection,
    pid: u32,
    xid: u64,
    max_elements: Option<usize>,
    max_depth: Option<usize>,
    walk_closed_menus: bool,
) -> Result<Option<Collected<'a>>> {
    let ResolvedApp {
        app,
        children: probed,
        sandbox_proxy,
    } = match resolve_app_for_pid(conn, pid, true).await? {
        AppLookup::Found(a) => a,
        lookup => {
            if let AppLookup::Incomplete(reason) = lookup {
                let _ = REGISTRY_INCOMPLETE.try_with(|cell| cell.set(Some(reason)));
            }
            if deadline_passed() {
                // The registry / application did not answer inside the caller's
                // budget. Distinguish that from "no such application" so the
                // caller can report a timeout instead of an absent tree.
                return Err(anyhow!("app_lookup_timeout"));
            }
            return Ok(None);
        }
    };
    // Chromium lazily enables accessibility on GetAttributes. Cache only a
    // successful wake of this exact process and application on this bus;
    // PID reuse, bus restart and failed reads must remain retryable.
    let wake_key = crate::proc_fs::process_instance_id(pid).map(|incarnation| ApplicationWakeKey {
        pid,
        incarnation,
        bus: conn.connection().server_guid().to_string(),
        destination: app.inner().destination().to_string(),
        path: app.inner().path().to_string(),
    });
    if wake_key
        .as_ref()
        .is_none_or(|key| !application_wakes().lock().unwrap().contains(key))
        && matches!(call(app.get_attributes()).await, Some(Ok(_)))
    {
        if let Some(key) = wake_key {
            application_wakes().lock().unwrap().remember(key);
        }
    }
    let zconn = conn.connection();
    // Mapped popups of this pid, read once and only when a menubar menu's
    // open state cannot be told from AT-SPI states alone (gail, focus elsewhere).
    let mut pid_popups: Option<Vec<crate::input::PopupWindow>> = None;

    // Keep a single immutable seed list: window correlation and traversal must
    // refer to the same top-level ordering.
    let app_answered = probed.is_some();
    let mut seeds: Vec<RawObjectRef> = probed
        .unwrap_or_default()
        .into_iter()
        .filter_map(|child| RawObjectRef::from_atspi(&child))
        .collect();
    // An application admitted through its sandbox's proxy may only contribute
    // objects on that proxy's own connection: from here on every object this
    // walk addresses (seeds, descendants, relation targets, menu peeks, child
    // counts) passes `sandbox_gate_allows`, and a reference to another
    // application's objects is never followed.
    if let Some(proxy) = sandbox_proxy {
        let opened = SANDBOX_GATE.try_with(|gate| {
            *gate.borrow_mut() = Some(SandboxGate {
                proxy,
                verdicts: std::collections::HashMap::new(),
            });
        });
        if opened.is_err() {
            return Err(anyhow!("sandbox gate unavailable for pid {pid}"));
        }
        let zconn = conn.connection();
        let mut kept = Vec::with_capacity(seeds.len());
        for seed in seeds {
            if sandbox_gate_allows(zconn, &seed.name).await {
                kept.push(seed);
            } else {
                dlog!(
                    "  sandboxed seed {:?} is not on its proxy's connection; skipped",
                    seed.name
                );
            }
        }
        seeds = kept;
    }
    if !app_answered {
        // The application registered with AT-SPI but never answered
        // `GetChildren`: LibreOffice with its bridge half-initialised, or a
        // modal-grabbed app. Report it as unresponsive rather than as an
        // application without windows — the caller can retry later.
        dlog!("application for pid {pid} did not answer GetChildren");
        return Ok(Some(Collected {
            visited: Vec::new(),
            scoped_frame: None,
            truncation: Some(Truncation {
                reason: "app_unresponsive",
                visited: 0,
                pending: 0,
            }),
        }));
    }

    let scoped_frame = if xid == 0 {
        None
    } else {
        resolve_window_frame(conn, pid, xid, &seeds).await
    };

    // Keep seed identities for X11 and stable keys for exact Wayland actions.
    let mut stack: Vec<(RawObjectRef, usize, bool, u64, usize)> = seeds
        .iter()
        .cloned()
        .enumerate()
        .map(|(frame_ordinal, root)| {
            let top_level_key = element_key_for_object(&root);
            (root, 0usize, false, top_level_key, frame_ordinal)
        })
        .rev()
        .collect();
    // Direct children of a `menu bar`: a top-level menu whose popup is not
    // open keeps its child count but is not descended into.
    let mut menubar_children: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    let mut visited: Vec<Visited<'a>> = Vec::new();
    let mut identity_owners = std::collections::HashMap::new();
    // Guard against pathological/looping trees. Defaults to 5 000 (the
    // historical hard-coded budget); callers can override via max_elements.
    let mut budget = max_elements.unwrap_or(5000usize);
    // Time budget alongside the node budget: when an app is unresponsive to
    // AT-SPI (most commonly because it holds a modal grab and isn't servicing
    // D-Bus), every per-node `call()` burns the full CALL_TIMEOUT before being
    // skipped, so the walk would otherwise grind for minutes. Callers that lack
    // their own OP_TIMEOUT (snapshot bounds, insert_text) relied on this
    // never happening — bound it here so the walk returns partial within
    // OP_TIMEOUT for every caller, instead of hanging get_window_state/type_text
    // on modal dialogs (#1936).
    // The caller's operation deadline (when one is in scope) wins over the
    // historical OP_TIMEOUT so a 1 s tool budget bounds the whole walk.
    let deadline = {
        let legacy = tokio::time::Instant::now() + OP_TIMEOUT;
        OP_DEADLINE.try_with(|d| (*d).min(legacy)).unwrap_or(legacy)
    };
    // Fast bail for an app that has stopped answering AT-SPI entirely (modal
    // grab): if several consecutive nodes each burn the full CALL_TIMEOUT, the
    // app is unresponsive and the remaining ~OP_TIMEOUT of walking would all
    // time out too. Give up after a few so type_text falls back to XTEST in a
    // few seconds rather than ~25s.
    let mut consecutive_timeouts = 0u32;
    let mut truncation: Option<&'static str> = None;
    // Containers left unexpanded because their child count exceeded the cap:
    // (child count, position in `visited` of the container).
    let mut huge_containers: Vec<(usize, usize)> = Vec::new();
    let child_cap = max_elements
        .map(|cap| cap.max(CHILD_ENUMERATION_CAP))
        .unwrap_or(CHILD_ENUMERATION_CAP);

    // Positive evidence is scoped to an application top-level, never the PID:
    // a showing document in another browser window says nothing about this one.
    let mut showing_documents = std::collections::HashMap::<usize, RawObjectRef>::new();
    let mut document_peeked = std::collections::HashSet::new();

    while let Some((oref, depth, inherited_web_doc, top_level_key, frame_ordinal)) = stack.pop() {
        if budget == 0 {
            dlog!("node budget exhausted; truncating walk");
            truncation = Some("node_budget");
            stack.push((oref, depth, inherited_web_doc, top_level_key, frame_ordinal));
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            dlog!("collect_visited time budget exhausted; returning partial walk");
            truncation = Some("timeout");
            stack.push((oref, depth, inherited_web_doc, top_level_key, frame_ordinal));
            break;
        }
        // WebKitGTK publishes its embedded page on a distinct WebProcess
        // D-Bus peer and can expose blank role names for the entire subtree.
        // The peer identity is therefore the reliable document boundary when
        // role-based AT-SPI discovery cannot identify one.
        let in_web_doc = inherited_web_doc || is_web_process_bus(&oref.name);

        // accessible_for builds a proxy whose first use round-trips to the
        // target app. On a modal-grabbed (AT-SPI-unresponsive) app this is the
        // await that actually hangs, so it MUST carry the per-call timeout —
        // otherwise the loop never returns to the deadline check at the top and
        // the walk stalls past OP_TIMEOUT for callers without an outer guard
        // (snapshot bounds, insert_text). That was the residual #1936 hang.
        if !sandbox_gate_allows(zconn, &oref.name).await {
            dlog!(
                "  sandboxed object on {:?} is not on its proxy's connection; skipped",
                oref.name
            );
            continue;
        }
        let object_identity = identity_ref(conn, &oref, &mut identity_owners).await;
        let frame_identity = identity_ref(conn, &seeds[frame_ordinal], &mut identity_owners).await;
        let acc = match call(accessible_for(
            conn,
            object_identity.as_ref().unwrap_or(&oref),
        ))
        .await
        {
            Some(Ok(a)) => a,
            Some(Err(error)) => {
                dlog!("  accessible_for failed: {error:#}");
                continue;
            }
            None => {
                if tokio::time::Instant::now() >= deadline {
                    truncation = Some("timeout");
                    stack.push((oref, depth, inherited_web_doc, top_level_key, frame_ordinal));
                    break;
                }
                consecutive_timeouts += 1;
                if consecutive_timeouts >= 3 {
                    dlog!(
                        "{} consecutive AT-SPI timeouts (accessible_for); app unresponsive, bailing walk",
                        consecutive_timeouts
                    );
                    truncation = Some("app_unresponsive");
                    stack.push((oref, depth, inherited_web_doc, top_level_key, frame_ordinal));
                    break;
                }
                continue;
            }
        };

        // Interfaces gate every other query; if even this times out the node is
        // unreachable, so skip it rather than stall.
        let ifaces = match call(acc.get_interfaces()).await {
            Some(Ok(i)) => {
                consecutive_timeouts = 0;
                i
            }
            // A completed-but-errored call is node-specific; keep walking.
            Some(Err(error)) => {
                dlog!("  get_interfaces failed: {error:#}");
                continue;
            }
            // A timeout means the app didn't answer in CALL_TIMEOUT. A run of
            // these means the whole app is wedged — bail so callers fall back.
            None => {
                if tokio::time::Instant::now() >= deadline {
                    truncation = Some("timeout");
                    stack.push((oref, depth, inherited_web_doc, top_level_key, frame_ordinal));
                    break;
                }
                consecutive_timeouts += 1;
                if consecutive_timeouts >= 3 {
                    dlog!(
                        "{} consecutive AT-SPI timeouts; app unresponsive, bailing walk",
                        consecutive_timeouts
                    );
                    truncation = Some("app_unresponsive");
                    stack.push((oref, depth, inherited_web_doc, top_level_key, frame_ordinal));
                    break;
                }
                continue;
            }
        };
        let has_action = ifaces.contains(Interface::Action);
        let has_editable = ifaces.contains(Interface::EditableText);
        let has_value = ifaces.contains(Interface::Value);
        let has_component = ifaces.contains(Interface::Component);
        let has_text = ifaces.contains(Interface::Text);

        // A toplevel transient of role `tool tip` is never read or entered:
        // gail (GTK2, e.g. GIMP 2.10) answers `name` on a tooltip-role popup
        // whose child is not a plain label with a `g_message` ("ATK_ROLE_TOOLTIP
        // object found, but doesn't look like a tooltip"), which GIMP shows as
        // a "GIMP Message" dialog. Such a popup exists only after the real
        // pointer hovered a widget (foreground delivery); it carries no
        // actionable element, so skipping it changes nothing else. One extra
        // round-trip per toplevel, not per node.
        if depth == 0 {
            if let Some(Ok(role)) = call(acc.get_role_name()).await {
                if is_tooltip_role(&role) {
                    dlog!("skipping tooltip toplevel {}", oref.path);
                    continue;
                }
            }
        }
        // These four are independent — issue them concurrently to cut the
        // per-node round-trip cost (large trees like Chromium's have hundreds
        // of nodes, so sequential reads dominate the walk time).
        let (role_r, name_r, state_r, count_r) = tokio::join!(
            call(acc.get_role_name()),
            call(acc.name()),
            call(acc.get_state()),
            call(raw_child_count(zconn, &oref)),
        );
        let role = match role_r {
            Some(Ok(r)) => r,
            _ => String::new(),
        };
        let top_level_document = !in_web_doc && tab_documents::is_tab_document(&role);
        let mut background_document = false;
        if top_level_document {
            if let Some(Ok(state)) = state_r.as_ref() {
                if state.contains(State::Showing) {
                    showing_documents.insert(frame_ordinal, oref.clone());
                } else {
                    // Only once per window, and only when a known non-showing
                    // document would otherwise consume the selected tab's cap.
                    // The pending stack carries ancestor context: never inspect
                    // a nested document or another top-level window as a peer.
                    if !showing_documents.contains_key(&frame_ordinal)
                        && document_peeked.insert(frame_ordinal)
                    {
                        let pending = stack
                            .iter()
                            .filter(|(_, _, web, _, frame)| !web && *frame == frame_ordinal)
                            .map(|(object, ..)| object.clone())
                            .collect();
                        if let Some(witness) =
                            tab_documents::find_showing(conn, pending, deadline).await
                        {
                            showing_documents.insert(frame_ordinal, witness);
                        }
                    }
                    if let Some(witness) = showing_documents.get(&frame_ordinal) {
                        background_document = tab_documents::still_showing(conn, witness).await;
                    }
                }
            }
        }
        // Fetch the child list only for real containers within the cap: leaves
        // (the majority of nodes) save a round-trip, and a huge container is
        // never enumerated (see CHILD_ENUMERATION_CAP). Keep a background
        // document's root and count, but do not even enumerate its descendants.
        let background_child_count = background_document.then(|| {
            count_r
                .as_ref()
                .and_then(|count| count.as_ref().ok())
                .copied()
                .unwrap_or(0)
                .max(0) as usize
        });
        let children_r: Option<Result<Vec<RawObjectRef>>> = match count_r {
            _ if background_document => Some(Ok(Vec::new())),
            Some(Ok(count)) if count <= 0 => Some(Ok(Vec::new())),
            Some(Ok(count)) if count as usize > child_cap => {
                huge_containers.push((count as usize, visited.len()));
                Some(Ok(Vec::new()))
            }
            Some(Ok(_)) => call(raw_children(zconn, &oref)).await,
            // Count unavailable (older bridges, error): fall back to the
            // historical unconditional fetch.
            Some(Err(_)) => call(raw_children(zconn, &oref)).await,
            None => None,
        };
        let mut name = match name_r {
            Some(Ok(n)) => n,
            _ => String::new(),
        };
        let focused = matches!(state_r.as_ref(), Some(Ok(s)) if s.contains(State::Focused));
        let role_lower = role.to_ascii_lowercase();
        let checked = if role_lower.contains("check") {
            state_r
                .as_ref()
                .and_then(|state| state.as_ref().ok())
                .map(|state| state.contains(State::Checked))
        } else {
            None
        };
        let enabled = state_r
            .as_ref()
            .and_then(|state| state.as_ref().ok())
            .map(is_enabled_state);
        let selectable = state_r
            .as_ref()
            .and_then(|state| state.as_ref().ok())
            .is_some_and(|state| state.contains(State::Selectable));
        let (showing, offscreen) = state_r
            .as_ref()
            .and_then(|state| state.as_ref().ok())
            .map_or((true, false), |state| listing_state(&role_lower, state));
        let expanded = state_r
            .as_ref()
            .and_then(|state| state.as_ref().ok())
            .is_some_and(|state| state.contains(State::Expanded));
        let under_menubar = menubar_children.remove(&(oref.name.clone(), oref.path.clone()));
        let selected = if role_lower.contains("check") {
            checked
        } else if role_lower.contains("radio")
            || role_lower.contains("list item")
            || role_lower.contains("menu item")
            || matches!(role_lower.as_str(), "tab" | "page tab" | "tab item")
        {
            state_r
                .as_ref()
                .and_then(|state| state.as_ref().ok())
                .map(|state| state.contains(State::Selected) || state.contains(State::Checked))
        } else {
            None
        };

        // Collect action names, numeric value, and (crucially) Text-interface
        // content. Only touch `proxies` when an interface is actually present,
        // and drop the borrow before `acc` moves into `visited`.
        let mut actions: Vec<String> = Vec::new();
        let mut value: Option<String> = None;
        let mut text_content = String::new();
        if has_action || has_value || has_text {
            if let Some(Ok(proxies)) = call(acc.proxies()).await {
                if has_action {
                    if let Some(Ok(ap)) = call(proxies.action()).await {
                        let n = call(ap.n_actions()).await.and_then(|r| r.ok()).unwrap_or(0);
                        for i in 0..n {
                            // Preserve the AT-SPI action index even when an
                            // individual name lookup fails. `do_action` takes
                            // this original index, so compacting the vector
                            // could otherwise actuate a different action than
                            // the name we selected.
                            push_action_name_slot(
                                &mut actions,
                                call(ap.get_name(i)).await.and_then(|result| result.ok()),
                            );
                        }
                    }
                }
                if has_value {
                    if let Some(Ok(vp)) = call(proxies.value()).await {
                        value = call(vp.current_value())
                            .await
                            .and_then(|r| r.ok())
                            .map(format_value);
                    }
                }
                // Text content is where editable/entry text (the typed string)
                // lives; `name` is usually empty for such widgets.
                if has_text {
                    if let Some(Ok(tp)) = call(proxies.text()).await {
                        let count = call(tp.character_count())
                            .await
                            .and_then(|r| r.ok())
                            .unwrap_or(0);
                        if count > 0 {
                            let end = count.min(4096);
                            if let Some(Ok(t)) = call(tp.get_text(0, end)).await {
                                text_content = t;
                            }
                        }
                    }
                }
            }
        }

        // An editable text widget's typed content is its value, reported
        // separately from its name (parity with macOS AXValue and the Windows
        // ValuePattern). Without it a named entry ("Note") hides what it holds
        // from `elements[].value` and `verify_state`.
        // A password field's content is never surfaced.
        let editable_text = has_editable && !role_lower.contains("password");
        value = editable_text_value(editable_text, has_value, value, &text_content);

        // Surface Text content as the display name when the widget has no
        // name — for text widgets. A value control (GtkSpinButton implements
        // AtkText with its formatted number) would otherwise be named after
        // its own value, and four spin buttons then all read "0.0".
        if name.trim().is_empty() && !text_content.trim().is_empty() && !has_value {
            name = text_content;
        }

        // AT-SPI `Description` for controls (one `Get` per actionable node,
        // never for passive containers/labels): Qt reports tooltips there.
        let is_control = has_action
            || has_value
            || has_editable
            || (has_component && is_control_role(&role_lower));
        let mut description = String::new();
        if is_control {
            if let Some(Ok(text)) = call(acc.description()).await {
                description = text.trim().to_owned();
            }
        }
        // An unnamed value/editable control: its `LABELLED_BY` relation names
        // it (GTK grids, Qt forms with buddies). One relation read plus one
        // name read, only for the controls that need it.
        if name.trim().is_empty() && (has_value || has_editable || is_control_role(&role_lower)) {
            if let Some(label) = labelled_by_name(conn, &acc).await {
                name = label;
            }
        }

        // Children inherit web-document context, plus this node's own role.
        let child_in_web_doc = in_web_doc || is_document_role(&role);

        // Enqueue children (fetched above) before moving `acc` into `visited`.
        // Honor max_depth (#22865): skip enqueueing descendants whose depth
        // would exceed the cap.
        let descend = max_depth.map(|d| depth < d).unwrap_or(true);
        // A menu that is not open (a menubar entry that is not expanded, or
        // any menu that is not showing) keeps its child count and is not
        // walked: its items are hidden, never indexed, and cost a round-trip
        // each. Open menus (expanded / showing popup contents) still descend.
        let mut closed_menu = !walk_closed_menus
            && role_lower == "menu"
            && (!showing || (under_menubar && !expanded));
        if closed_menu && showing && under_menubar {
            // LibreOffice VCL never sets EXPANDED on an open menubar menu; its
            // items are simply SHOWING. gail (GTK2, e.g. GIMP 2.10) never sets
            // EXPANDED either and never marks a menu entry SHOWING, open or
            // not (its entries are VISIBLE only, closed or open, and the
            // leading tearoff item carries no state at all); what it does
            // publish is FOCUSED on the menubar menu that is popped up. So
            // an open menu is one that is FOCUSED itself, or whose leading
            // children are SHOWING: a few extra state reads per menubar
            // menu. Everything else stays collapsed.
            if focused {
                closed_menu = false;
            } else if let Some(Ok(children)) = &children_r {
                let mut leading: Vec<AccessibleProxy<'_>> = Vec::new();
                for child_ref in children.iter().take(MENU_PEEK_CHILDREN) {
                    if let Some(Ok(child)) = call(accessible_for(conn, child_ref)).await {
                        if let Some(Ok(state)) = call(child.get_state()).await {
                            if is_showing_state(&state) {
                                closed_menu = false;
                                break;
                            }
                        }
                        leading.push(child);
                    }
                }
                if closed_menu {
                    // gail with the focus elsewhere (the caller's window is
                    // not active, so the popped-up menu is not FOCUSED): the
                    // open menu is a mapped override-redirect window of this
                    // pid, and its entries' screen extents lie inside it.
                    if pid_popups.is_none() {
                        let popups =
                            cua_driver_core::blocking::spawn(crate::input::mapped_popup_windows)
                                .await
                                .unwrap_or_default()
                                .into_iter()
                                .filter(|popup| popup.pid.is_none_or(|popup_pid| popup_pid == pid))
                                .collect::<Vec<_>>();
                        pid_popups = Some(popups);
                    }
                    if let Some(popups) = pid_popups.as_ref().filter(|popups| !popups.is_empty()) {
                        for child in &leading {
                            let Some(Ok(proxies)) = call(child.proxies()).await else {
                                continue;
                            };
                            let Some(Ok(component)) = call(proxies.component()).await else {
                                continue;
                            };
                            let Some(Ok((ex, ey, ew, eh))) =
                                call(component.get_extents(CoordType::Screen)).await
                            else {
                                continue;
                            };
                            if ew <= 0 || eh <= 0 || ex < -100_000 || ey < -100_000 {
                                continue;
                            }
                            let (cx, cy) = (ex + ew / 2, ey + eh / 2);
                            if popups.iter().any(|popup| {
                                cx >= popup.x
                                    && cy >= popup.y
                                    && cx < popup.x + popup.width as i32
                                    && cy < popup.y + popup.height as i32
                            }) {
                                closed_menu = false;
                                break;
                            }
                        }
                    }
                }
            }
        }
        let mut collapsed_children = background_child_count;
        if descend && closed_menu {
            if let Some(Ok(children)) = &children_r {
                collapsed_children = Some(children.len());
            }
        }
        if descend && !closed_menu {
            match children_r {
                Some(Ok(children)) => {
                    if role_lower == "menu bar" {
                        for c in &children {
                            menubar_children.insert((c.name.clone(), c.path.clone()));
                        }
                    }
                    for c in children.into_iter().rev() {
                        stack.push((c, depth + 1, child_in_web_doc, top_level_key, frame_ordinal));
                    }
                }
                Some(Err(error)) => dlog!("  get_children failed: {error:#}"),
                None => dlog!("  get_children timed out"),
            }
        }

        // Only nodes the caller can act on count against `max_elements`, so
        // a budget of N returns N elements rather than N visited containers.
        if showing
            && is_indexable_capabilities(
                &role,
                !actions.is_empty(),
                has_editable,
                has_value,
                selectable,
                has_component,
                enabled,
            )
        {
            budget -= 1;
        }
        visited.push(Visited {
            depth,
            role,
            name,
            description,
            value,
            checked,
            enabled,
            selected,
            selectable,
            showing,
            offscreen,
            collapsed_children,
            actions,
            has_editable,
            has_value,
            has_component,
            focused,
            in_web_doc,
            on_web_process_bus: is_web_process_bus(&oref.name),
            element_key: element_key_for_object(&oref),
            top_level_key,
            frame_ordinal,
            object_ref: ObjectRef {
                bus: oref.name.clone(),
                path: oref.path.clone(),
            },
            identity: object_identity
                .zip(frame_identity)
                .map(|(object, frame)| AtspiIdentity {
                    bus_name: object.name,
                    path: object.path,
                    frame_bus_name: frame.name,
                    frame_path: frame.path,
                }),
            acc,
        });
    }

    if truncation.is_none() && !huge_containers.is_empty() {
        for (count, at) in &huge_containers {
            dlog!(
                "container {:?} at node {at} has {count} children; not expanded",
                visited.get(*at).map(|v| v.role.as_str()).unwrap_or("?")
            );
        }
        truncation = Some("huge_container");
    }
    let truncation = truncation.map(|reason| Truncation {
        reason,
        visited: visited.len(),
        pending: stack.len(),
    });
    dlog!(
        "walked pid {pid}: {} node(s), truncation={:?}",
        visited.len(),
        truncation
    );
    Ok(Some(Collected {
        visited,
        scoped_frame,
        truncation,
    }))
}

/// Roles whose Description / LABELLED_BY are worth a round-trip even when
/// the node exposes no Action / Value / EditableText interface.
fn is_control_role(role_lower: &str) -> bool {
    matches!(
        role_lower,
        "button"
            | "push button"
            | "toggle button"
            | "combo box"
            | "spin button"
            | "slider"
            | "check box"
            | "radio button"
            | "entry"
            | "text"
            | "password text"
    )
}

/// Value controls the model tells apart by their label; when nothing names
/// one, the tree says so instead of repeating the value as its name.
fn is_value_control_role(role_lower: &str) -> bool {
    matches!(
        role_lower,
        "spin button" | "slider" | "scroll bar" | "progress bar" | "combo box" | "entry" | "text"
    )
}

/// The name of the first `LABELLED_BY` target of `acc`, when it has one.
async fn labelled_by_name(
    conn: &AccessibilityConnection,
    acc: &AccessibleProxy<'_>,
) -> Option<String> {
    let relations = call(acc.get_relation_set()).await?.ok()?;
    for (kind, targets) in relations {
        if kind != atspi::RelationType::LabelledBy {
            continue;
        }
        for target in targets {
            let Some(raw) = RawObjectRef::from_atspi(&target) else {
                continue;
            };
            let Some(Ok(label)) = call(accessible_for(conn, &raw)).await else {
                continue;
            };
            if let Some(Ok(text)) = call(label.name()).await {
                let text = text.trim();
                if !text.is_empty() {
                    return Some(text.to_owned());
                }
            }
        }
    }
    None
}

/// Prefix of the description note an unnamed value control carries; the
/// element builder turns it into `unlabelled: true`.
pub const UNLABELLED_NOTE_PREFIX: &str = "unlabelled";

fn ordinal_word(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The real AT-SPI parent of every visited node (its position in `visited`),
/// from the pre-order depth sequence: a node at depth `d` ends every open
/// subtree at depth >= `d`. Ancestry never spans two top-levels.
fn true_parents(visited: &[Visited<'_>]) -> Vec<Option<usize>> {
    let mut parents = Vec::with_capacity(visited.len());
    let mut chain: Vec<usize> = Vec::new();
    let mut current_frame: Option<usize> = None;
    for (pos, v) in visited.iter().enumerate() {
        if current_frame != Some(v.frame_ordinal) {
            current_frame = Some(v.frame_ordinal);
            chain.clear();
        }
        chain.truncate(v.depth);
        parents.push(chain.last().copied());
        chain.push(pos);
    }
    parents
}

/// For an unnamed value control at `pos`: the nearest preceding sibling
/// label in the same container (a GTK grid row "Hue:" [spin]) — scanning
/// stops at another control of the same role, so the second row never
/// borrows the first row's label.
fn preceding_sibling_label(
    visited: &[Visited<'_>],
    parents: &[Option<usize>],
    pos: usize,
) -> Option<String> {
    let parent = parents[pos]?;
    let role = visited[pos].role.to_ascii_lowercase();
    for q in (0..pos).rev() {
        if visited[q].depth < visited[pos].depth {
            break;
        }
        if parents[q] != Some(parent) {
            continue;
        }
        let q_role = visited[q].role.to_ascii_lowercase();
        if q_role == role {
            return None;
        }
        if q_role == "label" && !visited[q].name.trim().is_empty() {
            return Some(visited[q].name.trim().to_owned());
        }
    }
    None
}

/// "3rd of 4 spin buttons in this window": the control's place among the
/// showing same-role controls of its top-level, in walk (= visual) order —
/// what a screenshot shows, so the model can map the rows it sees.
fn sibling_ordinal(visited: &[Visited<'_>], _parents: &[Option<usize>], pos: usize) -> String {
    let role = visited[pos].role.to_ascii_lowercase();
    let same_role = |q: usize| {
        visited[q].frame_ordinal == visited[pos].frame_ordinal
            && visited[q].showing
            && visited[q].role.eq_ignore_ascii_case(&role)
    };
    let total = (0..visited.len()).filter(|q| same_role(*q)).count();
    let nth = (0..=pos).filter(|q| same_role(*q)).count();
    let plural = if total == 1 {
        role.clone()
    } else {
        format!("{role}s")
    };
    format!("{} of {total} {plural} in this window", ordinal_word(nth))
}

/// Render visited nodes into the markdown + node list `walk_tree` returns.
/// Format matches the historical pyatspi output exactly so downstream parsing
/// (`extract_text_from_markdown`, `query_dom`) is unaffected.
///
/// `parent_at_depth` tracks the most recently emitted actionable index at
/// each depth, so descendants can look up their parent_element_index without
/// a second pass.
///
/// `only_frame` restricts what is *emitted* to one application top-level while
/// leaving the index space application-wide. Element indices are the contract
/// between a snapshot and every actuator that later takes one
/// (`perform_action`, `focus_element`, `set_value`, …), and those resolve an
/// index against the whole application. Renumbering per window would make a
/// window-scoped snapshot's indices name different elements at actuation time.
fn render(visited: &[Visited<'_>], only_frame: Option<usize>) -> (String, Vec<AtspiNode>) {
    let mut md = String::new();
    let mut nodes = Vec::new();
    let mut idx = 0usize;
    // The real AT-SPI parent of every node, so `parent_element_index` is the
    // nearest *indexed ancestor* — never a preceding cousin at a lower depth
    // (which parented GIMP's spin buttons to a push button).
    let parents = true_parents(visited);
    let mut index_of: Vec<Option<usize>> = vec![None; visited.len()];

    for (pos, v) in visited.iter().enumerate() {
        let emit = only_frame.is_none_or(|frame| frame == v.frame_ordinal);
        let indent = "  ".repeat(v.depth);
        let parent_element_index = {
            let mut cursor = parents[pos];
            let mut found = None;
            while let Some(p) = cursor {
                if let Some(i) = index_of[p] {
                    found = Some(i);
                    break;
                }
                cursor = parents[p];
            }
            found
        };

        if is_indexable(v) {
            index_of[pos] = Some(idx);
            if !emit {
                // Consume the index without emitting: indices stay aligned with
                // the application-wide walk the actuators perform.
                idx += 1;
                continue;
            }
            let role_lower = v.role.to_ascii_lowercase();
            // An unnamed value control: name it from the sibling label of its
            // row when there is one; otherwise say it is unlabelled and where
            // it sits among its same-role siblings, and never repeat its value
            // as its name.
            let mut name = v.name.clone();
            let mut unlabelled_note = None;
            if name.trim().is_empty() && is_value_control_role(&role_lower) {
                if !v.description.trim().is_empty() {
                    // GIMP's spin scales draw their label inside the widget
                    // and publish it as the description.
                    name = v.description.trim().to_owned();
                } else if let Some(label) = preceding_sibling_label(visited, &parents, pos) {
                    name = label;
                } else {
                    unlabelled_note = Some(format!(
                        "{UNLABELLED_NOTE_PREFIX} {}; {}",
                        role_lower,
                        sibling_ordinal(visited, &parents, pos)
                    ));
                }
            }
            let act_str = v.actions.join(",");
            let val_part = match &v.value {
                Some(val) if !val.is_empty() => format!(" value=\"{val}\""),
                _ => String::new(),
            };
            let description_shown = (!v.description.is_empty() && v.description != name)
                .then(|| format!(" (description \"{}\")", v.description));
            let collapsed_note = v
                .collapsed_children
                .map(|n| {
                    if tab_documents::is_tab_document(&v.role) {
                        " (background tab; contents skipped)".to_owned()
                    } else {
                        format!(" (closed menu, {n} items; click it to open)")
                    }
                })
                .unwrap_or_default();
            let unlabelled_shown = unlabelled_note
                .as_ref()
                .map(|note| format!(" ({note})"))
                .unwrap_or_default();
            md.push_str(&format!(
                "{indent}- [{idx}] {role} \"{name}\"{desc}{val_part} [actions=[{act_str}]]{collapsed_note}{unlabelled_shown}
",
                role = v.role,
                desc = description_shown.as_deref().unwrap_or(""),
            ));
            let mut description_parts: Vec<String> = Vec::new();
            if !v.description.is_empty() {
                description_parts.push(v.description.clone());
            }
            if let Some(n) = v.collapsed_children {
                description_parts.push(if tab_documents::is_tab_document(&v.role) {
                    "background tab; contents skipped".to_owned()
                } else {
                    format!("closed menu with {n} items; not expanded: click it to open")
                });
            }
            if let Some(note) = unlabelled_note {
                description_parts.push(note);
            }
            if v.offscreen {
                description_parts.push(OFFSCREEN_NOTE.to_owned());
            }
            nodes.push(AtspiNode {
                element_index: Some(idx),
                role: v.role.clone(),
                name: if name.trim().is_empty() {
                    None
                } else {
                    Some(name)
                },
                value: v.value.clone().filter(|s| !s.is_empty()),
                checked: v.checked,
                enabled: v.enabled,
                selected: v.selected,
                description: (!description_parts.is_empty()).then(|| description_parts.join("; ")),
                actions: v.actions.clone(),
                element_key: v.element_key,
                identity: v.identity.clone(),
                depth: v.depth,
                parent_element_index,
                in_web_content: v.in_web_doc,
                object_ref: Some(v.object_ref.clone()),
            });
            idx += 1;
        } else if emit
            && ((v.collapsed_children.is_some() && tab_documents::is_tab_document(&v.role))
                || (!v.name.is_empty() && v.showing))
        {
            let skipped =
                if v.collapsed_children.is_some() && tab_documents::is_tab_document(&v.role) {
                    " (background tab; contents skipped)"
                } else {
                    ""
                };
            md.push_str(&format!(
                "{indent}- {role} = \"{name}\"{marker}{skipped}
",
                role = v.role,
                name = v.name,
                marker = passive_marker(&v.role, !v.actions.is_empty(), v.has_component, v.enabled),
            ));
        }
    }

    (md, nodes)
}

/// Why a visible, named control is listed without an index. A disabled
/// button is rendered as `- push button = "Restore" (disabled)` so a caller
/// learns it must enable it (select something) rather than assume the tree
/// dropped it.
fn passive_marker(
    role: &str,
    has_action: bool,
    has_component: bool,
    enabled: Option<bool>,
) -> &'static str {
    let control = has_action
        || (has_component
            && matches!(
                role.trim().to_ascii_lowercase().as_str(),
                "button"
                    | "push button"
                    | "toggle button"
                    | "menu item"
                    | "check box"
                    | "radio button"
            ));
    if control && enabled == Some(false) {
        " (disabled)"
    } else {
        ""
    }
}

/// GTK, Qt, VCL and the browser bridges publish `Showing` for a widget that
/// is mapped on screen; a hidden widget keeps `Visible` at most. A state set
/// that carries neither is a hidden widget, not an old bridge.
fn is_showing_state(state: &StateSet) -> bool {
    state.contains(State::Showing)
}

/// Menu entries are the exception to the `Showing` gate: gail (GTK2, e.g.
/// GIMP 2.10) never publishes `Showing` on a menu item, open menu or not,
/// and an item in a closed menu is still reachable through its AT-SPI
/// `click` action (that is how a background click opens a GIMP dialog). A
/// `Visible` menu entry therefore stays indexed; a foreground click on one
/// that is not actually on screen takes the action route instead of the
/// pointer (see the click tool), so listing it is safe.
fn is_menu_entry_role(role_lower: &str) -> bool {
    matches!(
        role_lower,
        "menu" | "menu item" | "check menu item" | "radio menu item" | "tear off menu item"
    )
}

fn counts_as_showing(role_lower: &str, state: &StateSet) -> bool {
    is_showing_state(state) || (is_menu_entry_role(role_lower) && state.contains(State::Visible))
}

/// `(listed, offscreen)` for a node with a known state set. A hidden widget
/// (neither showing nor `Visible`) is left out of the tree and the index. A
/// `Visible` widget that is not showing stays listed — an element action can
/// still reach a control scrolled out of view, as on every other platform —
/// and is marked off-screen.
fn listing_state(role_lower: &str, state: &StateSet) -> (bool, bool) {
    if counts_as_showing(role_lower, state) {
        (true, false)
    } else if state.contains(State::Visible) {
        (true, true)
    } else {
        (false, false)
    }
}

/// Description note for an indexed element that is not on screen.
const OFFSCREEN_NOTE: &str =
    "off-screen: scroll it into view before a pixel action; element actions still reach it";

/// How many leading children of a menubar menu the closed-menu peek reads
/// (gail puts a state-less tearoff item first).
const MENU_PEEK_CHILDREN: usize = 3;

/// Format an AT-SPI numeric value like the historical `str(currentValue)`
/// (e.g. `1.0`), so `value="..."` fields stay byte-compatible.
fn format_value(v: f64) -> String {
    format!("{v:?}")
}

/// Interpret the positive AT-SPI states that establish user operability.
///
/// GTK3 commonly publishes both `Enabled` and `Sensitive`. GTK4's native
/// exporter derives widget operability from its `disabled` accessibility state
/// and publishes `Sensitive` alone for an enabled widget. Either positive state
/// therefore establishes operability; an empty set still means disabled.
fn is_enabled_state(state: &StateSet) -> bool {
    state.contains(State::Enabled) || state.contains(State::Sensitive)
}

/// Whether a walked node is exposed as an indexed, usable element.
///
/// Historically this was "the node advertises AT-SPI Actions" (buttons, menu
/// items, links). That silently dropped Value-only widgets, editable text, and
/// selectable list rows. GTK list rows expose Component + Selectable state but
/// no Action even though a coordinate click on their bounds is operable. Keep
/// every such control in the shared index space so physical-input fallbacks can
/// address it without inventing pixels in the caller. Some GTK4 buttons expose
/// only Component plus their control role; include those only when the state set
/// positively verifies that they are enabled. Passive component-backed labels
/// and containers remain outside the index.
///
/// This predicate is the single source of truth for the element-index space and
/// MUST be applied identically in `render` and in every `action_nodes` filter
/// (`perform_action`, `set_value`, `get_element_bounds`, snapshot bounds);
/// any divergence would desync indices between the snapshot and the operations.
fn is_indexable(v: &Visited) -> bool {
    v.showing
        && is_indexable_capabilities(
            &v.role,
            !v.actions.is_empty(),
            v.has_editable,
            v.has_value,
            v.selectable,
            v.has_component,
            v.enabled,
        )
}

fn is_indexable_capabilities(
    role: &str,
    has_action: bool,
    has_editable: bool,
    has_value: bool,
    has_selectable_state: bool,
    has_component: bool,
    enabled: Option<bool>,
) -> bool {
    let normalized_role = role.trim().to_ascii_lowercase();
    let pixel_addressable_control = has_component
        && enabled == Some(true)
        && matches!(normalized_role.as_str(), "button" | "push button");
    !is_passive_role(&normalized_role)
        && (has_action
            || has_editable
            || has_value
            || has_selectable_state
            || pixel_addressable_control)
        && enabled == Some(true)
}

// ── Public (sync) entry points ───────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
struct ApplicationWakeKey {
    pid: u32,
    incarnation: u64,
    bus: String,
    destination: String,
    path: String,
}

#[derive(Default)]
struct ApplicationWakes(VecDeque<ApplicationWakeKey>);

impl ApplicationWakes {
    const CAPACITY: usize = 256;

    fn contains(&self, key: &ApplicationWakeKey) -> bool {
        self.0.contains(key)
    }

    fn remember(&mut self, key: ApplicationWakeKey) {
        if self.contains(&key) {
            return;
        }
        if self.0.len() == Self::CAPACITY {
            self.0.pop_front();
        }
        self.0.push_back(key);
    }
}

fn application_wakes() -> &'static Mutex<ApplicationWakes> {
    static WAKES: OnceLock<Mutex<ApplicationWakes>> = OnceLock::new();
    WAKES.get_or_init(|| Mutex::new(ApplicationWakes::default()))
}

pub fn walk_tree(pid: u32) -> Result<Option<(String, Vec<AtspiNode>)>> {
    walk_tree_bounded(pid, 0, None, None)
        .map(|snapshot| snapshot.map(|walked| (walked.markdown, walked.nodes)))
}

/// One accessibility snapshot, plus whether it was provably narrowed to the
/// caller's window.
pub struct WalkedTree {
    pub markdown: String,
    pub nodes: Vec<AtspiNode>,
    pub bounds: Vec<(usize, i32, i32, u32, u32)>,
    /// True when a non-zero `xid` was resolved to exactly one application
    /// top-level and the snapshot contains only that window's nodes. False
    /// means the snapshot spans every window the application publishes.
    pub window_scoped: bool,
    /// Set when the walk stopped before exhausting the tree.
    pub truncation: Option<Truncation>,
    /// False when the bounds phase ran out of time; some indexed elements
    /// then carry no frame even though they exist.
    pub bounds_complete: bool,
    /// Wall time of the walk + bounds phases.
    pub elapsed: Duration,
}

/// Walk the AT-SPI tree with caller-supplied node + depth caps.
/// `max_elements = None` keeps the historical 5 000-node default; `max_depth
/// = None` keeps the historical unbounded depth. Issue #22865.
pub fn walk_tree_bounded(
    pid: u32,
    xid: u64,
    max_elements: Option<usize>,
    max_depth: Option<usize>,
) -> Result<Option<WalkedTree>> {
    walk_tree_bounded_with_timeout(pid, xid, max_elements, max_depth, OP_TIMEOUT)
}

pub(super) fn walk_tree_bounded_with_timeout(
    pid: u32,
    xid: u64,
    max_elements: Option<usize>,
    max_depth: Option<usize>,
    timeout: Duration,
) -> Result<Option<WalkedTree>> {
    runtime().block_on(async {
        // Tree traversal and bounds collection form one snapshot. Keep one
        // deadline for both phases so a dead AT-SPI peer cannot outlive the
        // operation timeout while resolving geometry. The deadline is installed
        // as the task-local operation deadline so every per-node call clamps
        // to it and the walk returns *partial* results instead of being
        // dropped wholesale by the outer timeout.
        let walk_started = std::time::Instant::now();
        let deadline = tokio::time::Instant::now() + timeout;
        // Backstop only: the walk observes `deadline` itself and returns its
        // partial result; this guards the few un-wrapped awaits.
        let backstop = deadline + Duration::from_millis(500);
        let walk = REGISTRY_INCOMPLETE.scope(
            std::cell::Cell::new(None),
            OP_DEADLINE.scope(deadline, async {
                let connection = shared_connection().await?;
                let conn = &connection;
                let walked =
                    collect_visited_bounded(conn, pid, xid, max_elements, max_depth).await?;
                // Unproven absence is a failed lookup, which the caller retries,
                // not an application that is not there.
                match REGISTRY_INCOMPLETE.with(|cell| cell.get()) {
                    Some(reason) if walked.is_none() && !deadline_passed() => {
                        Err(anyhow!("atspi_registry_incomplete: {reason}"))
                    }
                    _ => Ok(walked),
                }
            }),
        );
        let walked = match before_snapshot_deadline(backstop, walk).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) if error.to_string() == "app_lookup_timeout" => {
                dlog!("walk_tree: application lookup timed out for pid {pid}");
                return Ok(Some(WalkedTree {
                    markdown: String::new(),
                    nodes: Vec::new(),
                    bounds: Vec::new(),
                    window_scoped: false,
                    truncation: Some(Truncation {
                        reason: "app_lookup_timeout",
                        visited: 0,
                        pending: 0,
                    }),
                    bounds_complete: false,
                    elapsed: walk_started.elapsed(),
                }));
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                dlog!("walk_tree timed out for pid {pid}");
                return Ok(Some(WalkedTree {
                    markdown: String::new(),
                    nodes: Vec::new(),
                    bounds: Vec::new(),
                    window_scoped: false,
                    truncation: Some(Truncation {
                        reason: "timeout",
                        visited: 0,
                        pending: 0,
                    }),
                    bounds_complete: false,
                    elapsed: walk_started.elapsed(),
                }));
            }
        };
        let Some(Collected {
            visited,
            scoped_frame,
            truncation,
        }) = walked
        else {
            return Ok(None);
        };
        let walk_elapsed = walk_started.elapsed();
        let (markdown, nodes) = render(&visited, scoped_frame);
        let bounds_started = std::time::Instant::now();
        // Bounds resolution shares the deadline, but a walk that consumed the
        // whole budget must still yield *some* frames (element-index clicks
        // resolve against them without a re-walk), so grant the bounds phase a
        // small grace of up to half the budget. The tool documents this.
        let bounds_deadline = deadline
            .max(tokio::time::Instant::now() + (timeout / 2).min(Duration::from_millis(1500)));
        let bounds_backstop = bounds_deadline + Duration::from_millis(500);
        let (bounds, bounds_complete) = match before_snapshot_deadline(
            bounds_backstop,
            OP_DEADLINE.scope(
                bounds_deadline,
                element_bounds_for_visited_with_completeness(&visited, pid, xid, scoped_frame),
            ),
        )
        .await
        {
            Ok((bounds, complete)) => (bounds, complete),
            Err(_) => {
                dlog!("element bounds timed out for pid {pid}");
                (Vec::new(), false)
            }
        };
        dlog!(
            "snapshot phases pid {pid}: walk_ms={} bounds_ms={} visited={} bounds={} complete={}",
            walk_elapsed.as_millis(),
            bounds_started.elapsed().as_millis(),
            visited.len(),
            bounds.len(),
            bounds_complete
        );
        // Bounds are keyed by the application-wide element index, so drop the
        // entries for windows this snapshot no longer shows.
        let bounds = if scoped_frame.is_some() {
            let emitted: std::collections::HashSet<usize> =
                nodes.iter().filter_map(|node| node.element_index).collect();
            bounds
                .into_iter()
                .filter(|(index, ..)| emitted.contains(index))
                .collect()
        } else {
            bounds
        };
        Ok(Some(WalkedTree {
            markdown,
            nodes,
            bounds,
            window_scoped: scoped_frame.is_some(),
            truncation,
            bounds_complete,
            elapsed: walk_started.elapsed(),
        }))
    })
}

/// Enumerate top-level windows from the AT-SPI registry — the window-listing
/// fallback for Wayland compositors that DON'T implement
/// `zwlr_foreign_toplevel_management` (GNOME Mutter, KDE KWin). Native Wayland
/// apps have no X11 XID and Mutter/KWin expose no foreign-toplevel list, so
/// `wayland::list_windows` comes back empty there and the whole element flow
/// (get_window_state -> click by element_index) is unreachable — even though the
/// AT-SPI tree itself is keyed by PID and works fine (see `walk_tree_bounded`,
/// whose walk ignores the xid). This bridges that gap: it returns one
/// [`WindowInfo`] per application top-level frame, with a SYNTHETIC but stable
/// `xid`. Downstream `get_window_state` / `click` walk the tree by PID and never
/// dereference the xid against X11, so the synthetic value only needs to be
/// non-zero and to round-trip back from the caller.
pub fn list_windows(filter_pid: Option<u32>) -> Vec<crate::x11::WindowInfo> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return std::thread::spawn(move || list_windows_blocking(filter_pid))
            .join()
            .unwrap_or_default();
    }
    list_windows_blocking(filter_pid)
}

/// Stacking position (`_NET_CLIENT_LIST_STACKING`, bottom-to-top) for an
/// AT-SPI frame, joined to the X11 toplevels of the same pid by title (the
/// AT-SPI xid is synthetic). `None` when X11 does not know the window.
fn z_index_from_stacking(
    stacking: &[crate::x11::WindowInfo],
    pid: u32,
    title: &str,
) -> Option<usize> {
    let same_pid: Vec<&crate::x11::WindowInfo> =
        stacking.iter().filter(|w| w.pid == Some(pid)).collect();
    same_pid
        .iter()
        .find(|w| w.title == title)
        .copied()
        .or_else(|| (same_pid.len() == 1).then(|| same_pid[0]))
        .and_then(|w| w.z_index)
}

fn list_windows_blocking(filter_pid: Option<u32>) -> Vec<crate::x11::WindowInfo> {
    use crate::x11::WindowInfo;
    // X11 knows the stacking order the AT-SPI registry does not; on an X
    // session the frames get their `z_index` from it so callers can rank
    // an app's dialog above the main window it covers.
    let stacking: Vec<WindowInfo> = if crate::wayland::is_wayland() {
        Vec::new()
    } else {
        crate::x11::list_windows(filter_pid)
    };
    let result = runtime().block_on(async {
        let work = async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let zconn = conn.connection();
            let root = match call(conn.root_accessible_on_registry()).await {
                Some(Ok(r)) => r,
                _ => return Ok(Vec::new()),
            };
            let dbus = atspi::zbus::fdo::DBusProxy::new(zconn)
                .await
                .map_err(|e| anyhow!("DBus proxy unavailable: {e}"))?;
            let apps = match call(root.get_children()).await {
                Some(Ok(a)) => a,
                _ => return Ok(Vec::new()),
            };
            let mut out: Vec<WindowInfo> = Vec::new();
            for app_ref in apps {
                // Skip apps that can't answer the pid query (modal-grabbed) and
                // apps that don't match the filter.
                let cpid = match call(pid_of(&dbus, &app_ref)).await {
                    Some(Some(p)) => p,
                    _ => continue,
                };
                if let Some(want) = filter_pid {
                    if cpid != want {
                        continue;
                    }
                }
                let app_ref = match RawObjectRef::from_atspi(&app_ref) {
                    Some(app_ref) => app_ref,
                    None => continue,
                };
                let app = match call(accessible_for(conn, &app_ref)).await {
                    Some(Ok(a)) => a,
                    _ => continue,
                };
                let app_name = call(app.name())
                    .await
                    .and_then(|r| r.ok())
                    .unwrap_or_default();
                let frames = match call(app.get_children()).await {
                    Some(Ok(c)) => c,
                    _ => Vec::new(),
                };
                let mut emitted = 0usize;
                for (i, frame_ref) in frames.iter().enumerate() {
                    let frame_ref = match RawObjectRef::from_atspi(frame_ref) {
                        Some(frame_ref) => frame_ref,
                        None => continue,
                    };
                    let frame = match call(accessible_for(conn, &frame_ref)).await {
                        Some(Ok(f)) => f,
                        _ => continue,
                    };
                    let role = call(frame.get_role_name())
                        .await
                        .and_then(|r| r.ok())
                        .unwrap_or_default();
                    if !matches!(
                        role.as_str(),
                        "frame" | "window" | "dialog" | "alert" | "file chooser"
                    ) {
                        continue;
                    }
                    let title = call(frame.name())
                        .await
                        .and_then(|r| r.ok())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| app_name.clone());
                    let geometry = match call(frame.proxies()).await {
                        Some(Ok(proxies)) => match call(proxies.component()).await {
                            Some(Ok(component)) => call(component.get_extents(CoordType::Screen))
                                .await
                                .and_then(|result| result.ok()),
                            _ => None,
                        },
                        _ => None,
                    };
                    let (observed_x, observed_y, width, height) = geometry
                        .filter(|(_, _, width, height)| *width > 0 && *height > 0)
                        .map(|(x, y, width, height)| {
                            (x, y, width.max(0) as u32, height.max(0) as u32)
                        })
                        .unwrap_or((0, 0, 0, 0));
                    // On native Wayland, AT-SPI frame extents can be a stale
                    // toolkit default even when the compositor has already
                    // placed the window elsewhere. Prefer compositor metadata
                    // by pid/title so list_windows and later element geometry
                    // share the same screen origin.
                    let (x, y) = prefer_authoritative_wayland_origin(
                        authoritative_wayland_origin(cpid, 0, Some(&title)),
                        Some((observed_x, observed_y)),
                    )
                    .unwrap_or((observed_x, observed_y));
                    // Stable, non-zero, unique per (pid, frame ordinal).
                    let xid = accessibility_window_id(cpid, i);
                    let z_index = z_index_from_stacking(&stacking, cpid, &title);
                    out.push(WindowInfo {
                        xid,
                        pid: Some(cpid),
                        app_name: app_name.clone(),
                        title,
                        is_on_screen: width > 0 && height > 0,
                        z_index,
                        x,
                        y,
                        width,
                        height,
                        native_window_id: None,
                        target_id: None,
                        helper_epoch: None,
                        transient_for_window_id: None,
                        transient_for_target_id: None,
                        is_attached_dialog: None,
                        is_modal: None,
                        window_type: None,
                        workspace_index: None,
                        workspace_active: None,
                        sticky: None,
                        monitor: None,
                        capture_current: None,
                        identity_capabilities: None,
                    });
                    emitted += 1;
                }
                // App with no enumerable top-level frame still gets one handle so
                // the by-pid AT-SPI element flow stays reachable.
                if emitted == 0 {
                    out.push(WindowInfo {
                        xid: (cpid as u64).max(1),
                        pid: Some(cpid),
                        app_name: app_name.clone(),
                        title: app_name,
                        is_on_screen: true,
                        z_index: None,
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                        native_window_id: None,
                        target_id: None,
                        helper_epoch: None,
                        transient_for_window_id: None,
                        transient_for_target_id: None,
                        is_attached_dialog: None,
                        is_modal: None,
                        window_type: None,
                        workspace_index: None,
                        workspace_active: None,
                        sticky: None,
                        monitor: None,
                        capture_current: None,
                        identity_capabilities: None,
                    });
                }
            }
            Ok::<_, anyhow::Error>(out)
        };
        Ok(match tokio::time::timeout(OP_TIMEOUT, work).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                dlog!("atspi list_windows failed: {e}");
                Vec::new()
            }
            Err(_) => {
                dlog!("atspi list_windows timed out");
                Vec::new()
            }
        })
    });
    result.unwrap_or_default()
}

/// Pick the editable node to write into, by priority:
///   1. the focused editable (if the toolkit exposes focus),
///   2. an editable inside web/document content — for a browser this is the
///      page's field, not the address bar (which sorts first in the tree but is
///      chrome),
///   3. the first editable anywhere (covers single-field apps like a GTK dialog
///      entry, or a GTK4 GtkEntry).
///
/// Choose the editable to write into. `complete` says whether `visited` is the
/// whole tree: on a *partial* walk the "first editable anywhere" fallback is
/// withheld, because the field the user means (the focused one) may simply not
/// have been reached yet and writing into an arbitrary earlier field (a name
/// box, a search entry) would be a silent misdelivery.
fn pick_editable<'v, 'a>(visited: &'v [Visited<'a>]) -> Option<&'v Visited<'a>> {
    visited
        .iter()
        .find(|v| v.has_editable && v.focused)
        .or_else(|| visited.iter().find(|v| v.has_editable && v.in_web_doc))
        .or_else(|| visited.iter().find(|v| v.has_editable))
}

/// Try to write `text` into the best editable node in `visited` via AT-SPI
/// EditableText. Returns `Ok(true)` if the write landed,
/// `Ok(false)` if no editable was found / the EditableText write was rejected.
async fn write_into_editable(visited: &[Visited<'_>], text: &str) -> Result<bool> {
    let target = match pick_editable(visited) {
        Some(t) => t,
        None => return Ok(false),
    };
    write_into_editable_target(target, text).await
}

async fn write_into_editable_target(target: &Visited<'_>, text: &str) -> Result<bool> {
    write_into_editable_target_checked(target, text, None).await
}

/// The observed route revalidates after proxy/caret lookups and immediately
/// before each mutation, including the optional GrabFocus between writes.
async fn write_into_editable_target_checked(
    target: &Visited<'_>,
    text: &str,
    observed: Option<&ObservedClickTarget>,
) -> Result<bool> {
    dlog!(
        "insert target: role={:?} in_web_doc={} focused={} has_component={}",
        target.role,
        target.in_web_doc,
        target.focused,
        target.has_component
    );

    let proxies = target
        .acc
        .proxies()
        .await
        .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;

    // A focus-free EditableText write is the strongest background route. Try it
    // before GrabFocus: WebKitGTK can invalidate the original proxy when focus
    // changes, and writing through that stale object then returns false.
    if write_through_editable_proxies(&proxies, text, observed).await? {
        return Ok(true);
    }

    // Try to grab focus on the widget via AT-SPI Component.GrabFocus.
    // This should give the widget internal keyboard focus without activating
    // the window, allowing GTK4 (and similar toolkits) to expose EditableText
    // on an unfocused window's focused widget.
    if target.has_component {
        if let Ok(comp) = proxies.component().await {
            if let Some(observed) = observed {
                observed.verify_live_at_mutation().await?;
            }
            match call(comp.grab_focus()).await {
                Some(Ok(true)) => dlog!("GrabFocus succeeded on {:?}", target.role),
                Some(Ok(false)) => dlog!("GrabFocus returned false on {:?}", target.role),
                Some(Err(e)) => dlog!("GrabFocus failed on {:?}: {}", target.role, e),
                None => dlog!("GrabFocus timed out on {:?}", target.role),
            }
        } else {
            dlog!("Component interface unavailable despite has_component=true");
        }
    } else {
        dlog!("Target has no Component interface, skipping GrabFocus");
    }

    write_through_editable_proxies(&proxies, text, observed).await
}

async fn write_through_editable_proxies(
    proxies: &atspi::proxy::proxy_ext::Proxies<'_>,
    text: &str,
    observed: Option<&ObservedClickTarget>,
) -> Result<bool> {
    let et = proxies
        .editable_text()
        .await
        .map_err(|e| anyhow!("EditableText unavailable: {e}"))?;

    let off = match proxies.text().await {
        Ok(tp) => tp.caret_offset().await.unwrap_or(0),
        Err(_) => 0,
    };
    let len = text.chars().count() as i32;

    if let Some(observed) = observed {
        observed.verify_live_at_mutation().await?;
    }
    // An indeterminate write on a retained target must not be replayed.
    let inserted = et.insert_text(off, text, len).await;
    let inserted = if observed.is_some() {
        inserted?
    } else {
        inserted.unwrap_or(false)
    };
    if inserted {
        return Ok(true);
    }
    if let Some(observed) = observed {
        observed.verify_live_at_mutation().await?;
    }
    let replaced = et.set_text_contents(text).await;
    let replaced = if observed.is_some() {
        replaced?
    } else {
        replaced.unwrap_or(false)
    };
    if replaced {
        return Ok(true);
    }
    Ok(false)
}

/// Write into the best editable exposed by the current AT-SPI tree without
/// falling through to synthetic X11 input.
pub fn type_into_editable(pid: u32, text: &str) -> Result<()> {
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let visited = collect_visited(conn, pid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            if write_into_editable(&visited, text).await? {
                Ok(())
            } else {
                Err(anyhow!("no writable AT-SPI element found for pid {pid}"))
            }
        },
        || Err(anyhow!("AT-SPI editable lookup timed out for pid {pid}")),
    )
}

/// Write into the exact indexed editable exposed by the caller's snapshot.
pub fn type_into_editable_at(pid: u32, idx: usize, text: &str) -> Result<()> {
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let visited = collect_visited(conn, pid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let target = visited
                .iter()
                .filter(|node| is_indexable(node))
                .nth(idx)
                .ok_or_else(|| anyhow!("element {idx} not found (total: {})", visited.len()))?;
            if write_into_editable_target(target, text).await? {
                Ok(())
            } else {
                // GrabFocus can rebuild WebKitGTK's accessibility object. Walk
                // the same index space again and retry only that exact element;
                // never fall through to a different focused/first editable.
                let refreshed = collect_visited(conn, pid)
                    .await?
                    .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
                let refreshed_target = refreshed
                    .iter()
                    .filter(|node| is_indexable(node))
                    .nth(idx)
                    .ok_or_else(|| {
                        anyhow!("element {idx} disappeared after AT-SPI focus refresh")
                    })?;
                if write_into_editable_target(refreshed_target, text).await? {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "element {idx} is not writable through AT-SPI EditableText"
                    ))
                }
            }
        },
        || {
            Err(anyhow!(
                "AT-SPI editable write timed out for element {idx} in pid {pid}"
            ))
        },
    )
}

pub fn insert_text(pid: u32, text: &str) -> Result<bool> {
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let visited = match collect_visited(conn, pid).await? {
                Some(v) => v,
                None => return Ok(false),
            };

            dlog!(
                "insert_text: {} node(s), {} editable, {} entry/text-role",
                visited.len(),
                visited.iter().filter(|v| v.has_editable).count(),
                visited
                    .iter()
                    .filter(|v| v.role.contains("entry") || v.role.contains("text"))
                    .count(),
            );

            // Primary attempt: write into an editable exposed by the current tree.
            // GrabFocus (inside write_into_editable) gives the widget internal
            // keyboard focus without activating the window, so toolkits that expose
            // EditableText on an unfocused window (Qt6, and GTK4 with GTK_A11Y=atspi)
            // accept the write here.
            if write_into_editable(&visited, text).await? {
                return Ok(true);
            }

            // GTK3 fallback: the toolkit exposes entry/text nodes in the tree (so
            // get_text reads work) but gates EditableText on focus/activation. Try
            // finding an entry/text role with Component bounds and use X11 click+type.
            dlog!(
                "AT-SPI EditableText unavailable; checking for entry/text with Component for X11 fallback"
            );

            let entry_candidate = visited.iter().find(|v| {
                let r = v.role.to_ascii_lowercase();
                (r.contains("entry") || r.contains("text")) && v.has_component
            });

            if let Some(entry) = entry_candidate {
                dlog!(
                    "GTK3 fallback: found entry role={:?} with Component; attempting X11 click+type",
                    entry.role
                );

                // Get the entry widget's screen bounds via Component.GetExtents.
                if let Ok(proxies) = entry.acc.proxies().await {
                    if let Ok(comp) = proxies.component().await {
                        if let Some(Ok((x, y, w, h))) =
                            call(comp.get_extents(CoordType::Screen)).await
                        {
                            // Click the center of the entry to establish widget focus (not window focus).
                            let cx = x + (w.max(0) / 2);
                            let cy = y + (h.max(0) / 2);
                            dlog!(
                                "GTK3 fallback: entry bounds ({x},{y} {w}x{h}), clicking center ({cx},{cy})"
                            );

                            // Get the window XID for this app so we can send X11 events to it.
                            let Some(xid) = entry_find_window_xid(pid).await else {
                                dlog!("GTK3 fallback: could not find window XID");
                                return Ok(false);
                            };

                            // Translate screen coords to window-local coords for XSendEvent.
                            let Some((wx, wy)) = screen_to_window_coords(xid, cx, cy) else {
                                dlog!("GTK3 fallback: screen-to-window coord translation failed");
                                return Ok(false);
                            };

                            dlog!("GTK3 fallback: window XID {xid}, local coords ({wx},{wy})");

                            // Click the entry to focus the widget (widget focus, not window focus).
                            if let Err(e) = crate::input::send_click(xid, wx, wy, 1, 1) {
                                dlog!("GTK3 fallback: click failed: {e}");
                                return Ok(false);
                            };

                            // Small delay for the click to register and the widget to update focus.
                            tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

                            // Now type via X11 XSendEvent — the entry widget has internal focus
                            // so it should accept the keystrokes even though the window is unfocused.
                            if let Err(e) = crate::input::send_type_text(xid, text) {
                                dlog!("GTK3 fallback: send_type_text failed: {e}");
                                return Ok(false);
                            }

                            dlog!("GTK3 fallback: X11 click+type succeeded");
                            return Ok(true);
                        }
                    }
                }
            }

            Ok(false)
        },
        || {
            dlog!("insert_text timed out for pid {pid}; falling back to synthetic typing");
            Ok(false)
        },
    )
}

/// Classify what holds keyboard focus in `pid`'s tree, so `type_text` can target
/// the thing the user just clicked rather than the first editable anywhere:
///   `Some(true)`  — a focused **editable** widget (a text entry/box). AT-SPI
///                   EditableText insertion targets it correctly.
///   `Some(false)` — a focused **non-editable** widget that still accepts typed
///                   input (a spreadsheet cell/grid, a terminal, a canvas). An
///                   AT-SPI editable search would grab the wrong field here (e.g.
///                   gnumeric's name box), so the caller should synth-type into
///                   the focused widget instead.
///   `None`        — nothing is focused (or the app is unreachable): fall back to
///                   the focus-free "first editable" path for background typing.
pub fn focused_is_editable(pid: u32) -> Result<Option<bool>> {
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let visited = match collect_visited(conn, pid).await? {
                Some(v) => v,
                None => return Ok(None),
            };
            Ok(visited.iter().find(|v| v.focused).map(|v| v.has_editable))
        },
        || Ok(None),
    )
}

/// Find the window XID for a PID by listing its X11 windows.
async fn entry_find_window_xid(pid: u32) -> Option<u64> {
    use crate::x11::list_windows;

    // List X11 windows for that PID and return the first one.
    let windows = list_windows(Some(pid));
    let xid = windows.first()?.xid;
    Some(xid)
}

/// Translate screen coordinates to window-local coordinates.
fn screen_to_window_coords(xid: u64, screen_x: i32, screen_y: i32) -> Option<(i32, i32)> {
    use x11rb::protocol::xproto::*;
    use x11rb::rust_connection::RustConnection;

    let (conn, _) = RustConnection::connect(None).ok()?;
    let window = xid as u32;

    // Get window geometry to find its screen position.
    let geom = conn.get_geometry(window).ok()?.reply().ok()?;

    // Translate to root coordinates (screen coords of window's origin).
    let trans = conn
        .translate_coordinates(window, geom.root, 0, 0)
        .ok()?
        .reply()
        .ok()?;

    // Window-local = screen - window_origin.
    Some((screen_x - trans.dst_x as i32, screen_y - trans.dst_y as i32))
}

/// Activation verbs an AT-SPI action name may carry. Compared against the
/// segment after the last `.`, because GTK4 exposes namespaced action names
/// (`buffer.delete-line`, `clipboard.copy`) while GTK3/Qt expose bare ones
/// (`click`, `activate`).
const ACTIVATION_VERBS: &[&str] = &[
    "click",
    // Chromium exposes this on a static/text node whose clickable target is an
    // ancestor. Dropping it would take away a working path.
    "clickancestor",
    "activate",
    "press",
    "invoke",
    "toggle",
    "open",
    "jump",
    "dodefault",
];

/// Checkbox-only verbs exposed by Chromium's AT-SPI bridge. These are not
/// globally safe activation names: an unrelated widget may advertise a
/// namespaced action ending in `check`, so the role gate is mandatory.
const CHECKBOX_ACTIVATION_VERBS: &[&str] = &["check", "uncheck"];

fn normalized_action_verb(name: &str) -> String {
    name.rsplit('.')
        .next()
        .unwrap_or(name)
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Is this action name an activation — the AT-SPI analogue of a click?
///
/// Position is not meaning. A GTK4 text view advertises fifteen actions whose
/// first is `buffer.delete-line`, so actuating "action 0" there deletes a line
/// of the user's document instead of placing a caret.
fn is_activation_action(name: &str) -> bool {
    let verb = normalized_action_verb(name);
    ACTIVATION_VERBS.contains(&verb.as_str())
}

fn is_checkbox_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "check box" | "checkbox"
    )
}

/// Index of the action to actuate, or `None` when the element advertises no
/// activation. `None` must not fall back to index 0: firing an arbitrary
/// action is worse than reporting that there is nothing to fire.
fn activation_index(role: &str, actions: &[String]) -> Option<usize> {
    actions.iter().position(|action| {
        is_activation_action(action)
            || (is_checkbox_role(role)
                && CHECKBOX_ACTIVATION_VERBS.contains(&normalized_action_verb(action).as_str()))
    })
}

pub(crate) fn is_container_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "layered pane"
            | "list"
            | "list box"
            | "table"
            | "tree"
            | "tree table"
            | "icon view"
            | "panel"
            | "scroll pane"
            | "viewport"
    )
}

/// Item roles a click selects rather than activates when they expose no
/// Action of their own (Nautilus file `canvas` items, GTK list rows / cells).
/// A spreadsheet / grid cell. Selecting one through its table's `Selection`
/// interface marks it (LibreOffice Calc highlights the cell) but does not
/// move the cell cursor, so text typed afterwards still lands in the
/// previously active cell (A1 on a fresh sheet). With a real pointer
/// available, a click on a cell must be a real press.
fn is_cell_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "table cell" | "cell"
    )
}

/// Whether a click on this node must be a real pointer press. A press on
/// editable text, a focus-taking control (entry, spin button, slider, combo
/// box) or a table cell places the caret or cell cursor. Its accessibility
/// action does something else: a GTK entry's `activate` is Enter, which can
/// submit a dialog's default button. Element clicks and pixel clicks that
/// resolve to an accessible node share this rule.
fn requires_real_pointer_press(has_editable: bool, role: &str) -> bool {
    has_editable || super::is_focus_taking_role(role) || is_cell_role(role)
}

pub(crate) fn is_tooltip_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "tool tip" | "tooltip"
    )
}

fn is_selectable_item_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "canvas" | "list item" | "table cell" | "cell" | "icon" | "tree item" | "table row" | "row"
    )
}

/// The at-point activation rule. The deepest node under the point may fire
/// any activation it advertises unless it is a container (a click on a
/// container's empty area must not open its current selection); an ancestor
/// may only be fired when it is neither a container nor a `canvas` (a drawing
/// surface whose `activate` means "open the selection" once it has children).
fn at_point_activation_index(
    role: &str,
    actions: &[String],
    is_deepest_hit: bool,
) -> Option<usize> {
    if is_container_role(role) || crate::at_point_policy::is_top_level_shell_role(role) {
        return None;
    }
    if !is_deepest_hit && role.trim().eq_ignore_ascii_case("canvas") {
        return None;
    }
    activation_index(role, actions)
}

/// What an at-point accessibility click did, for the tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtPointHit {
    /// AT-SPI action name fired (`activate`, `click`), or `select_child`
    /// when the item was selected through its container's Selection
    /// interface instead.
    pub action: String,
    pub role: String,
    pub name: String,
    /// D-Bus object path of the node acted on.
    pub path: String,
    /// Set on the Selection route: the item that is now selected.
    pub selected: Option<String>,
    /// Selection route only: `Selection.IsChildSelected` read back true.
    pub selection_verified: bool,
}

impl AtPointHit {
    pub fn fired(action: String, role: String, name: String, path: String) -> Self {
        Self {
            action,
            role,
            name,
            path,
            selected: None,
            selection_verified: false,
        }
    }

    /// `hit: push button "Restore" action=activate path=...` /
    /// `selected: canvas "file.txt" ...`.
    pub fn describe(&self) -> String {
        match &self.selected {
            Some(item) => format!(
                "selected: {} \"{}\" (Selection.SelectChild on its container{})",
                self.role,
                item,
                if self.selection_verified {
                    ", read back as selected"
                } else {
                    ", selection not read back"
                }
            ),
            None => format!(
                "hit: {} \"{}\" action={} path={}",
                self.role, self.name, self.action, self.path
            ),
        }
    }
}

fn is_menu_role(role: &str) -> bool {
    role.trim().to_ascii_lowercase().contains("menu")
}

/// Find one exact visible menu lineage in a flattened pre-order AT-SPI walk.
/// Unlabelled menu containers are transparent; all labelled menu ancestors
/// must match the requested prefix, so duplicate labels elsewhere fail closed.
fn exact_menu_path_matches(visited: &[Visited<'_>], path: &[String]) -> Vec<usize> {
    let mut parent_at_depth: Vec<Option<usize>> = Vec::new();
    let mut parents = vec![None; visited.len()];
    for (index, node) in visited.iter().enumerate() {
        parents[index] = if node.depth == 0 {
            None
        } else {
            (0..node.depth)
                .rev()
                .find_map(|depth| parent_at_depth.get(depth).copied().flatten())
        };
        while parent_at_depth.len() <= node.depth {
            parent_at_depth.push(None);
        }
        parent_at_depth[node.depth] = Some(index);
        parent_at_depth[node.depth + 1..].fill(None);
    }

    visited
        .iter()
        .enumerate()
        .filter_map(|(index, node)| {
            if !is_menu_role(&node.role)
                || node.enabled == Some(false)
                || node.name.trim() != path.last().map(String::as_str).unwrap_or("")
            {
                return None;
            }
            let mut lineage = Vec::new();
            let mut cursor = Some(index);
            while let Some(current) = cursor {
                let ancestor = &visited[current];
                if is_menu_role(&ancestor.role) && !ancestor.name.trim().is_empty() {
                    lineage.push(ancestor.name.trim());
                }
                cursor = parents[current];
            }
            lineage.reverse();
            lineage.dedup();
            (lineage == path.iter().map(String::as_str).collect::<Vec<_>>()).then_some(index)
        })
        .collect()
}

/// Resolve and invoke an application menu one live hierarchy level at a time.
/// Every hop re-walks AT-SPI after the preceding menu has materialized; no
/// snapshot index is retained across mutations.
pub fn invoke_menu_path(
    target_proof: &crate::wayland::ExactTargetProof,
    path: &[String],
) -> Result<()> {
    crate::wayland::validate_exact_target(target_proof)?;
    let target_proof = target_proof.clone();
    let pid = target_proof.pid();
    let window_id = target_proof.window_id();
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            for depth in 0..path.len() {
                let proof_for_read = target_proof.clone();
                cua_driver_core::blocking::spawn(move || {
                    crate::wayland::validate_exact_target(&proof_for_read)
                })
                .await
                .map_err(|error| anyhow!("exact target validation worker failed: {error}"))??;
                let Collected {
                    visited,
                    scoped_frame,
                    ..
                } = collect_visited_bounded(conn, pid, window_id, None, None)
                    .await?
                    .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
                let scoped_frame = scoped_frame.ok_or_else(|| {
                    anyhow!("could not scope AT-SPI menu traversal to exact window {window_id}")
                })?;
                let matches = exact_menu_path_matches(&visited, &path[..=depth])
                    .into_iter()
                    .filter(|index| visited[*index].frame_ordinal == scoped_frame)
                    .collect::<Vec<_>>();
                let target_index = match matches.as_slice() {
                    [index] => *index,
                    [] => anyhow::bail!("menu path segment {depth} was not found"),
                    _ => anyhow::bail!("menu path segment {depth} is ambiguous"),
                };
                let target = &visited[target_index];
                if target.enabled == Some(false) {
                    anyhow::bail!("menu path segment {depth} is disabled");
                }
                let proxies = target
                    .acc
                    .proxies()
                    .await
                    .map_err(|error| anyhow!("interface proxies unavailable: {error}"))?;
                let action = proxies
                    .action()
                    .await
                    .map_err(|error| anyhow!("Action unavailable: {error}"))?;
                invoke_live_activation(
                    &action,
                    &target.role,
                    &format!("menu path segment {depth}"),
                )
                .await?;
                if depth + 1 != path.len() {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                }
            }
            Ok(())
        },
        || Err(anyhow!("invoke_menu timed out for pid {pid}")),
    )
}

pub fn perform_action(pid: u32, idx: usize) -> Result<(String, bool)> {
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let visited = collect_visited(conn, pid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let action_nodes: Vec<&Visited> = visited.iter().filter(|v| is_indexable(v)).collect();
            let target = action_nodes.get(idx).ok_or_else(|| {
                anyhow!("element {idx} not found (total: {})", action_nodes.len())
            })?;

            let suspected_noop = is_passive_role(&target.role);

            let ap = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?
                .action()
                .await
                .map_err(|e| anyhow!("Action unavailable: {e}"))?;
            let action =
                invoke_live_activation(&ap, &target.role, &format!("element {idx}")).await?;
            // AT-SPI's doAction acknowledgement can precede the renderer's
            // queued DOM mutation. Give WebKit/Chromium one short event-loop
            // turn before returning success so a caller's immediate external
            // state read observes the action it was told was delivered.
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok((action, suspected_noop))
        },
        || {
            Err(anyhow!(
                "perform_action timed out for pid {pid} (app unresponsive to AT-SPI)"
            ))
        },
    )
}

/// Actuate an application-wide element index only when the action-time AT-SPI
/// walk proves that the indexed object belongs to the carried exact window.
/// The retained proxy is mutated directly; never hand the ordinal back to a
/// second process-wide walk, where top-level reordering could retarget it.
pub fn perform_action_in_exact_window(
    target_proof: &crate::wayland::ExactTargetProof,
    element_key: u64,
) -> Result<(String, bool)> {
    crate::wayland::validate_exact_target(target_proof)?;
    let target_proof = target_proof.clone();
    let pid = target_proof.pid();
    let window_id = target_proof.window_id();
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let Collected {
                visited,
                scoped_frame,
                ..
            } = collect_visited_bounded(conn, pid, window_id, None, None)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let scoped_frame = scoped_frame.ok_or_else(|| {
                anyhow!("could not scope AT-SPI action to exact window {window_id}")
            })?;
            let action_nodes: Vec<&Visited> =
                visited.iter().filter(|node| is_indexable(node)).collect();
            let indexed_elements: Vec<(usize, u64)> = action_nodes
                .iter()
                .map(|node| (node.frame_ordinal, node.element_key))
                .collect();
            let target = action_nodes[exact_window_element_index(
                &indexed_elements,
                scoped_frame,
                element_key,
                window_id,
            )?];
            let suspected_noop = is_passive_role(&target.role);
            let action_proxy = target
                .acc
                .proxies()
                .await
                .map_err(|error| anyhow!("interface proxies unavailable: {error}"))?
                .action()
                .await
                .map_err(|error| anyhow!("Action unavailable: {error}"))?;

            let action = invoke_exact_live_activation(
                &action_proxy,
                conn,
                &target_proof,
                element_key,
                &target.role,
                &format!("element key {element_key:#x} in exact window {window_id}"),
                None,
            )
            .await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok((action, suspected_noop))
        },
        || {
            Err(anyhow!(
                "perform_action_in_exact_window timed out for pid {pid} window {window_id}"
            ))
        },
    )
}

/// Resolve the AT-SPI top-level ancestry that corresponds to an exact native
/// window. A compositor-backed window ID is not generally an AT-SPI object ID,
/// so the browser-setup singleton contract is the correlation authority. When
/// the ID is the AT-SPI fallback's synthetic `(pid, frame ordinal)` ID, retain
/// that stronger direct match as well. Any ambiguous ancestry fails closed.
fn exact_window_top_level_key(roots: &[(u64, usize)], pid: u32, window_id: u64) -> Result<u64> {
    let mut unique = roots.to_vec();
    unique.sort_unstable();
    unique.dedup();

    let direct = unique
        .iter()
        .filter(|(_, ordinal)| accessibility_window_id(pid, *ordinal) == window_id)
        .map(|(key, _)| *key)
        .collect::<Vec<_>>();
    match direct.as_slice() {
        [key] => return Ok(*key),
        [] => {}
        _ => anyhow::bail!("AT-SPI exact window ancestry is ambiguous"),
    }

    match unique.as_slice() {
        [(key, _)] => Ok(*key),
        [] => anyhow::bail!("AT-SPI exact window has no top-level ancestry"),
        _ => anyhow::bail!(
            "AT-SPI application exposes multiple top-level ancestries for exact window {window_id}"
        ),
    }
}

fn ensure_element_descends_from_exact_window(
    element_key: u64,
    actual_top_level: u64,
    expected_top_level: u64,
    window_id: u64,
) -> Result<()> {
    if actual_top_level != expected_top_level {
        anyhow::bail!(
            "AT-SPI element key {element_key:#x} is not descended from exact window {window_id}"
        );
    }
    Ok(())
}

/// Perform an expected advertised action on the exact AT-SPI object returned by
/// an earlier snapshot. Stable semantics and the D-Bus acknowledgement are
/// checked on the retained action-time proxy. The same scan also binds the
/// object to the exact native window's AT-SPI ancestry, and the immutable
/// compositor proof is revalidated immediately before `do_action`.
pub fn perform_verified_action_by_key(
    target_proof: &crate::wayland::ExactTargetProof,
    element_key: u64,
    expected_role: &str,
    expected_name: &str,
    expected_checked: Option<bool>,
    expected_actions: &[String],
    expected_action: &str,
) -> Result<(String, bool)> {
    // Run the initial compositor check outside the private AT-SPI runtime: the
    // window enumerator may itself use AT-SPI as a Wayland fallback.
    crate::wayland::validate_single_exact_target(target_proof)?;
    let target_proof = target_proof.clone();
    let pid = target_proof.pid();
    let window_id = target_proof.window_id();
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let visited = collect_visited(conn, pid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let roots = visited
                .iter()
                .filter(|node| node.depth == 0)
                .map(|node| (node.top_level_key, node.frame_ordinal))
                .collect::<Vec<_>>();
            let expected_top_level = exact_window_top_level_key(&roots, pid, window_id)?;

            // Detect key collisions across the entire process tree before
            // checking ancestry. A duplicate key is never safe, even when one
            // copy happens to sit below the expected root.
            let mut matches = visited
                .iter()
                .filter(|node| is_indexable(node) && node.element_key == element_key);
            let target = matches.next().ok_or_else(|| {
                anyhow!("AT-SPI element key {element_key:#x} is stale for pid {pid}")
            })?;
            if matches.next().is_some() {
                return Err(anyhow!(
                    "AT-SPI element key {element_key:#x} is ambiguous for pid {pid}"
                ));
            }
            ensure_element_descends_from_exact_window(
                element_key,
                target.top_level_key,
                expected_top_level,
                window_id,
            )?;
            if target.role != expected_role
                || target.name != expected_name
                || target.checked != expected_checked
                || target.actions != expected_actions
            {
                anyhow::bail!(
                    "AT-SPI element key {element_key:#x} changed semantic identity before action"
                );
            }
            let suspected_noop = is_passive_role(&target.role);
            let proxy = target
                .acc
                .proxies()
                .await
                .map_err(|error| anyhow!("interface proxies unavailable: {error}"))?
                .action()
                .await
                .map_err(|error| anyhow!("Action unavailable: {error}"))?;

            let live_actions = live_action_names(&proxy).await?;
            if live_actions != expected_actions {
                anyhow::bail!(
                    "AT-SPI element key {element_key:#x} changed its live action vector before action"
                );
            }
            let action_index = live_actions
                .iter()
                .position(|action| action.eq_ignore_ascii_case(expected_action))
                .ok_or_else(|| {
                    anyhow!(
                        "AT-SPI element key {element_key:#x} no longer exposes exact action {expected_action:?}"
                    )
                })?;
            let action = live_actions[action_index].clone();

            // No AT-SPI/D-Bus await follows this authoritative compositor
            // read before DoAction, so replacement or helper-epoch drift during
            // the final semantic round trips refuses instead of retargeting.
            let proof_for_action = target_proof.clone();
            cua_driver_core::blocking::spawn(move || {
                crate::wayland::validate_single_exact_target(&proof_for_action)
            })
            .await
            .map_err(|error| anyhow!("exact target validation worker failed: {error}"))??;
            let accepted = proxy
                .do_action(action_index as i32)
                .await
                .map_err(|error| anyhow!("doAction failed: {error}"))?;
            if !accepted {
                return Err(anyhow!(
                    "AT-SPI rejected exact action {expected_action:?} for element key {element_key:#x}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok((action, suspected_noop))
        },
        || {
            Err(anyhow!(
                "perform_verified_action_by_key timed out for pid {pid} window {window_id} (app unresponsive to AT-SPI)"
            ))
        },
    )
}

/// A mutation target resolved from the object address observed in the snapshot.
/// The public integer index remains an address within that snapshot only.
pub struct ObservedClickTarget {
    pid: u32,
    xid: u64,
    index: usize,
    bounds_index: usize,
    target_position: usize,
    frame_ordinal: usize,
    visited: Vec<Visited<'static>>,
    exact_target: Option<crate::wayland::ExactTargetProof>,
}

/// Read an uncached Parent edge at the mutation boundary. Decode the bus name
/// as a string for WebKit's well-known names, then resolve its current unique
/// owner; a missing owner, malformed reply or timeout is never ancestry proof.
async fn live_parent_address(
    conn: &AccessibilityConnection,
    (name, path): ObjectAddress,
) -> Result<Option<ObjectAddress>> {
    call(async {
        let acc = accessible_for(conn, &RawObjectRef { name, path }).await?;
        let (name, path): (String, atspi::zbus::zvariant::OwnedObjectPath) =
            acc.inner().get_property("Parent").await?;
        if name.is_empty() || path.as_str() == "/org/a11y/atspi/null" {
            return Ok(None);
        }
        // No name-owner cache survives a boundary check (or even a Parent edge).
        let raw = RawObjectRef {
            name,
            path: path.to_string(),
        };
        let parent = identity_ref(conn, &raw, &mut std::collections::HashMap::new())
            .await
            .context("stale_element_token: Parent unique owner is unavailable")?;
        Ok::<_, anyhow::Error>(Some((parent.name, parent.path)))
    })
    .await
    .context("stale_element_token: Parent lookup timed out")?
    .context("stale_element_token: Parent lookup failed")
}

#[cfg(test)]
#[path = "retained_pointer_tests.rs"]
pub(crate) mod retained_pointer_tests;

fn project_retained_pointer_bounds(
    raw: (i32, i32, i32, i32),
    frame_raw: (i32, i32, i32, i32),
    origin: (i32, i32),
    content: (i32, i32),
    wayland: bool,
) -> Result<(i32, i32, u32, u32)> {
    anyhow::ensure!(
        crate::snapshot_queries::plausible_raw_extents(frame_raw)
            && crate::snapshot_queries::plausible_raw_extents(raw),
        "unusable retained bounds"
    );
    let offset = if wayland {
        // Chromium's negative renderer frame origin is not an output
        // origin. Keep positive content insets, as snapshot projection does.
        (
            i64::from(origin.0) + i64::from(content.0) - i64::from(frame_raw.0.min(0)),
            i64::from(origin.1) + i64::from(content.1) - i64::from(frame_raw.1.min(0)),
        )
    } else if frame_raw.0.abs() <= 2 && frame_raw.1.abs() <= 2 {
        anyhow::ensure!(raw.0 != 0 || raw.1 != 0, "collapsed Screen extents");
        (
            i64::from(origin.0) - i64::from(frame_raw.0),
            i64::from(origin.1) - i64::from(frame_raw.1),
        )
    } else {
        anyhow::ensure!(raw.0 != 0 || raw.1 != 0, "collapsed Screen extents");
        (0, 0)
    };
    Ok((
        i32::try_from(i64::from(raw.0) + offset.0)?,
        i32::try_from(i64::from(raw.1) + offset.1)?,
        raw.2 as u32,
        raw.3 as u32,
    ))
}

fn context_menu_action(name: &str) -> bool {
    let normalized: String = name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect();
    matches!(
        normalized.as_str(),
        "showcontextmenu" | "contextmenu" | "menu" | "popup" | "popupmenu"
    )
}

// Native-I/O seam: discovery must never turn a failed read into an empty slot.
trait MenuAction {
    async fn count(&self) -> Result<i32>;
    async fn name(&self, index: i32) -> Result<String>;
    async fn dispatch(&self, index: i32) -> Result<bool>;
}
impl MenuAction for atspi::proxy::action::ActionProxy<'_> {
    async fn count(&self) -> Result<i32> {
        Ok(self.n_actions().await?)
    }
    async fn name(&self, index: i32) -> Result<String> {
        Ok(self.get_name(index).await?)
    }
    async fn dispatch(&self, index: i32) -> Result<bool> {
        Ok(self.do_action(index).await?)
    }
}

async fn strict_context_menu<A: MenuAction, F: std::future::Future<Output = Result<()>>>(
    proxy: impl std::future::Future<Output = Result<A>>,
    verify: impl Fn() -> F,
) -> Result<bool> {
    let action = call(proxy)
        .await
        .context("context menu Action proxy timed out")??;
    let count = call(action.count())
        .await
        .context("context menu NActions timed out")??;
    anyhow::ensure!(
        (0..=CHILD_ENUMERATION_CAP as i32).contains(&count),
        "invalid context menu action count"
    );
    let mut names = Vec::new();
    for index in 0..count {
        names.push(
            call(action.name(index))
                .await
                .context("context menu GetName timed out")??,
        );
    }
    // Only a complete, affirmative live read of ALL names can authorize pixels.
    let Some(index) = names.iter().position(|name| context_menu_action(name)) else {
        return Ok(false);
    };
    verify().await?;
    let accepted = call(action.dispatch(index as i32))
        .await
        .context("context menu DoAction timed out; refusing pointer replay")??;
    require_affirmative_ack(accepted, "context menu")?;
    Ok(true)
}

fn require_pointer_frame_owner(identity: Option<&AtspiIdentity>) -> Result<()> {
    // collect_native pins BOTH addresses with identity_ref/GetNameOwner before
    // constructing the retained proxy. Never accept raw well-known names here.
    let qualified = identity.is_some_and(|id| {
        atspi::zbus::names::UniqueName::try_from(id.bus_name.as_str()).is_ok()
            && atspi::zbus::names::UniqueName::try_from(id.frame_bus_name.as_str()).is_ok()
            && id.bus_name == id.frame_bus_name
    });
    anyhow::ensure!(
        qualified,
        "element_route_unqualified: element is in a separate accessibility process; retry with x,y"
    );
    Ok(())
}

impl ObservedClickTarget {
    /// Async half is also called after action-name lookups, without nesting the
    /// private runtime. Always keep the original proxy and snapshot identity.
    async fn verify_live_at_mutation(&self) -> Result<()> {
        cua_driver_core::blocking::check_cancelled()?;
        let target = &self.visited[self.target_position];
        let identity = target
            .identity
            .as_ref()
            .context("stale_element_token: observed object identity is unavailable")?;
        let state = call(target.acc.get_state())
            .await
            .and_then(|reply| reply.ok())
            .context("stale_element_token: observed object no longer responds")?;
        if state.contains(State::Defunct) || !is_enabled_state(&state) {
            anyhow::bail!("stale_element_token: observed object is defunct or disabled");
        }
        let connection = shared_connection().await?;
        let conn = &connection;
        verify_observed_frame_ancestry(identity, |address| live_parent_address(conn, address))
            .await?;
        // Recheck ownership AFTER the ancestry round trips as well. No further
        // AX lookup may intervene between this gate and DoAction/pointer input.
        if let Some(proof) = &self.exact_target {
            let proof = proof.clone();
            cua_driver_core::blocking::spawn(move || crate::wayland::validate_exact_target(&proof))
                .await
                .context("exact target validation worker failed")??;
        } else if !crate::x11::window_belongs_to_pid(self.xid, self.pid) {
            anyhow::bail!("stale_element_token: window ownership changed");
        }
        cua_driver_core::blocking::check_cancelled()?;
        Ok(())
    }

    pub fn verify_live(&self) -> Result<()> {
        bounded(self.verify_live_at_mutation(), || {
            Err(anyhow!(
                "stale_element_token: observed object liveness check timed out"
            ))
        })
    }

    pub fn screen_bounds(&self) -> Result<(i32, i32, u32, u32)> {
        bounded(
            async {
                element_bounds_for_visited(
                    &self.visited,
                    self.pid,
                    self.xid,
                    Some(self.frame_ordinal),
                )
                .await
                .into_iter()
                .find(|(index, _, _, _, _)| *index == self.bounds_index)
                .map(|(_, x, y, width, height)| (x, y, width, height))
                .ok_or_else(|| anyhow!("element {} has no usable Component bounds", self.index))
            },
            || {
                Err(anyhow!(
                    "indexed click bounds timed out for pid {}",
                    self.pid
                ))
            },
        )
    }

    pub(crate) fn exact_target_proof(&self) -> Option<crate::wayland::ExactTargetProof> {
        self.exact_target.clone()
    }

    pub fn needs_foreground_pointer(&self) -> bool {
        let target = &self.visited[self.target_position];
        requires_real_pointer_press(target.has_editable, &target.role)
    }

    /// Focus this retained Component, never a new tree ordinal. A successful
    /// GrabFocus must be followed by this object's own Focused read-back.
    pub fn focus(&self) -> Result<()> {
        bounded(
            async {
                let target = &self.visited[self.target_position];
                let component = target.acc.proxies().await?.component().await?;
                self.verify_live_at_mutation().await?;
                let accepted = call(component.grab_focus())
                    .await
                    .context("observed Component.GrabFocus timed out")??;
                require_affirmative_ack(accepted, "observed Component.GrabFocus")?;
                let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
                while tokio::time::Instant::now() < deadline {
                    if matches!(
                        tokio::time::timeout_at(deadline, target.acc.get_state()).await,
                        Ok(Ok(state)) if state.contains(State::Focused)
                    ) {
                        return self.verify_live_at_mutation().await;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                anyhow::bail!("observed Component.GrabFocus did not establish Focused state")
            },
            || Err(anyhow!("observed focus timed out")),
        )
    }

    /// `false` means no EditableText interface was advertised and no write was
    /// attempted. All errors (including indeterminate writes) stop delivery.
    /// A toolkit focus rebuild may be retried once, by the SAME unique object
    /// and frame identity, never by ordinal or focused/first-editable search.
    pub fn type_into_editable(&self, text: &str) -> Result<bool> {
        let target = &self.visited[self.target_position];
        if !target.has_editable {
            self.verify_live()?;
            return Ok(false);
        }
        let write = |observed: &Self| {
            bounded(
                async {
                    observed.verify_live_at_mutation().await?;
                    write_into_editable_target_checked(
                        &observed.visited[observed.target_position],
                        text,
                        Some(observed),
                    )
                    .await
                },
                || {
                    Err(anyhow!(
                        "observed EditableText write timed out; refusing replay"
                    ))
                },
            )
        };
        if write(self)? {
            return Ok(true);
        }
        let identity = target
            .identity
            .as_ref()
            .context("stale_element_token: observed identity unavailable after focus")?;
        // Outside bounded: resolving here must not nest the private runtime.
        let refreshed = resolve_observed_target(
            self.pid,
            self.index,
            self.xid,
            identity,
            self.exact_target.clone(),
        )
        .context("stale_element_token: observed editable disappeared after focus")?;
        if write(&refreshed)? {
            return Ok(true);
        }
        anyhow::bail!("observed element rejected AT-SPI EditableText; refusing another input route")
    }

    pub fn set_value(&self, value: &str) -> Result<()> {
        bounded(
            async {
                let target = &self.visited[self.target_position];
                // Upstream Selection.SelectChild and popup activation replay a
                // newly enumerated child ordinal/object. A permit for this
                // retained combo does not qualify those child targets. Refuse
                // before any write until an exact retained-option route exists.
                if target.role.eq_ignore_ascii_case("combo box") {
                    anyhow::bail!("element_route_unqualified: GTK combo selection is not qualified for retained element targets; no value was written");
                }
                let proxies = target.acc.proxies().await?;
                if target.has_editable {
                    let editable = proxies.editable_text().await?;
                    self.verify_live_at_mutation().await?;
                    let accepted = editable.set_text_contents(value).await?;
                    return require_affirmative_ack(accepted, "SetTextContents");
                }
                if target.has_value {
                    let numeric: f64 = value.parse().context("value is not numeric")?;
                    anyhow::ensure!(numeric.is_finite(), "value is not finite");
                    let value_proxy = proxies.value().await?;
                    self.verify_live_at_mutation().await?;
                    value_proxy.set_current_value(numeric).await?;
                    return Ok(());
                }
                anyhow::bail!("observed element exposes neither EditableText nor Value")
            },
            || Err(anyhow!("observed value mutation timed out")),
        )
    }

    /// Only an affirmative action-name miss permits a pointer fallback. Errors
    /// from action discovery or DoAction are indeterminate and never replayed.
    pub(crate) fn context_menu(&self) -> Result<bool> {
        bounded(
            async {
                let target = &self.visited[self.target_position];
                self.verify_live_at_mutation().await?;
                strict_context_menu(
                    async { Ok(target.acc.proxies().await?.action().await?) },
                    || self.verify_live_at_mutation(),
                )
                .await
            },
            || {
                Err(anyhow!(
                    "context menu outcome unknown; refusing pointer replay"
                ))
            },
        )
    }

    /// Strict retained-object bounds for secondary clicks. The caller supplies
    /// the ONE authoritative geometry used for this validation and dispatch;
    /// never consult PID/title/ordinal geometry or treat local pixels as absolute.
    pub(crate) fn pointer_bounds(
        &self,
        origin: (i32, i32),
        content: (i32, i32),
        wayland: bool,
    ) -> Result<(i32, i32, u32, u32)> {
        bounded(
            async {
                let target = &self.visited[self.target_position];
                require_pointer_frame_owner(target.identity.as_ref())?;
                let identity = target
                    .identity
                    .as_ref()
                    .context("retained identity missing")?;
                let frame = self
                    .visited
                    .iter()
                    .find(|node| {
                        node.identity.as_ref().is_some_and(|id| {
                            id.bus_name == identity.frame_bus_name && id.path == identity.frame_path
                        })
                    })
                    .context("retained frame geometry unavailable")?;
                let coord = if wayland {
                    CoordType::Window
                } else {
                    CoordType::Screen
                };
                let component = target.acc.proxies().await?.component().await?;
                let frame_component = frame.acc.proxies().await?.component().await?;
                let frame_raw = call(frame_component.get_extents(coord))
                    .await
                    .context("retained frame bounds timed out")??;
                let raw = call(component.get_extents(coord))
                    .await
                    .context("retained bounds timed out")??;
                project_retained_pointer_bounds(raw, frame_raw, origin, content, wayland)
            },
            || Err(anyhow!("retained pointer bounds timed out")),
        )
    }

    pub fn perform_action(&self, allow_activation: bool) -> Result<(String, bool)> {
        let target = &self.visited[self.target_position];
        if self.needs_foreground_pointer() {
            return Err(super::ElementClickNeedsForeground.into());
        }
        if !allow_activation {
            return Err(super::ClickActionUnavailable(
                "modified click requires pointer delivery".into(),
            )
            .into());
        }
        let suspected_noop = is_passive_role(&target.role);
        bounded(
            async {
                // A known absence is safe to fall back from; lookup errors and
                // indeterminate DoAction results must never replay as pixels.
                if target.actions.is_empty() {
                    return Err(super::ClickActionUnavailable(format!(
                        "element {} has no advertised Action interface",
                        self.index
                    ))
                    .into());
                }
                let action = target.acc.proxies().await?.action().await?;
                let names = live_action_names(&action).await?;
                let chosen = activation_index(&target.role, &names).ok_or_else(|| {
                    super::ClickActionUnavailable(format!(
                        "element {} has no live safe activation action",
                        self.index,
                    ))
                })?;
                // Action-vector round trips can reparent the still-live object.
                // An ancestry failure is NOT a typed no-input fallback outcome.
                self.verify_live_at_mutation().await?;
                let accepted = action.do_action(chosen as i32).await?;
                require_affirmative_ack(accepted, &format!("element {}", self.index))?;
                Ok((names[chosen].clone(), suspected_noop))
            },
            || {
                Err(anyhow!(
                    "indexed click action timed out for pid {}",
                    self.pid
                ))
            },
        )
    }
}

#[cfg(test)]
#[test]
fn observed_keyboard_mutations_verify_identity_without_nested_runtime_or_ordinal_retry() {
    let source = include_str!("native.rs");
    let focus = source
        .split_once(concat!("pub fn ", "focus(&self)"))
        .unwrap()
        .1
        .split_once("pub fn type_into_editable(&self")
        .unwrap()
        .0;
    assert!(
        focus.find("verify_live_at_mutation().await?").unwrap()
            < focus.find("call(component.grab_focus())").unwrap()
    );
    assert!(focus.contains("target.acc.get_state()"));
    assert!(focus.contains("state.contains(State::Focused)"));
    assert!(focus.contains("timeout_at(deadline"));
    let typed = source
        .split_once(concat!("pub fn ", "type_into_editable(&self"))
        .unwrap()
        .1
        .split_once("pub fn set_value(&self")
        .unwrap()
        .0;
    assert!(!typed.contains(".nth("));
    assert!(!typed.contains("pick_editable("));
    assert!(typed
        .split_whitespace()
        .collect::<String>()
        .contains("target.identity.as_ref()"));
    let write = typed.find("if write(self)?").unwrap();
    let resolve = typed
        .find("let refreshed = resolve_observed_target(")
        .unwrap();
    assert!(
        write < resolve,
        "identity retry must be outside the bounded write"
    );
    assert!(typed.contains("self.exact_target.clone()"));
    assert!(typed.contains("write(&refreshed)?"));
    let checked = source
        .split_once(concat!("async fn ", "write_into_editable_target_checked("))
        .unwrap()
        .1
        .split_once("/// Write into the best editable")
        .unwrap()
        .0;
    assert!(checked.contains("observed.verify_live_at_mutation().await?"));
    assert!(
        checked
            .find("observed.verify_live_at_mutation().await?")
            .unwrap()
            < checked.find("call(comp.grab_focus())").unwrap()
    );
    let proxies = checked
        .split_once("async fn write_through_editable_proxies(")
        .unwrap()
        .1;
    assert!(
        proxies
            .find("observed.verify_live_at_mutation().await?")
            .unwrap()
            < proxies.find("et.insert_text(").unwrap()
    );
    assert!(
        proxies
            .rfind("observed.verify_live_at_mutation().await?")
            .unwrap()
            < proxies.find("et.set_text_contents(").unwrap()
    );
}

/// Match a fresh walk against the object and owning frame observed in a
/// snapshot. A reordered ordinal must refuse rather than retarget the action.
pub fn resolve_observed_click_target(
    pid: u32,
    index: usize,
    xid: u64,
    identity: &AtspiIdentity,
) -> Result<ObservedClickTarget> {
    resolve_observed_target(pid, index, xid, identity, None)
}

/// Wayland carries the immutable compositor proof alongside the same retained
/// accessible/frame identity. Neither key-only nor ordinal fallback is allowed.
pub fn resolve_observed_target(
    pid: u32,
    index: usize,
    xid: u64,
    identity: &AtspiIdentity,
    exact_target: Option<crate::wayland::ExactTargetProof>,
) -> Result<ObservedClickTarget> {
    if let Some(proof) = &exact_target {
        anyhow::ensure!(
            proof.pid() == pid && proof.window_id() == xid,
            "stale_element_token: exact target mismatch"
        );
        crate::wayland::validate_exact_target(proof)?;
    }
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let Collected {
                visited,
                scoped_frame,
                ..
            } = collect_visited_bounded(conn, pid, xid, None, None)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let frame_ordinal = scoped_frame
                .ok_or_else(|| anyhow!("stale_element_token: target window frame is unproven"))?;
            let target_position = unique_observed_identity_position(
                visited
                    .iter()
                    .enumerate()
                    .map(|(position, node)| ObservedIdentityCandidate {
                        position,
                        bus_name: node.acc.inner().destination().as_str(),
                        path: node.acc.inner().path().as_str(),
                        identity: node.identity.as_ref(),
                        frame_ordinal: node.frame_ordinal,
                        indexable: is_indexable(node),
                    }),
                identity,
                frame_ordinal,
            )?;
            let bounds_index = visited[..target_position]
                .iter()
                .filter(|node| is_indexable(node))
                .count();
            Ok(ObservedClickTarget {
                pid,
                xid,
                index,
                bounds_index,
                target_position,
                frame_ordinal,
                visited,
                exact_target,
            })
        },
        || {
            Err(anyhow!(
                "stale_element_token: indexed click resolution timed out"
            ))
        },
    )
}

/// A mutation was attempted; callers must not replay through another route.
#[derive(Debug)]
pub struct ScrollProgress {
    pub acknowledged: u32,
    pub complete: bool,
    pub detail: Option<String>,
}

fn finish_scroll(result: Result<()>, attempted: bool, acknowledged: u32) -> Result<ScrollProgress> {
    match result {
        Ok(()) => Ok(ScrollProgress {
            acknowledged,
            complete: true,
            detail: None,
        }),
        Err(error) if attempted => Ok(ScrollProgress {
            acknowledged,
            complete: false,
            detail: Some(error.to_string()),
        }),
        Err(error) => Err(error),
    }
}

async fn scroll_sequence<D, DF, S, SF>(
    amount: usize,
    mut baseline: Option<Vec<(i64, i64)>>,
    attempted: &std::cell::Cell<bool>,
    acknowledged: &std::cell::Cell<u32>,
    mut dispatch: D,
    mut settle: S,
) -> Result<()>
where
    D: FnMut() -> DF,
    DF: std::future::Future<Output = Result<bool>>,
    S: FnMut(Vec<(i64, i64)>) -> SF,
    SF: std::future::Future<Output = Result<Vec<(i64, i64)>>>,
{
    for step in 0..amount.max(1) {
        dlog!(
            "scroll sequence dispatch step={} baseline={baseline:?}",
            step + 1
        );
        let deadline = tokio::time::Instant::now() + CALL_TIMEOUT;
        attempted.set(true);
        if !tokio::time::timeout_at(deadline, dispatch())
            .await
            .map_err(|_| anyhow!("scroll action timed out"))??
        {
            return Err(anyhow!("scroll action returned false"));
        }
        acknowledged.set(acknowledged.get() + 1);
        dlog!("scroll sequence acknowledged step={}", step + 1);
        if step + 1 < amount {
            if let Some(previous) = baseline.take() {
                baseline = Some(
                    tokio::time::timeout_at(deadline, settle(previous))
                        .await
                        .map_err(|_| anyhow!("scroll movement did not settle before deadline"))??,
                );
            }
        }
    }
    Ok(())
}

fn descendant_indices(depths: impl Iterator<Item = usize>, target: usize) -> Vec<usize> {
    let depths: Vec<_> = depths.collect();
    ((target + 1)..depths.len())
        .take_while(|&index| depths[index] > depths[target])
        .take(64)
        .collect()
}

fn relative_position(
    target: (i32, i32, i32, i32),
    probe: (i32, i32, i32, i32),
) -> Option<(i64, i64)> {
    let valid =
        |(x, y, w, h): (i32, i32, i32, i32)| x != i32::MIN && y != i32::MIN && w > 0 && h > 0;
    (valid(target) && valid(probe)).then(|| {
        (
            i64::from(probe.0) - i64::from(target.0),
            i64::from(probe.1) - i64::from(target.1),
        )
    })
}

struct PageMotion {
    baseline: Vec<(i64, i64)>,
    previous: Vec<(i64, i64)>,
    stable: usize,
}

impl PageMotion {
    fn new(baseline: Vec<(i64, i64)>) -> Self {
        Self {
            previous: baseline.clone(),
            baseline,
            stable: 0,
        }
    }

    fn observe(&mut self, sample: &[(i64, i64)], direction: &str) -> bool {
        if sample.len() != self.baseline.len() || sample.is_empty() {
            return false;
        }
        let axis = |point: (i64, i64)| {
            if matches!(direction, "left" | "right") {
                point.0
            } else {
                point.1
            }
        };
        let moved = sample.iter().zip(&self.baseline).any(|(&now, &before)| {
            if matches!(direction, "up" | "left") {
                axis(now) > axis(before)
            } else {
                axis(now) < axis(before)
            }
        });
        self.stable = if moved && sample == self.previous {
            self.stable + 1
        } else {
            0
        };
        self.previous = sample.to_vec();
        self.stable >= 2
    }
}

#[cfg(test)]
mod page_scroll_tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn probes_are_bounded_descendants_in_preorder() {
        assert_eq!(
            descendant_indices([0, 1, 2, 3, 2, 1, 2].into_iter(), 1),
            vec![2, 3, 4]
        );
        assert!(descendant_indices([0, 1, 1].into_iter(), 1).is_empty());
        assert_eq!(
            descendant_indices(std::iter::once(0).chain(std::iter::repeat_n(1, 100)), 0).len(),
            64
        );
    }

    #[test]
    fn relative_geometry_rejects_unrealized_and_window_motion() {
        assert_eq!(
            relative_position((10, 20, 200, 100), (30, 60, 10, 10)),
            Some((20, 40))
        );
        assert_eq!(
            relative_position((110, 220, 200, 100), (130, 260, 10, 10)),
            Some((20, 40))
        );
        assert!(relative_position((0, 0, 200, 100), (i32::MIN, 0, 10, 10)).is_none());
        assert!(relative_position((0, 0, 0, 100), (0, 0, 10, 10)).is_none());
        let mut motion = PageMotion::new(vec![(20, 40)]);
        for _ in 0..10 {
            assert!(!motion.observe(&[(20, 40)], "down"));
        }
    }

    #[test]
    fn directional_motion_requires_consecutive_stable_samples() {
        for (direction, moved) in [
            ("down", (20, 10)),
            ("up", (20, 70)),
            ("left", (50, 40)),
            ("right", (-10, 40)),
        ] {
            let mut motion = PageMotion::new(vec![(20, 40)]);
            assert!(!motion.observe(&[moved], direction));
            assert!(!motion.observe(&[moved], direction));
            assert!(motion.observe(&[moved], direction));
        }
        let mut motion = PageMotion::new(vec![(20, 40)]);
        for _ in 0..4 {
            assert!(!motion.observe(&[(20, 70)], "down"));
        }
        assert!(!motion.observe(&[], "down"));
    }

    #[tokio::test(start_paused = true)]
    async fn asynchronous_page_ack_waits_for_movement_and_stability() {
        let events = RefCell::new(Vec::new());
        let attempted = Cell::new(false);
        let acknowledged = Cell::new(0);
        scroll_sequence(
            2,
            Some(vec![(0, 100)]),
            &attempted,
            &acknowledged,
            || async {
                events.borrow_mut().push("ack");
                Ok(true)
            },
            |baseline| async {
                let mut motion = PageMotion::new(baseline);
                for (event, y) in [
                    ("unchanged", 100),
                    ("move", 0),
                    ("stable", 0),
                    ("stable", 0),
                ] {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    events.borrow_mut().push(event);
                    if motion.observe(&[(0, y)], "down") {
                        return Ok(vec![(0, y)]);
                    }
                }
                unreachable!()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            *events.borrow(),
            vec!["ack", "unchanged", "move", "stable", "stable", "ack"]
        );
        assert_eq!(acknowledged.get(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_or_timed_out_attempts_never_become_fallback() {
        for successful_before in [0, 1] {
            for timeout in [false, true] {
                let attempted = Cell::new(false);
                let acknowledged = Cell::new(0);
                let result = scroll_sequence(
                    2,
                    None,
                    &attempted,
                    &acknowledged,
                    || async {
                        if acknowledged.get() < successful_before {
                            return Ok(true);
                        }
                        if timeout {
                            std::future::pending::<()>().await;
                        }
                        Ok(false)
                    },
                    |baseline| async { Ok(baseline) },
                )
                .await;
                let outcome = finish_scroll(result, attempted.get(), acknowledged.get()).unwrap();
                assert!(!outcome.complete);
                assert_eq!(outcome.acknowledged, successful_before);
            }
        }
        assert!(finish_scroll(Err(anyhow!("unsupported before dispatch")), false, 0).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn missing_or_stalled_probe_stops_after_first_ack() {
        for timeout in [false, true] {
            let attempted = Cell::new(false);
            let acknowledged = Cell::new(0);
            let result = scroll_sequence(
                2,
                Some(vec![(0, 100)]),
                &attempted,
                &acknowledged,
                || async { Ok(true) },
                |_| async {
                    if timeout {
                        std::future::pending::<()>().await;
                    }
                    Err(anyhow!("probe disappeared"))
                },
            )
            .await;
            let outcome = finish_scroll(result, attempted.get(), acknowledged.get()).unwrap();
            assert!(!outcome.complete);
            assert_eq!(outcome.acknowledged, 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn overall_timeout_retains_progress() {
        let attempted = Cell::new(false);
        let acknowledged = Cell::new(0);
        let result = tokio::time::timeout(
            Duration::from_millis(1),
            scroll_sequence(
                2,
                Some(vec![(0, 100)]),
                &attempted,
                &acknowledged,
                || async { Ok(true) },
                |_| std::future::pending::<Result<Vec<(i64, i64)>>>(),
            ),
        )
        .await
        .map_err(|_| anyhow!("overall timeout"))
        .and_then(|r| r);
        let outcome = finish_scroll(result, attempted.get(), acknowledged.get()).unwrap();
        assert_eq!(outcome.acknowledged, 1);
        assert!(!outcome.complete);
    }
}

async fn sample_page_probes(
    target: &atspi::proxy::component::ComponentProxy<'_>,
    probes: &[atspi::proxy::component::ComponentProxy<'_>],
    coord: CoordType,
) -> Result<Vec<(i64, i64)>> {
    let before = call(target.get_extents(coord))
        .await
        .and_then(|r| r.ok())
        .ok_or_else(|| anyhow!("scroll target geometry unavailable"))?;
    let mut sample = Vec::new();
    for probe in probes {
        let bounds = call(probe.get_extents(coord))
            .await
            .and_then(|r| r.ok())
            .ok_or_else(|| anyhow!("scroll probe disappeared"))?;
        sample.push(
            relative_position(before, bounds)
                .ok_or_else(|| anyhow!("scroll probe geometry invalid"))?,
        );
    }
    let after = call(target.get_extents(coord)).await.and_then(|r| r.ok());
    if after != Some(before) {
        return Err(anyhow!("scroll target geometry changed during observation"));
    }
    Ok(sample)
}

/// Invoke the original target's semantic actions, pacing multi-page requests
/// with descendant motion. `Err` means no mutation was attempted; an incomplete
/// `ScrollProgress` preserves uncertainty after an attempted mutation.
pub fn scroll_observed_element(
    observed: &ObservedClickTarget,
    direction: &str,
    amount: usize,
    by: cua_driver_contract::ScrollBy,
) -> Result<ScrollProgress> {
    let pid = observed.pid;
    let idx = observed.index;
    let visited = &observed.visited;
    let target = &visited[observed.target_position];
    let attempted = std::cell::Cell::new(false);
    let acknowledged = std::cell::Cell::new(0);
    let result = bounded(
        async {
            observed.verify_live_at_mutation().await?;
            let proxies = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let wanted = match direction {
                "up" => ["scrollup", "scrollbackward"],
                "left" => ["scrollleft", "scrollbackward"],
                "right" => ["scrollright", "scrollforward"],
                _ => ["scrolldown", "scrollforward"],
            };
            let mut selected = None;
            let mut action_proxy = None;
            if let Ok(action) = proxies.action().await {
                let count = call(action.n_actions())
                    .await
                    .and_then(|result| result.ok())
                    .unwrap_or(0);
                for action_index in 0..count {
                    if let Some(Ok(name)) = call(action.get_name(action_index)).await {
                        let normalized: String = name
                            .chars()
                            .filter(|ch| ch.is_ascii_alphanumeric())
                            .flat_map(|ch| ch.to_lowercase())
                            .collect();
                        if wanted.iter().any(|candidate| *candidate == normalized) {
                            selected = Some(action_index);
                            break;
                        }
                    }
                }
                action_proxy = Some(action);
            }

            if let (Some(action), Some(action_index)) = (action_proxy, selected) {
                let mut observation = None;
                if by == cua_driver_contract::ScrollBy::Page && amount > 1 {
                    let component = proxies.component().await?;
                    let target_index = observed.target_position;
                    let mut probes = Vec::new();
                    // Fix probe identities before dispatch; never replace vanished descendants.
                    for index in
                        descendant_indices(visited.iter().map(|node| node.depth), target_index)
                    {
                        if !visited[index].has_component {
                            continue;
                        }
                        if let Some(Ok(proxies)) = call(visited[index].acc.proxies()).await {
                            if let Some(Ok(probe)) = call(proxies.component()).await {
                                probes.push(probe);
                            }
                        }
                    }
                    for coord in [CoordType::Window, CoordType::Screen] {
                        let mut usable = Vec::new();
                        for probe in &probes {
                            if sample_page_probes(&component, std::slice::from_ref(probe), coord)
                                .await
                                .is_ok()
                            {
                                usable.push(probe.clone());
                                if usable.len() == 8 {
                                    break;
                                }
                            }
                        }
                        if !usable.is_empty() {
                            let baseline = sample_page_probes(&component, &usable, coord).await?;
                            observation = Some((component.clone(), usable, coord, baseline));
                            break;
                        }
                    }
                    if observation.is_none() {
                        return Err(anyhow!("no usable descendant scroll probes"));
                    }
                }
                scroll_sequence(
                    amount,
                    observation.as_ref().map(|o| o.3.clone()),
                    &attempted,
                    &acknowledged,
                    || async {
                        observed.verify_live_at_mutation().await?;
                        action.do_action(action_index).await.map_err(Into::into)
                    },
                    |baseline| async {
                        let (component, probes, coord, _) = observation
                            .as_ref()
                            .expect("page probes selected before dispatch");
                        let mut motion = PageMotion::new(baseline);
                        let started = tokio::time::Instant::now();
                        loop {
                            let sample = sample_page_probes(component, probes, *coord).await?;
                            let settled = motion.observe(&sample, direction);
                            dlog!(
                                "scroll sequence sample elapsed_ms={} positions={sample:?} settled={settled}",
                                started.elapsed().as_millis()
                            );
                            if settled {
                                return Ok(sample);
                            }
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    },
                )
                .await?;
                return Ok(());
            }

            if target.has_value {
                let value = proxies
                    .value()
                    .await
                    .map_err(|e| anyhow!("Value interface unavailable: {e}"))?;
                let current = call(value.current_value())
                    .await
                    .and_then(|result| result.ok())
                    .ok_or_else(|| anyhow!("scroll value lookup timed out"))?;
                let minimum = call(value.minimum_value())
                    .await
                    .and_then(|result| result.ok())
                    .unwrap_or(current);
                let maximum = call(value.maximum_value())
                    .await
                    .and_then(|result| result.ok())
                    .unwrap_or(current);
                let increment = call(value.minimum_increment())
                    .await
                    .and_then(|result| result.ok())
                    .filter(|increment| *increment > 0.0)
                    .unwrap_or(1.0);
                let sign = if matches!(direction, "up" | "left") {
                    -1.0
                } else {
                    1.0
                };
                let next =
                    (current + sign * increment * amount.max(1) as f64).clamp(minimum, maximum);
                observed.verify_live_at_mutation().await?;
                attempted.set(true);
                call(value.set_current_value(next))
                    .await
                    .and_then(|result| result.ok())
                    .ok_or_else(|| anyhow!("scroll value update timed out"))?;
                acknowledged.set(1);
                return Ok(());
            }

            Err(anyhow!(
                "the element exposes neither directional scroll actions nor Value"
            ))
        },
        || Err(anyhow!("observed scroll timed out for pid {pid}")),
    );
    finish_scroll(result, attempted.get(), acknowledged.get())
}

#[cfg(test)]
mod retained_scroll_tests {
    use super::*;

    #[tokio::test]
    async fn retained_scroll_identity_survives_reorder_and_refuses_vanished_or_duplicated_objects()
    {
        let observed = AtspiIdentity {
            bus_name: ":1.7".into(),
            path: "/scroller".into(),
            frame_bus_name: ":1.7".into(),
            frame_path: "/frame".into(),
        };
        let other = AtspiIdentity {
            path: "/replacement".into(),
            ..observed.clone()
        };
        for (live, expected) in [
            (vec![other.clone(), observed.clone()], Some(1)),
            (vec![other.clone()], None),
            (vec![observed.clone(), observed.clone()], None),
        ] {
            let selected = unique_observed_identity_position(
                live.iter()
                    .enumerate()
                    .map(|(position, identity)| ObservedIdentityCandidate {
                        position,
                        bus_name: &identity.bus_name,
                        path: &identity.path,
                        identity: Some(identity),
                        frame_ordinal: 0,
                        indexable: true,
                    }),
                &observed,
                0,
            );
            let mutations = std::cell::RefCell::new(Vec::new());
            match (selected, expected) {
                (Ok(position), Some(expected)) => {
                    assert_eq!(position, expected);
                    let attempted = std::cell::Cell::new(false);
                    let acknowledged = std::cell::Cell::new(0);
                    scroll_sequence(
                        2,
                        None,
                        &attempted,
                        &acknowledged,
                        || {
                            mutations.borrow_mut().push(live[position].path.clone());
                            std::future::ready(Ok(true))
                        },
                        |_| std::future::ready(Ok(vec![])),
                    )
                    .await
                    .unwrap();
                    assert_eq!(*mutations.borrow(), vec!["/scroller", "/scroller"]);
                    assert_eq!(acknowledged.get(), 2);
                }
                (Err(error), None) => {
                    assert!(error.to_string().contains("stale_element_token"));
                    assert!(mutations.borrow().is_empty());
                }
                mismatch => panic!("retained scroll identity mismatch: {mismatch:?}"),
            }
        }
    }
}

/// Give an indexed element keyboard focus through AT-SPI Component.GrabFocus
/// without activating or raising its toplevel window.
///
/// `GrabFocus` acknowledges the request before Chromium/Electron necessarily
/// updates its renderer-owned focused control. Sending key events immediately
/// after the acknowledgement can therefore split one string between the old
/// and new controls. Wait for the target's Focused state to become observable;
/// an acknowledgement without read-back is not sufficient for global input.
pub fn focus_element(pid: u32, idx: usize) -> Result<bool> {
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let visited = collect_visited(conn, pid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let target = visited
                .iter()
                .filter(|v| is_indexable(v))
                .nth(idx)
                .ok_or_else(|| anyhow!("element {idx} not found (total: {})", visited.len()))?;
            let proxies = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let component = proxies
                .component()
                .await
                .map_err(|e| anyhow!("Component interface unavailable: {e}"))?;
            let accepted = match call(component.grab_focus()).await {
                Some(Ok(focused)) => focused,
                Some(Err(e)) => {
                    return Err(anyhow!("Component.GrabFocus failed for element {idx}: {e}"));
                }
                None => return Err(anyhow!("Component.GrabFocus timed out for element {idx}")),
            };
            if !accepted {
                return Ok(false);
            }

            let settle_deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(500);
            while tokio::time::Instant::now() < settle_deadline {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    target.acc.get_state(),
                )
                .await
                {
                    Ok(Ok(state)) if state.contains(State::Focused) => return Ok(true),
                    _ => {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                }
            }
            Ok(false)
        },
        || Err(anyhow!("focus_element timed out for pid {pid}")),
    )
}

/// Resolve a window-local pixel `(win_x, win_y)` to the deepest actionable
/// AT-SPI element covering it and perform its primary action.
///
/// This is the no-focus-steal way to land a *pixel* click on toolkits that drop
/// synthetic X11 pointer events. GTK3/4 take input via XInput2, so neither the
/// background `XSendEvent` path (synthetic, `send_event=True` — toolkits ignore
/// it) nor XTEST (its core events don't reach an XI2-only client; on a headless
/// Xvfb it also can't move a real device) actually clicks a GTK button. AT-SPI
/// `doAction` does, without activating or raising the window — the same path the
/// `element_index` click already uses, here driven by coordinates instead.
///
/// Hit-testing uses `Component.GetExtents(CoordType::Window)` so the caller's
/// window-local coordinates are compared directly against window-local widget
/// bounds — no screen-origin guessing. The smallest-area containing node wins so
/// a click lands on the button, not its enclosing panel. Returns `Ok(Some(action))`
/// when an element was actuated, `Ok(None)` when no actionable element covers the
/// point (the caller then falls back to the synthetic X11 path).
pub fn perform_action_at_point(pid: u32, win_x: i32, win_y: i32) -> Result<Option<String>> {
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let visited = match collect_visited(conn, pid).await? {
                Some(v) => v,
                None => return Ok(None),
            };
            let web_document_origin = web_document_origin_for_visited(&visited, pid, 0, None)
                .await
                .unwrap_or((0, 0));

            // Collect actionable nodes whose window-local bounds contain the point,
            // then let `select_click_target` pick the innermost *real actuator* —
            // preferring a button over its slightly-smaller inner label (GTK4 nests
            // one inside every button; an area-only pick lands on the inert label
            // and `do_action` silently no-ops). Pre-order keeps containers ahead of
            // children, but the area/role split is what actually disambiguates.
            let mut frames: Vec<(usize, i32, i32, u32, u32, bool)> = Vec::new();
            for (i, v) in visited.iter().enumerate() {
                if v.actions.is_empty() || !v.has_component {
                    continue;
                }
                let Some(Ok(proxies)) = call(v.acc.proxies()).await else {
                    continue;
                };
                let Some(Ok(comp)) = call(proxies.component()).await else {
                    continue;
                };
                let Some(Ok((x, y, w, h))) = call(comp.get_extents(CoordType::Window)).await else {
                    continue;
                };
                if w <= 0 || h <= 0 {
                    continue;
                }
                let (document_x, document_y) = if v.in_web_doc {
                    web_document_origin
                } else {
                    (0, 0)
                };
                frames.push((
                    i,
                    x + document_x,
                    y + document_y,
                    w as u32,
                    h as u32,
                    is_passive_role(&v.role),
                ));
            }

            let Some(idx) = select_click_target(&frames, win_x, win_y) else {
                return Ok(None);
            };
            let target = &visited[idx];
            // Same rule as the element route: never fire the action of a node
            // that takes a real press. Nothing has been sent at this point.
            if requires_real_pointer_press(target.has_editable, &target.role) {
                return Err(super::ElementClickNeedsForeground.into());
            }
            let ap = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?
                .action()
                .await
                .map_err(|e| anyhow!("Action unavailable: {e}"))?;
            let action = invoke_live_activation(&ap, &target.role, "point target").await?;
            Ok(Some(action))
        },
        || Ok(None),
    )
}

/// Vision/pixel click that actually lands — the Wayland answer (and a robust
/// GTK4 path generally). Maps a *screen* pixel to the smallest `element_index`
/// whose reconstructed screen frame covers it, then fires that element's
/// primary action via [`perform_action`].
///
/// Why not [`perform_action_at_point`]: that one hit-tests raw
/// `CoordType::Window` extents over an ad-hoc node set and `do_action`s the node
/// it resolves directly. On GTK4 that can land on an inner, non-actuating node
/// (a label inside the button) → a silent no-op that still returns `Some`
/// ("false success"). And on native Wayland there is no virtual-pointer click to
/// fall back to (Mutter drops synthetic pointer events). This routine instead
/// uses the SAME screen-frame reconstruction that `get_window_state` exposes to
/// the agent, via the GNOME Shell helper on Wayland and `_GTK_FRAME_EXTENTS` on
/// X11, and actuates by `element_index`, the click path already verified
/// working. So "click at pixel (x,y)" becomes "click the element the agent sees
/// there", with no pointer injection and no reliance on `CoordType::Screen`
/// (which GTK4 reports as (0,0)).
///
/// `screen_x`/`screen_y` are full-display screen pixels (what the vision
/// screenshot and `get_window_state` frames are in). Returns `Ok(Some(action))`
/// on a hit, `Ok(None)` when no element covers the point so the caller can fall
/// back to its native injection path.
pub fn perform_action_at_screen_point(
    target_proof: &crate::wayland::ExactTargetProof,
    screen_x: i32,
    screen_y: i32,
) -> Result<Option<String>> {
    crate::wayland::validate_exact_target(target_proof)?;
    let target_proof = target_proof.clone();
    let pid = target_proof.pid();
    let xid = target_proof.window_id();
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let Collected {
                visited,
                scoped_frame,
                ..
            } = match collect_visited_bounded(conn, pid, xid, None, None).await? {
                Some(walked) => walked,
                None => return Ok(None),
            };
            let scoped_frame = scoped_frame.ok_or_else(|| {
                anyhow!("could not scope AT-SPI screen-point action to exact window {xid}")
            })?;
            let web_document_origin =
                web_document_origin_for_visited(&visited, pid, xid, Some(scoped_frame))
                    .await
                    .unwrap_or((0, 0));

            // Reconstruct each indexable element's SCREEN frame the same way
            // get_window_state does: WINDOW-relative extents (GTK4 reports these
            // correctly; Screen is (0,0)) plus the window's screen origin (the
            // GNOME Shell helper on Wayland, _GTK_FRAME_EXTENTS on X11). When no
            // offset resolves, fall back to CoordType::Screen (correct on Qt/GTK3).
            let offset = bounded_blocking(move || window_to_screen_offset(pid, xid, None))
                .await
                .flatten();
            let coord = if offset.is_some() {
                CoordType::Window
            } else {
                CoordType::Screen
            };
            let (ox, oy) = offset.unwrap_or((0, 0));

            // Restrict both hit-testing and mutation to the one action-time
            // correlated top-level. Indices here are local to this retained set
            // and are never passed to a second process-wide traversal.
            let action_nodes: Vec<&Visited> = visited
                .iter()
                .filter(|node| node.frame_ordinal == scoped_frame && is_indexable(node))
                .collect();
            let mut frames: Vec<(usize, i32, i32, u32, u32, bool)> = Vec::new();
            for (idx, node) in action_nodes.iter().enumerate() {
                if !node.has_component {
                    continue;
                }
                let proxies = call(node.acc.proxies())
                    .await
                    .ok_or_else(|| anyhow!("point hit-test proxy lookup timed out"))?
                    .map_err(|error| anyhow!("point hit-test proxies unavailable: {error}"))?;
                let comp = call(proxies.component())
                    .await
                    .ok_or_else(|| anyhow!("point hit-test component lookup timed out"))?
                    .map_err(|error| anyhow!("point hit-test component unavailable: {error}"))?;
                let (x, y, w, h) = call(comp.get_extents(coord))
                    .await
                    .ok_or_else(|| anyhow!("point hit-test extents lookup timed out"))?
                    .map_err(|error| anyhow!("point hit-test extents unavailable: {error}"))?;
                if x == i32::MIN || y == i32::MIN || w <= 1 || h <= 1 {
                    continue;
                }
                let (document_x, document_y) = if node.in_web_doc {
                    web_document_origin
                } else {
                    (0, 0)
                };
                frames.push((
                    idx,
                    x + ox + document_x,
                    y + oy + document_y,
                    w as u32,
                    h as u32,
                    is_passive_role(&node.role),
                ));
            }

            let Some(idx) = select_click_target(&frames, screen_x, screen_y) else {
                return Ok(None);
            };
            let target = action_nodes[idx];
            // Same rule as the element route: never fire the action of a node
            // that takes a real press. Nothing has been sent at this point.
            if requires_real_pointer_press(target.has_editable, &target.role) {
                return Err(super::ElementClickNeedsForeground.into());
            }
            let ap = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?
                .action()
                .await
                .map_err(|e| anyhow!("Action unavailable: {e}"))?;
            let proof_for_action = target_proof.clone();
            cua_driver_core::blocking::spawn(move || {
                crate::wayland::validate_exact_target(&proof_for_action)
            })
            .await
            .map_err(|error| anyhow!("exact target validation worker failed: {error}"))??;

            // Re-hit-test the retained object set at the mutation boundary. A
            // renderer reorder or geometry move must not let the earlier winner
            // authorize a different current recipient.
            let mut live_frames = Vec::new();
            for (idx, node) in action_nodes.iter().enumerate() {
                if !node.has_component {
                    continue;
                }
                let proxies = node
                    .acc
                    .proxies()
                    .await
                    .map_err(|error| anyhow!("live hit-test proxies unavailable: {error}"))?;
                let component = proxies
                    .component()
                    .await
                    .map_err(|error| anyhow!("live hit-test component unavailable: {error}"))?;
                let (x, y, w, h) = component
                    .get_extents(coord)
                    .await
                    .map_err(|error| anyhow!("live hit-test extents unavailable: {error}"))?;
                if x == i32::MIN || y == i32::MIN || w <= 1 || h <= 1 {
                    continue;
                }
                let (document_x, document_y) = if node.in_web_doc {
                    web_document_origin
                } else {
                    (0, 0)
                };
                live_frames.push((
                    idx,
                    x + ox + document_x,
                    y + oy + document_y,
                    w as u32,
                    h as u32,
                    is_passive_role(&node.role),
                ));
            }
            let live_keys = action_nodes
                .iter()
                .enumerate()
                .map(|(index, node)| (index, node.element_key))
                .collect::<Vec<_>>();
            if selected_key_at_point(&live_frames, &live_keys, screen_x, screen_y)
                != Some(target.element_key)
            {
                anyhow::bail!("screen-point recipient changed before mutation");
            }
            let action = invoke_exact_live_activation(
                &ap,
                conn,
                &target_proof,
                target.element_key,
                &target.role,
                "screen-point target",
                Some((screen_x, screen_y, coord, ox, oy)),
            )
            .await?;
            Ok(Some(action))
        },
        || {
            Err(anyhow!(
                "perform_action_at_screen_point timed out for pid {pid} window {xid}; delivery is indeterminate and must not be replayed"
            ))
        },
    )
}

/// Roles that draw text/graphics but don't *do* anything when actuated. GTK4
/// nests a `label` inside every `button` with a near-identical (slightly
/// smaller) frame, so an area-only hit-test lands on the inert label —
/// `do_action` is a silent no-op (the "false success"). Treat these as
/// last-resort click targets.
fn is_passive_role(role: &str) -> bool {
    matches!(
        role,
        "label" | "static" | "static text" | "separator" | "filler" | "image" | "icon"
    )
}

/// Pick the `element_index` to actuate for a click at `(px, py)`. `frames` are
/// `(element_index, x, y, w, h, is_passive_label)` in the same coordinate space
/// as the point. Prefers the smallest covering *real actuator*; only falls back
/// to a passive label if nothing else covers the point. Smallest-area within a
/// class wins so the click lands on the button, not its enclosing panel.
/// Right/bottom edges are exclusive (`px < x + w`). `None` if nothing covers it.
fn select_click_target(
    frames: &[(usize, i32, i32, u32, u32, bool)],
    px: i32,
    py: i32,
) -> Option<usize> {
    let mut best_active: Option<(i64, usize)> = None;
    let mut best_passive: Option<(i64, usize)> = None;
    for &(idx, x, y, w, h, passive) in frames {
        let (w, h) = (w as i32, h as i32);
        if px >= x && px < x + w && py >= y && py < y + h {
            let area = (w as i64) * (h as i64);
            let slot = if passive {
                &mut best_passive
            } else {
                &mut best_active
            };
            if slot.map(|(a, _)| area < a).unwrap_or(true) {
                *slot = Some((area, idx));
            }
        }
    }
    best_active.or(best_passive).map(|(_, idx)| idx)
}

fn selected_key_at_point(
    frames: &[(usize, i32, i32, u32, u32, bool)],
    indexed_keys: &[(usize, u64)],
    px: i32,
    py: i32,
) -> Option<u64> {
    let index = select_click_target(frames, px, py)?;
    indexed_keys
        .iter()
        .find_map(|(candidate, key)| (*candidate == index).then_some(*key))
}

async fn revalidate_exact_window_element_ancestry(
    conn: &AccessibilityConnection,
    pid: u32,
    window_id: u64,
    element_key: u64,
) -> Result<()> {
    let Collected {
        visited,
        scoped_frame,
        ..
    } = collect_visited_bounded(conn, pid, window_id, None, None)
        .await?
        .ok_or_else(|| anyhow!("exact target disappeared before mutation"))?;
    let scoped_frame = scoped_frame
        .ok_or_else(|| anyhow!("could not revalidate exact window ancestry before mutation"))?;
    let indexed_elements = visited
        .iter()
        .filter(|node| is_indexable(node))
        .map(|node| (node.frame_ordinal, node.element_key))
        .collect::<Vec<_>>();
    exact_window_element_index(&indexed_elements, scoped_frame, element_key, window_id)?;
    Ok(())
}

pub fn set_value(pid: u32, idx: usize, value: &str) -> Result<()> {
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let visited = collect_visited(conn, pid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let action_nodes: Vec<&Visited> = visited.iter().filter(|v| is_indexable(v)).collect();
            let target = action_nodes.get(idx).ok_or_else(|| {
                anyhow!("element {idx} not found (total: {})", action_nodes.len())
            })?;

            let proxies = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;

            // SetValue is a focus-free accessibility operation. Do not call
            // Component.GrabFocus here: GTK may activate and raise the entire
            // toplevel in response, violating the background contract. Toolkits
            // that expose EditableText only while focused must return an honest
            // unsupported error rather than changing desktop focus implicitly.
            if let Ok(et) = proxies.editable_text().await {
                // Replace whole contents (parity with the Windows/macOS set_value,
                // which overwrite rather than insert at the caret).
                if et.set_text_contents(value).await.unwrap_or(false) {
                    return Ok(());
                }
                // Some toolkits reject SetTextContents but accept an insert at the
                // caret offset; clear-then-insert as a fallback.
                let off = match proxies.text().await {
                    Ok(tp) => tp.caret_offset().await.unwrap_or(0),
                    Err(_) => 0,
                };
                let len = value.chars().count() as i32;
                if et.insert_text(off, value, len).await.unwrap_or(false) {
                    return Ok(());
                }
            }
            if target.has_value {
                let v: f64 = value
                    .parse()
                    .map_err(|_| anyhow!("value '{value}' is not numeric for a Value element"))?;
                proxies
                    .value()
                    .await
                    .map_err(|e| anyhow!("Value unavailable: {e}"))?
                    .set_current_value(v)
                    .await
                    .map_err(|e| anyhow!("setCurrentValue failed: {e}"))?;
                return Ok(());
            }
            Err(anyhow!(
                "element {idx} exposes neither EditableText nor Value"
            ))
        },
        || {
            Err(anyhow!(
                "set_value timed out for pid {pid} (app unresponsive to AT-SPI)"
            ))
        },
    )
}

/// Set an indexed value only through the retained object from an action-time,
/// exact-window-scoped traversal.
pub fn set_value_in_exact_window(
    target_proof: &crate::wayland::ExactTargetProof,
    element_key: u64,
    value: &str,
) -> Result<()> {
    crate::wayland::validate_exact_target(target_proof)?;
    let target_proof = target_proof.clone();
    let pid = target_proof.pid();
    let window_id = target_proof.window_id();
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let Collected {
                visited,
                scoped_frame,
                ..
            } = collect_visited_bounded(conn, pid, window_id, None, None)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let scoped_frame = scoped_frame.ok_or_else(|| {
                anyhow!("could not scope AT-SPI value mutation to exact window {window_id}")
            })?;
            let action_nodes: Vec<&Visited> =
                visited.iter().filter(|node| is_indexable(node)).collect();
            let indexed_elements: Vec<(usize, u64)> = action_nodes
                .iter()
                .map(|node| (node.frame_ordinal, node.element_key))
                .collect();
            let target = action_nodes[exact_window_element_index(
                &indexed_elements,
                scoped_frame,
                element_key,
                window_id,
            )?];
            let proxies = target
                .acc
                .proxies()
                .await
                .map_err(|error| anyhow!("interface proxies unavailable: {error}"))?;

            let proof_for_action = target_proof.clone();
            cua_driver_core::blocking::spawn(move || {
                crate::wayland::validate_exact_target(&proof_for_action)
            })
            .await
            .map_err(|error| anyhow!("exact target validation worker failed: {error}"))??;

            if let Ok(editable) = proxies.editable_text().await {
                revalidate_exact_window_element_ancestry(conn, pid, window_id, element_key).await?;
                let proof = target_proof.clone();
                cua_driver_core::blocking::spawn(move || {
                    crate::wayland::validate_exact_target(&proof)
                })
                .await
                .map_err(|error| anyhow!("exact target validation worker failed: {error}"))??;
                let accepted = editable
                    .set_text_contents(value)
                    .await
                    .map_err(|error| anyhow!("SetTextContents failed: {error}"))?;
                require_affirmative_ack(
                    accepted,
                    &format!("SetTextContents for element key {element_key:#x}"),
                )?;
                return Ok(());
            }
            if target.has_value {
                let numeric: f64 = value
                    .parse()
                    .map_err(|_| anyhow!("value '{value}' is not numeric for a Value element"))?;
                let value_proxy = proxies
                    .value()
                    .await
                    .map_err(|error| anyhow!("Value unavailable: {error}"))?;
                revalidate_exact_window_element_ancestry(conn, pid, window_id, element_key).await?;
                let proof = target_proof.clone();
                cua_driver_core::blocking::spawn(move || {
                    crate::wayland::validate_exact_target(&proof)
                })
                .await
                .map_err(|error| anyhow!("exact target validation worker failed: {error}"))??;
                value_proxy
                    .set_current_value(numeric)
                    .await
                    .map_err(|error| anyhow!("setCurrentValue failed: {error}"))?;
                return Ok(());
            }
            Err(anyhow!(
                "element key {element_key:#x} exposes neither EditableText nor Value"
            ))
        },
        || {
            Err(anyhow!(
                "set_value_in_exact_window timed out for pid {pid} window {window_id}"
            ))
        },
    )
}

pub fn get_element_bounds(pid: u32, idx: usize) -> Result<(i32, i32, u32, u32)> {
    if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
        let window = crate::wayland::hyprland::window_for_pid(pid)
            .context("Hyprland bounds require an exact window for multi-window apps")?;
        return get_element_bounds_for_window(pid, window.address, idx);
    }
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let visited = collect_visited(conn, pid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let web_document_origin = web_document_origin_for_visited(&visited, pid, 0, None)
                .await
                .unwrap_or((0, 0));
            let action_nodes: Vec<&Visited> = visited.iter().filter(|v| is_indexable(v)).collect();
            let target = action_nodes
                .get(idx)
                .ok_or_else(|| anyhow!("element {idx} not found"))?;
            // Pid-only bounds have no requested window. xid 0 is a no-op in
            // gnome_window_bound_element_async; window-scoped reads use
            // get_element_bounds_for_window, which already requires a correlated frame.
            gnome_window_bound_element_async(pid, 0, target.frame_ordinal, None).await?;
            if !target.has_component {
                return Err(anyhow!("element {idx} exposes no Component interface"));
            }
            let comp = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?
                .component()
                .await
                .map_err(|e| anyhow!("Component unavailable: {e}"))?;
            // Prefer WINDOW coords + a deterministic screen offset — fixes GTK4,
            // whose CoordType::Screen collapses every element to (0,0). Fall back to
            // Screen on Wayland / when no X11 window resolves (offset is None).
            match bounded_blocking(move || window_to_screen_offset(pid, 0, None))
                .await
                .flatten()
            {
                Some((ox, oy)) => {
                    let (x, y, w, h) = comp
                        .get_extents(CoordType::Window)
                        .await
                        .map_err(|e| anyhow!("getExtents failed: {e}"))?;
                    let (document_x, document_y) = if target.in_web_doc {
                        web_document_origin
                    } else {
                        (0, 0)
                    };
                    Ok((
                        x + ox + document_x,
                        y + oy + document_y,
                        w.max(0) as u32,
                        h.max(0) as u32,
                    ))
                }
                None => {
                    let (x, y, w, h) = comp
                        .get_extents(CoordType::Screen)
                        .await
                        .map_err(|e| anyhow!("getExtents failed: {e}"))?;
                    Ok((x, y, w.max(0) as u32, h.max(0) as u32))
                }
            }
        },
        || {
            Err(anyhow!(
                "get_element_bounds timed out for pid {pid} (app unresponsive to AT-SPI)"
            ))
        },
    )
}

pub fn get_element_bounds_for_window(
    pid: u32,
    xid: u64,
    idx: usize,
) -> Result<(i32, i32, u32, u32)> {
    if !crate::wayland::is_wayland() || !crate::wayland::hyprland::is_session() {
        return get_element_bounds(pid, idx);
    }
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let Collected {
                visited,
                scoped_frame,
                ..
            } = collect_visited_bounded(conn, pid, xid, None, None)
                .await?
                .context("no AT-SPI application")?;
            let scope =
                scoped_frame.context("Hyprland accessibility window identity is unproven")?;
            let target = visited
                .iter()
                .filter(|v| is_indexable(v))
                .nth(idx)
                .context("element no longer exists")?;
            if target.frame_ordinal != scope {
                return Err(anyhow!("element does not belong to the requested window"));
            }
            element_bounds_for_visited(&visited, pid, xid, Some(scope))
                .await
                .into_iter()
                .find(|(index, ..)| *index == idx)
                .map(|(_, x, y, w, h)| (x, y, w, h))
                .context("element has no compositor-attested screen bounds")
        },
        || Err(anyhow!("Hyprland element bounds timed out")),
    )
}

pub fn get_verified_element_bounds_by_key(
    pid: u32,
    window_id: u64,
    element_key: u64,
    expected_role: &str,
    expected_name: &str,
    expected_checked: Option<bool>,
    expected_actions: &[String],
) -> Result<(i32, i32, u32, u32)> {
    bounded(
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let visited = collect_visited(conn, pid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let web_document_origin =
                web_document_origin_for_visited(&visited, pid, window_id, None)
                    .await
                    .unwrap_or((0, 0));
            let mut matches = visited
                .iter()
                .filter(|node| is_indexable(node) && node.element_key == element_key);
            let target = matches.next().ok_or_else(|| {
                anyhow!("AT-SPI element key {element_key:#x} is stale for pid {pid}")
            })?;
            if matches.next().is_some() {
                return Err(anyhow!(
                    "AT-SPI element key {element_key:#x} is ambiguous for pid {pid}"
                ));
            }
            if target.role != expected_role
                || target.name != expected_name
                || target.checked != expected_checked
                || target.actions != expected_actions
            {
                return Err(anyhow!(
                    "AT-SPI element key {element_key:#x} changed semantic identity before bounds lookup"
                ));
            }
            if !target.has_component {
                return Err(anyhow!(
                    "AT-SPI element key {element_key:#x} exposes no Component interface"
                ));
            }
            let component = target
                .acc
                .proxies()
                .await
                .map_err(|error| anyhow!("interface proxies unavailable: {error}"))?
                .component()
                .await
                .map_err(|error| anyhow!("Component unavailable: {error}"))?;
            match bounded_blocking(move || window_to_screen_offset(pid, window_id, None))
                .await
                .flatten()
            {
                Some((offset_x, offset_y)) => {
                    let (x, y, width, height) = component
                        .get_extents(CoordType::Window)
                        .await
                        .map_err(|error| anyhow!("getExtents failed: {error}"))?;
                    let (document_x, document_y) = if target.in_web_doc {
                        web_document_origin
                    } else {
                        (0, 0)
                    };
                    Ok((
                        x + offset_x + document_x,
                        y + offset_y + document_y,
                        width.max(0) as u32,
                        height.max(0) as u32,
                    ))
                }
                None => {
                    let (x, y, width, height) = component
                        .get_extents(CoordType::Screen)
                        .await
                        .map_err(|error| anyhow!("getExtents failed: {error}"))?;
                    Ok((x, y, width.max(0) as u32, height.max(0) as u32))
                }
            }
        },
        || {
            Err(anyhow!(
                "get_verified_element_bounds_by_key timed out for pid {pid} window {window_id} (app unresponsive to AT-SPI)"
            ))
        },
    )
}

/// Real on-screen origin (root-relative top-left) of an X11 window, or `None`
/// if it can't be resolved. Mirrors `list_windows`' geometry path.
fn x11_display_size() -> Option<(u32, u32)> {
    use x11rb::connection::Connection as _;
    use x11rb::rust_connection::RustConnection;
    let (conn, screen_num) = RustConnection::connect(None).ok()?;
    let screen = &conn.setup().roots[screen_num];
    Some((
        u32::from(screen.width_in_pixels),
        u32::from(screen.height_in_pixels),
    ))
}

/// Whether a `CoordType::Screen` answer can be used as is. GTK 4.6 (Ubuntu
/// 22.04) reports correct Screen extents while its `CoordType::Window`
/// extents are relative to the *outer* X11 window (shadow included), so the
/// `_GTK_FRAME_EXTENTS` reconstruction lands (left, top) px too far; newer
/// GTK4 instead collapses Screen extents to (0,0). Trust a Screen answer when
/// it is neither that sentinel nor off the display.
pub(crate) fn screen_extents_trusted(
    raw: (i32, i32, i32, i32),
    display: Option<(u32, u32)>,
) -> bool {
    let (x, y, w, h) = raw;
    if !crate::snapshot_queries::plausible_raw_extents(raw) || (x == 0 && y == 0) {
        return false;
    }
    match display {
        Some((dw, dh)) => x < dw as i32 && y < dh as i32 && x + w > 0 && y + h > 0,
        None => true,
    }
}

/// Whether a `CoordType::Screen` answer may override the Window-relative
/// reconstruction. GTK3 on native Wayland reports window-local values as
/// `Screen` (no display bounds exist to reject them), so a compositor-attested
/// native client's origin plus `Window` extents must win. XWayland clients
/// report true screen extents and are never attested.
fn screen_answer_overrides_window(
    raw: (i32, i32, i32, i32),
    display: Option<(u32, u32)>,
    native_origin_attested: bool,
) -> bool {
    !native_origin_attested && screen_extents_trusted(raw, display)
}

/// True only when the Hyprland compositor identifies this exact client
/// (nonzero address `xid` AND `pid`) as native Wayland, not XWayland. Missing,
/// mismatched, or unreported clients fail closed, so `Screen` extents keep
/// winning, which is correct for XWayland GTK and LibreOffice. Performs
/// blocking compositor IPC: async callers must use `bounded_blocking`.
fn native_wayland_origin_attested(pid: u32, xid: u64) -> bool {
    xid != 0
        && crate::wayland::is_wayland()
        && crate::wayland::hyprland::is_session()
        && crate::wayland::hyprland::native_client_attested(xid, pid)
}

pub(crate) fn x11_window_origin(xid: u64) -> Option<(i32, i32)> {
    use x11rb::protocol::xproto::*;
    use x11rb::rust_connection::RustConnection;

    let (conn, _) = RustConnection::connect(None).ok()?;
    let window = xid as u32;
    let geom = conn.get_geometry(window).ok()?.reply().ok()?;
    let trans = conn
        .translate_coordinates(window, geom.root, 0, 0)
        .ok()?
        .reply()
        .ok()?;
    Some((trans.dst_x as i32, trans.dst_y as i32))
}

fn x11_top_level_geometry(xid: u64) -> Option<(i32, i32, i32, i32)> {
    use x11rb::protocol::xproto::*;
    use x11rb::rust_connection::RustConnection;

    let (conn, _) = RustConnection::connect(None).ok()?;
    let mut window = xid as u32;
    loop {
        let tree = conn.query_tree(window).ok()?.reply().ok()?;
        if tree.parent == tree.root || tree.parent == x11rb::NONE {
            break;
        }
        window = tree.parent;
    }
    let geom = conn.get_geometry(window).ok()?.reply().ok()?;
    Some((
        i32::from(geom.x),
        i32::from(geom.y),
        i32::from(geom.width),
        i32::from(geom.height),
    ))
}

/// Read the GTK4 client-side-decoration shadow inset from the X11
/// `_GTK_FRAME_EXTENTS` property (`CARDINAL[4]` = left, right, top, bottom).
///
/// A GTK4 window is an outer X11 window whose *visible content* starts `left`
/// px in and `top` px down — the rest is the invisible CSD shadow. AT-SPI
/// `CoordType::Window` coordinates are relative to that content origin, so
/// reconstructing true screen coords needs this inset added to the X11 window
/// origin. Returns `None` (treated as no inset, i.e. `(0,0)`) for non-GTK /
/// server-side-decorated windows that don't set the property — which is also
/// how we tell GTK4-CSD apart from everyone else.
fn gtk_frame_extents(xid: u64) -> Option<(i32, i32)> {
    use x11rb::protocol::xproto::*;
    use x11rb::rust_connection::RustConnection;

    let (conn, _) = RustConnection::connect(None).ok()?;
    // only_if_exists=true → atom is 0 when no client ever set the property.
    let atom = conn
        .intern_atom(true, b"_GTK_FRAME_EXTENTS")
        .ok()?
        .reply()
        .ok()?
        .atom;
    if atom == 0 {
        return None;
    }
    let reply = conn
        .get_property(false, xid as u32, atom, AtomEnum::CARDINAL, 0, 4)
        .ok()?
        .reply()
        .ok()?;
    let vals: Vec<u32> = reply.value32()?.collect();
    parse_gtk_frame_extents(&vals)
}

/// Parse a `_GTK_FRAME_EXTENTS` `CARDINAL[4]` (`[left, right, top, bottom]`) into
/// the `(left, top)` shadow inset. `None` when fewer than 4 values (property
/// absent or malformed). Split out from [`gtk_frame_extents`] so the index
/// mapping (left = `[0]`, top = `[2]`, *not* `[1]`/`[3]`) is unit-tested without
/// an X server.
fn parse_gtk_frame_extents(vals: &[u32]) -> Option<(i32, i32)> {
    if vals.len() < 4 {
        return None;
    }
    Some((vals[0] as i32, vals[2] as i32))
}

/// Additive screen-coordinate offset that turns an element's
/// `CoordType::Window` extents into true screen coordinates:
/// `screen = x11_window_origin + _GTK_FRAME_EXTENTS.(left,top) + window_xy`.
///
/// AT-SPI `CoordType::Window` coords are relative to the toolkit's *content*
/// toplevel. The content's screen position is the X11 window's root-relative
/// origin plus the GTK4 CSD shadow inset (`_GTK_FRAME_EXTENTS`). This is
/// deterministic and replaces the old frame-(0,0)-detection heuristic; it fixes
/// GTK4, whose `CoordType::Screen` collapses *every* element to (0,0) so a
/// constant offset could never separate them (GNOME/gtk a11y rework, issues
/// #1564 / #1739) — `CoordType::Window` returns the distinct per-widget offsets
/// instead.
///
/// **Gated on `_GTK_FRAME_EXTENTS` presence**: only GTK toolkits set that
/// property (for CSD), and only GTK's Screen extents are unreliable. Non-GTK
/// toolkits (Qt, etc.) have no such property *and* report correct Screen
/// extents, so we return `None` for them — callers keep the unchanged Screen
/// path and the WINDOW reconstruction can never regress a toolkit that was
/// already correct. Also returns `None` on native Wayland (clients may not
/// query screen origins, by design) or when no X11 window resolves.
fn window_to_screen_offset(pid: u32, xid: u64, title: Option<&str>) -> Option<(i32, i32)> {
    if crate::wayland::is_wayland() {
        if crate::wayland::hyprland::is_session() {
            // Never promote toolkit-local coordinates or a stale PID origin
            // to screen coordinates when native target geometry is missing.
            let window = if xid != 0 {
                crate::wayland::hyprland::window_for_address(xid)
            } else {
                crate::wayland::hyprland::window_for_pid(pid)
            }?;
            return (window.pid == pid).then_some((window.x, window.y));
        }
        // Native Wayland: clients can't query a window's screen origin, and
        // AT-SPI CoordType::Screen collapses to (0,0) on Mutter. The bundled
        // `org.cua.WinRects` GNOME Shell extension supplies the window's screen
        // origin (`meta_window.get_frame_rect()`); combined with the per-widget
        // CoordType::Window coords (which GTK4 reports correctly on Wayland too)
        // this reconstructs real screen coords — the GNOME analogue of the X11
        // `_GTK_FRAME_EXTENTS` path below. `None` (no extension) keeps the
        // legacy Screen path (still (0,0), but no worse than before).
        let authoritative = authoritative_wayland_origin(pid, xid, title);
        // AT-SPI discovery can only guess a native Wayland origin when the
        // compositor exposes no geometry. Keep that observation as the final
        // fallback so stale default placement cannot override Sway IPC or a
        // shell helper's authoritative frame.
        return wayland_window_origin(
            authoritative,
            || crate::wayland::observed_window_origin(pid),
            crate::wayland::shell_helper::available,
        );
    }
    // Resolve a usable window xid. `xid == 0` means "no hint" (get_element_bounds
    // has no window context); fall back to this pid's first window — the same
    // convention resolve_element_local_coords uses. Guard the 0 case explicitly:
    // x11_window_origin(0) would resolve the *root* window to (0,0), not None.
    let win_xid = if xid != 0 {
        xid
    } else {
        crate::x11::list_windows(Some(pid)).first().map(|w| w.xid)?
    };
    // `?` here is the GTK gate: no _GTK_FRAME_EXTENTS → non-GTK toolkit → keep
    // the legacy Screen path (which those toolkits report correctly).
    let (fl, ft) = gtk_frame_extents(win_xid)?;
    let (ox, oy) = x11_window_origin(win_xid)?;
    Some((ox + fl, oy + ft))
}

/// Resolve a native Wayland window origin from compositor-owned metadata.
/// These sources are authoritative over AT-SPI's observed frame location,
/// which can remain at a toolkit default after Sway places the real window.
fn authoritative_wayland_origin(pid: u32, xid: u64, title: Option<&str>) -> Option<(i32, i32)> {
    if !crate::wayland::is_wayland() {
        return None;
    }
    crate::wayland::inject_accessibility_offset(xid)
        .or_else(|| crate::wayland::sway_ipc::window_origin_for_pid(pid))
        .or_else(|| {
            (xid != 0)
                .then(|| crate::wayland::sway_ipc::window_for_id(xid))
                .flatten()
                .map(|window| (window.x, window.y))
        })
        .or_else(|| {
            (xid != 0)
                .then(|| {
                    crate::wayland::sway_ipc::list_windows().and_then(|_| {
                        crate::wayland::window_geometry(xid)
                            .map(|(window_x, window_y, _, _)| (window_x, window_y))
                    })
                })
                .flatten()
        })
        .or_else(|| crate::wayland::shell_helper::window_origin(pid, xid))
        .or_else(|| title.and_then(crate::wayland::sway_ipc::window_origin_for_title))
}

fn prefer_authoritative_wayland_origin(
    authoritative: Option<(i32, i32)>,
    observed: Option<(i32, i32)>,
) -> Option<(i32, i32)> {
    authoritative.or(observed)
}

/// [`prefer_authoritative_wayland_origin`] for one window's element geometry.
/// The observation is keyed by PID alone, so it may be a sibling window's
/// origin. An attested GNOME Shell helper reports every window's exact frame;
/// when it has none for this window, the observation must not stand in.
fn wayland_window_origin(
    authoritative: Option<(i32, i32)>,
    observed: impl FnOnce() -> Option<(i32, i32)>,
    shell_reports_exact_frames: impl FnOnce() -> bool,
) -> Option<(i32, i32)> {
    if authoritative.is_some() || shell_reports_exact_frames() {
        return authoritative;
    }
    observed()
}

/// A window-bound geometry query that must refuse instead of falling back to
/// `CoordType::Screen`: the attested GNOME Shell helper reports exact frames,
/// and Mutter's Screen extents are not screen coordinates.
fn exact_wayland_origin_required(xid: u64) -> bool {
    xid != 0
        && crate::wayland::is_wayland()
        && !crate::wayland::hyprland::is_session()
        && crate::wayland::shell_helper::available()
}

fn combine_wayland_content_offsets(
    compositor: Option<(i32, i32)>,
    document: Option<(i32, i32)>,
    document_is_separate: bool,
) -> Option<(i32, i32)> {
    if !document_is_separate {
        // Chromium descendants' CoordType::Window extents are already rooted
        // below the document accessible. Adding that document's own (x,y)
        // double-counts its renderer inset. WebKitGTK exports page content on
        // a distinct WebProcess bus, so only that bridge needs the extra hop.
        return compositor;
    }
    match (compositor, document) {
        (Some((cx, cy)), Some((dx, dy))) => Some((cx + dx, cy + dy)),
        (Some(offset), None) | (None, Some(offset)) => Some(offset),
        (None, None) => None,
    }
}

fn hyprland_document_top_inset(
    target: (u64, u32),
    window: Option<(u64, u32, u32, u32)>,
    document: (i32, i32, i32, i32),
) -> Option<i32> {
    let (address, pid, window_width, window_height) = window?;
    let (x, y, width, height) = document;
    if target.0 == 0 || target != (address, pid) || x != 0 || y < 0 || width <= 0 || height <= 0 {
        return None;
    }
    let top = i64::from(window_height) - i64::from(height);
    // A full-width embedded document can omit GTK's CSD header from its
    // WINDOW origin. Reject partial panes and gaps larger than a title bar.
    if (i64::from(window_width) - i64::from(width)).abs() > 4 || !(1..=128).contains(&top) {
        return None;
    }
    Some(y.max(top as i32))
}

fn select_web_document<T>(
    candidates: impl Iterator<Item = (T, usize, bool)>,
    require_web_process_bus: bool,
) -> Option<T> {
    candidates
        .filter(|(_, _, on_web_process_bus)| !require_web_process_bus || *on_web_process_bus)
        .min_by_key(|(_, depth, _)| *depth)
        .map(|(document, _, _)| document)
}

/// Offset of embedded web content inside a captured Wayland toplevel.
/// Compositor decorations and toolkit document offsets are independent and
/// therefore additive: choosing one or the other leaves WebKit controls one
/// title bar away from the pixels shown to the caller.
async fn web_document_origin_for_visited(
    visited: &[Visited<'_>],
    pid: u32,
    xid: u64,
    scoped_frame: Option<usize>,
) -> Option<(i32, i32)> {
    if !crate::wayland::is_wayland() {
        return None;
    }
    let sway_window = crate::wayland::sway_ipc::window_for_pid(pid);
    let compositor = sway_window
        .as_ref()
        .map(|window| (window.content_x, window.content_y));
    let document_is_separate = visited.iter().any(|node| {
        scoped_frame.is_none_or(|scope| node.frame_ordinal == scope) && node.on_web_process_bus
    });
    // Only a correlated AX frame proves these document dimensions belong to
    // the named client; an application-wide walk could select a sibling.
    let hyprland_window = if document_is_separate
        && scoped_frame.is_some()
        && xid != 0
        && crate::wayland::hyprland::is_session()
    {
        crate::wayland::hyprland::accessibility_window(xid, pid)
            .map(|window| (window.address, window.pid, window.width, window.height))
    } else {
        None
    };
    // Hyprland's inferred inset must use dimensions from the WebProcess whose
    // descendants receive that inset, not a shallower application-bus document.
    let document = select_web_document(
        visited
            .iter()
            .filter(|node| scoped_frame.is_none_or(|scope| node.frame_ordinal == scope))
            .filter(|node| node.has_component)
            .filter(|node| is_document_role(&node.role) || node.in_web_doc)
            .map(|node| (node, node.depth, node.on_web_process_bus)),
        hyprland_window.is_some(),
    );
    let document = if let Some(document) = document {
        match call(document.acc.proxies()).await {
            Some(Ok(proxies)) => match call(proxies.component()).await {
                Some(Ok(component)) => match call(component.get_extents(CoordType::Window)).await {
                    Some(Ok((x, y, width, height))) if x >= 0 && y >= 0 => {
                        dlog!(
                            "Wayland web document extents: target={xid:#x} pid={pid} document=({x},{y},{width},{height}) hyprland={hyprland_window:?}"
                        );
                        let inferred_top = match (compositor, sway_window.as_ref()) {
                            (Some((_, 0)), Some(window))
                                if width > 0
                                    && height > 0
                                    && (i64::from(window.width) - i64::from(width)).abs() <= 4
                                    && i64::from(window.height) > i64::from(height) =>
                            {
                                (i64::from(window.height) - i64::from(height))
                                    .min(i64::from(i32::MAX)) as i32
                            }
                            _ => 0,
                        };
                        let top = hyprland_document_top_inset(
                            (xid, pid),
                            hyprland_window,
                            (x, y, width, height),
                        )
                        .unwrap_or(y.max(inferred_top));
                        Some((x, top))
                    }
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
    } else {
        None
    };
    let combined = combine_wayland_content_offsets(compositor, document, document_is_separate);
    dlog!(
        "Wayland web content offset: compositor={compositor:?} document={document:?} separate_process={document_is_separate} combined={combined:?}"
    );
    combined
}

fn screen_extent_rebase(
    x11_origin: (i32, i32),
    accessible_frame_extents: (i32, i32, i32, i32),
    top_level: Option<(i32, i32, i32, i32)>,
) -> Option<(i32, i32)> {
    if !crate::snapshot_queries::plausible_raw_extents(accessible_frame_extents) {
        return None;
    }
    let (frame_x, frame_y, _, _) = accessible_frame_extents;
    // Chromium's broken "Screen" provider is rooted at the renderer-local
    // origin. A legitimate screen provider may differ from the X11 client
    // origin by title-bar/CSD extents; rebasing that small decoration delta
    // would move otherwise-correct GTK coordinates off their controls.
    if top_level == Some(accessible_frame_extents) {
        return None;
    }
    if frame_x.abs() <= 2 && frame_y.abs() <= 2 {
        Some((x11_origin.0 - frame_x, x11_origin.1 - frame_y))
    } else {
        None
    }
}

fn rebase_renderer_window_offset(
    mut offset: (i32, i32),
    frame_origin: Option<(i32, i32)>,
) -> (i32, i32) {
    if let Some((frame_x, frame_y)) = frame_origin {
        // Chromium may expose a negative renderer-local frame origin. Rebase
        // that shape, but keep positive content insets: its descendants are
        // already relative to the content origin and subtracting the inset
        // moves first-row controls above the captured Wayland window.
        if frame_x < 0 {
            offset.0 = offset.0.saturating_sub(frame_x);
        }
        if frame_y < 0 {
            offset.1 = offset.1.saturating_sub(frame_y);
        }
    }
    offset
}

fn project_screen_extents(
    (x, y, w, h): (i32, i32, i32, i32),
    (offset_x, offset_y): (i32, i32),
    document_origin: Option<(i32, i32)>,
) -> Option<(i32, i32, u32, u32)> {
    // Reject unrealized-widget sentinels before applying any offsets.
    if !crate::snapshot_queries::plausible_raw_extents((x, y, w, h)) {
        return None;
    }
    let (document_x, document_y) = document_origin.unwrap_or((0, 0));
    Some((
        x + offset_x + document_x,
        y + offset_y + document_y,
        w as u32,
        h as u32,
    ))
}

fn scoped_component_nodes<'a, T>(
    nodes: &'a [T],
    scoped_frame: Option<usize>,
    component: impl Fn(&T) -> (usize, bool) + 'a,
) -> impl Iterator<Item = (usize, &'a T)> + 'a {
    // Filter after enumeration: public element indices belong to the whole app.
    nodes.iter().enumerate().filter(move |(_, node)| {
        let (frame, has_component) = component(node);
        has_component && scoped_frame.is_none_or(|scope| scope == frame)
    })
}

const WINDOW_SCOPE_UNPROVEN: &str = "the accessibility frame of this GNOME window is unproven \
     and its process has other windows";
const ELEMENT_OUTSIDE_WINDOW: &str = "element does not belong to the requested window";

/// A window-bound element request failed its window-membership proof. Callers
/// acting for an exact window refuse on it instead of acting elsewhere.
#[derive(Debug)]
pub struct WindowMembershipUnproven(&'static str);

impl std::fmt::Display for WindowMembershipUnproven {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for WindowMembershipUnproven {}

pub fn is_window_membership_unproven(error: &anyhow::Error) -> bool {
    error.downcast_ref::<WindowMembershipUnproven>().is_some()
}

/// The AT-SPI window list's handle for top-level `ordinal` of `pid`: stable,
/// non-zero and unique per (pid, ordinal).
pub(crate) fn accessibility_window_id(pid: u32, ordinal: usize) -> u64 {
    ((u64::from(pid) << 16) | ordinal as u64).max(1)
}

/// GNOME ties an AT-SPI top-level to a Shell window only through frame
/// correlation. Without it, a window-bound query would admit every same-PID
/// window's nodes and project them with the requested window's origin, so a
/// sibling's control could be hit-tested or bounded as the requested one.
///
/// A correlated `scoped_frame` is proof. Otherwise the attested Shell must
/// list exactly one window for the PID, the requested one. With no attested
/// Shell, a GNOME session proves nothing, except that the AT-SPI window list's
/// own handle for an application's first top-level names that top-level
/// (`accessibility_handle`). Compositors without a GNOME Shell keep their
/// route.
pub(crate) fn gnome_window_scope_proven(
    xid: u64,
    scoped_frame: Option<usize>,
    accessibility_handle: bool,
    gnome_session: impl FnOnce() -> bool,
    pid_shell_windows: impl FnOnce() -> Option<Vec<u64>>,
) -> bool {
    if xid == 0 || scoped_frame.is_some() {
        return true;
    }
    match pid_shell_windows() {
        Some(ids) => ids == [xid],
        None => accessibility_handle || !gnome_session(),
    }
}

/// Whether an application's lone AT-SPI top-level renders window `xid` on a
/// Wayland session outside Hyprland.
pub(crate) fn lone_top_level_renders(
    pid: u32,
    xid: u64,
    gnome_session: impl FnOnce() -> bool,
    pid_shell_windows: impl FnOnce() -> Option<Vec<u64>>,
) -> bool {
    gnome_window_scope_proven(
        xid,
        None,
        xid == accessibility_window_id(pid, 0),
        gnome_session,
        pid_shell_windows,
    )
}

fn lone_top_level_proves_window(pid: u32, xid: u64) -> bool {
    lone_top_level_renders(
        pid,
        xid,
        crate::wayland::shell_helper::gnome_shell_session,
        || crate::wayland::shell_helper::trusted_window_ids_for_pid(pid),
    )
}

/// An element answers a window-bound query only from a proven window scope,
/// and only from that window's top-level when one was correlated.
fn window_bound_element(
    element_frame: usize,
    scoped_frame: Option<usize>,
    scope_proven: impl FnOnce() -> bool,
) -> Result<()> {
    if !scope_proven() {
        return Err(WindowMembershipUnproven(WINDOW_SCOPE_UNPROVEN).into());
    }
    if scoped_frame.is_some_and(|scope| element_frame != scope) {
        return Err(WindowMembershipUnproven(ELEMENT_OUTSIDE_WINDOW).into());
    }
    Ok(())
}

/// The window-scope rule for a walk of window `xid`. X11 and Hyprland keep
/// their own routes.
fn window_scope_proven(pid: u32, xid: u64, scoped_frame: Option<usize>) -> bool {
    if !crate::wayland::is_wayland() || crate::wayland::hyprland::is_session() {
        return true;
    }
    gnome_window_scope_proven(
        xid,
        scoped_frame,
        false,
        crate::wayland::shell_helper::gnome_shell_session,
        || crate::wayland::shell_helper::trusted_window_ids_for_pid(pid),
    )
}

/// [`window_scope_proven`] from async code. Cases that need no Shell query
/// answer at once; the rest run off the runtime and refuse on timeout.
async fn window_scope_proven_async(pid: u32, xid: u64, scoped_frame: Option<usize>) -> bool {
    if xid == 0
        || scoped_frame.is_some()
        || !crate::wayland::is_wayland()
        || crate::wayland::hyprland::is_session()
    {
        return true;
    }
    bounded_blocking(move || window_scope_proven(pid, xid, scoped_frame))
        .await
        .unwrap_or(false)
}

/// [`window_bound_element`] where GNOME's rules apply: Wayland outside
/// Hyprland with an attested Shell helper or a GNOME session. X11, Hyprland
/// and compositors without a GNOME Shell keep their routes.
fn gnome_window_bound_element(
    pid: u32,
    xid: u64,
    element_frame: usize,
    scoped_frame: Option<usize>,
) -> Result<()> {
    if xid == 0 || !crate::wayland::is_wayland() || crate::wayland::hyprland::is_session() {
        return Ok(());
    }
    let shell_windows = crate::wayland::shell_helper::trusted_window_ids_for_pid(pid);
    if shell_windows.is_none() && !crate::wayland::shell_helper::gnome_shell_session() {
        return Ok(());
    }
    window_bound_element(element_frame, scoped_frame, || {
        gnome_window_scope_proven(xid, scoped_frame, false, || true, || shell_windows)
    })
}

async fn gnome_window_bound_element_async(
    pid: u32,
    xid: u64,
    element_frame: usize,
    scoped_frame: Option<usize>,
) -> Result<()> {
    if xid == 0 || !crate::wayland::is_wayland() || crate::wayland::hyprland::is_session() {
        return Ok(());
    }
    bounded_blocking(move || gnome_window_bound_element(pid, xid, element_frame, scoped_frame))
        .await
        .unwrap_or_else(|| {
            Err(WindowMembershipUnproven("the window-membership check timed out").into())
        })
}

/// Whether a snapshot of window `xid` may bind its elements to that window as
/// element tokens. Hyprland needs a correlated top-level. Elsewhere a native
/// walk's correlated top-level counts, or else the window-scope rule for an
/// uncorrelated walk: always on X11 and on compositors without a GNOME Shell,
/// and on GNOME only when the attested Shell lists exactly `xid` for the PID.
pub(crate) fn snapshot_binds_window(
    hyprland_session: bool,
    window_scoped: bool,
    trusted: bool,
    unscoped_walk_proven: impl FnOnce() -> bool,
) -> bool {
    if hyprland_session {
        return window_scoped;
    }
    (trusted && window_scoped) || unscoped_walk_proven()
}

pub fn snapshot_binds_window_for(pid: u32, xid: u64, window_scoped: bool, trusted: bool) -> bool {
    snapshot_binds_window(
        crate::wayland::is_wayland() && crate::wayland::hyprland::is_session(),
        window_scoped,
        trusted,
        || window_scope_proven(pid, xid, None),
    )
}

/// The element a window-bound action names: an index into the walk the
/// action makes, or the AT-SPI identity a snapshot observed.
pub enum WindowElement {
    Index(usize),
    Identity(super::AtspiIdentity),
}

impl WindowElement {
    fn find<'v, 'a>(&self, visited: &'v [Visited<'a>]) -> Option<&'v Visited<'a>> {
        match self {
            Self::Index(idx) => visited.iter().filter(|v| is_indexable(v)).nth(*idx),
            Self::Identity(identity) => visited
                .iter()
                .find(|v| v.identity.as_ref() == Some(identity)),
        }
    }
}

/// What [`require_window_element`] established for an element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowProof {
    /// A fresh walk scoped to the window found the element there. The action
    /// must act on that element and never re-resolve an index.
    Proven,
    /// The session keeps its own route (X11, Hyprland, compositors without a
    /// GNOME Shell); no proof was needed.
    NotRequired,
}

/// Membership of an element found at `element_frame` (`None` when the fresh
/// walk no longer has it) in a window-bound walk.
pub(crate) fn window_element_membership(
    element_frame: Option<usize>,
    scoped_frame: Option<usize>,
    scope_proven: impl FnOnce() -> bool,
) -> Result<()> {
    let frame = element_frame.ok_or(WindowMembershipUnproven(
        "the element is no longer in the application's accessibility tree",
    ))?;
    window_bound_element(frame, scoped_frame, scope_proven)
}

/// Fresh-walk proof that `element` still belongs to window `xid`, for an
/// action on behalf of that window. The walk is scoped to `xid` and judged by
/// the rule window-bound bounds use. X11, Hyprland and compositors without a
/// GNOME Shell keep their own routes ([`WindowProof::NotRequired`]).
pub fn require_window_element(pid: u32, xid: u64, element: WindowElement) -> Result<WindowProof> {
    if xid == 0 || !crate::wayland::is_wayland() || crate::wayland::hyprland::is_session() {
        return Ok(WindowProof::NotRequired);
    }
    let shell_windows = crate::wayland::shell_helper::trusted_window_ids_for_pid(pid);
    if shell_windows.is_none() && !crate::wayland::shell_helper::gnome_shell_session() {
        return Ok(WindowProof::NotRequired);
    }
    bounded_for(
        INDEX_RESOLVE_BUDGET,
        async move {
            let conn = shared_connection().await?;
            let Collected {
                visited,
                scoped_frame,
                ..
            } = collect_visited_bounded(&conn, pid, xid, None, None)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let element_frame = element.find(&visited).map(|v| v.frame_ordinal);
            window_element_membership(element_frame, scoped_frame, || {
                gnome_window_scope_proven(xid, scoped_frame, false, || true, || shell_windows)
            })
            .map(|()| WindowProof::Proven)
        },
        || {
            Err(WindowMembershipUnproven(
                "the window-membership walk timed out (app unresponsive to AT-SPI)",
            )
            .into())
        },
    )
}

/// Screen-coordinate bounds for the exact visited sequence rendered into the
/// current snapshot. Nodes without a usable Component interface, or whose
/// extents query fails/times out, are omitted rather than borrowing another
/// live traversal's ordinal.
///
/// GTK4 caveat: GTK4's AT-SPI bridge returns `GetExtents(Screen)` as `(0,0)`
/// for every element (issue #1564 / the #1739 a11y rework), so a screen query
/// is useless. Instead we query `CoordType::Window` (which GTK4 *does* report
/// correctly, per-widget) and add a deterministic screen offset — the X11
/// window origin plus the GTK4 CSD shadow inset from `_GTK_FRAME_EXTENTS` (see
/// [`window_to_screen_offset`]). For GTK3/Qt the inset is absent, so the
/// offset is just the X11 origin and the result matches the old screen path.
///
/// Returns `(element_index, x, y, width, height)` tuples.
async fn element_bounds_for_visited_with_completeness(
    visited: &[Visited<'_>],
    pid: u32,
    xid: u64,
    scoped_frame: Option<usize>,
) -> (Vec<(usize, i32, i32, u32, u32)>, bool) {
    // Query WINDOW-relative extents and add a deterministic screen offset
    // (X11 window origin + GTK4 CSD inset). This fixes GTK4 — whose
    // CoordType::Screen reports every element at (0,0) — by using the
    // distinct per-widget WINDOW coords instead. On Wayland / when no X11
    // window resolves, `offset` is None and we keep the legacy Screen path
    // so non-X11 behaviour is unchanged.
    let window_title = visited.iter().find_map(|node| {
        (scoped_frame.is_none_or(|scope| node.frame_ordinal == scope)
            && matches!(
                node.role.to_ascii_lowercase().as_str(),
                "frame" | "window" | "dialog" | "alert" | "file chooser"
            ))
        .then_some(node.name.as_str())
    });
    // Wayland's `window_to_screen_offset` path never touches X11 (it reads
    // compositor IPC / AT-SPI state instead), so only the X11 branch risks an
    // un-timed blocking round-trip; still route both through `bounded_blocking`
    // so this single call site can't stall the walk past its budget (#42).
    let window_title_owned = window_title.map(str::to_owned);
    let offset =
        bounded_blocking(move || window_to_screen_offset(pid, xid, window_title_owned.as_deref()))
            .await
            .flatten();
    if crate::wayland::is_wayland()
        && crate::wayland::hyprland::is_session()
        && (offset.is_none() || scoped_frame.is_none())
    {
        return (Vec::new(), true);
    }
    if offset.is_none()
        && bounded_blocking(move || exact_wayland_origin_required(xid))
            .await
            .unwrap_or(true)
    {
        return (Vec::new(), true);
    }
    if !window_scope_proven_async(pid, xid, scoped_frame).await {
        return (Vec::new(), true);
    }
    let coord = if offset.is_some() {
        CoordType::Window
    } else {
        CoordType::Screen
    };
    let display = if crate::wayland::is_wayland() {
        None
    } else {
        bounded_blocking(x11_display_size).await.flatten()
    };
    // Chromium on X11 labels its component extents as Screen while
    // returning coordinates relative to the renderer frame. Rebase
    // those values by comparing the top-level accessible frame with
    // the actual X11 window origin. Correct screen-coordinate providers
    // produce a zero delta; Chromium's local (0,0) frame produces the
    // required window-origin delta. GTK's explicit Window-coordinate
    // path above remains authoritative when available.
    let screen_rebase = if offset.is_none() && !crate::wayland::is_wayland() && xid != 0 {
        let x11_origin = bounded_blocking(move || x11_window_origin(xid))
            .await
            .flatten();
        let top_level = bounded_blocking(move || x11_top_level_geometry(xid))
            .await
            .flatten();
        let frame = visited.iter().find(|node| {
            scoped_frame.is_none_or(|scope| node.frame_ordinal == scope)
                && node.has_component
                && matches!(
                    node.role.to_ascii_lowercase().as_str(),
                    "frame" | "window" | "dialog" | "alert" | "file chooser"
                )
        });
        if let (Some(origin), Some(frame)) = (x11_origin, frame) {
            let accessible_frame = match call(frame.acc.proxies()).await {
                Some(Ok(proxies)) => match call(proxies.component()).await {
                    Some(Ok(component)) => {
                        match call(component.get_extents(CoordType::Screen)).await {
                            Some(Ok(extents)) => Some(extents),
                            _ => None,
                        }
                    }
                    _ => None,
                },
                _ => None,
            };
            accessible_frame
                .and_then(|frame_extents| screen_extent_rebase(origin, frame_extents, top_level))
        } else {
            None
        }
    } else {
        None
    };
    // Renderer bridges can expose Window coordinates relative to an internal
    // frame whose origin is not (0,0) (Chromium commonly reports a negative
    // title-bar offset). Normalize that frame to the compositor window origin
    // before adding the screen offset. Native GTK reports (0,0), so this is a
    // no-op there.
    let window_frame_origin = if offset.is_some() {
        let frame = visited.iter().find(|node| {
            scoped_frame.is_none_or(|scope| node.frame_ordinal == scope)
                && node.has_component
                && matches!(
                    node.role.to_ascii_lowercase().as_str(),
                    "frame" | "window" | "dialog" | "alert" | "file chooser"
                )
        });
        if let Some(frame) = frame {
            match call(frame.acc.proxies()).await {
                Some(Ok(proxies)) => match call(proxies.component()).await {
                    Some(Ok(component)) => {
                        match call(component.get_extents(CoordType::Window)).await {
                            Some(Ok((x, y, _, _))) => Some((x, y)),
                            _ => None,
                        }
                    }
                    _ => None,
                },
                _ => None,
            }
        } else {
            None
        }
    } else {
        None
    };
    // Add compositor decorations and the embedded document origin for web
    // descendants only. Electron commonly contributes zero for both; WebKitGTK
    // under Sway needs the sum.
    let web_document_origin = if offset.is_some() {
        web_document_origin_for_visited(visited, pid, xid, scoped_frame).await
    } else {
        None
    };
    let (offset_x, offset_y) = rebase_renderer_window_offset(
        offset.or(screen_rebase).unwrap_or((0, 0)),
        window_frame_origin,
    );
    if let Some((ox, oy)) = offset {
        dlog!("element bounds: WINDOW coords + screen offset ({ox},{oy})");
    } else if let Some((ox, oy)) = screen_rebase {
        dlog!("element bounds: SCREEN coords + X11 frame rebase ({ox},{oy})");
    }

    let action_nodes: Vec<&Visited> = visited.iter().filter(|v| is_indexable(v)).collect();
    // Hard wall-clock budget for the whole collection: on pathological
    // trees individual D-Bus calls each burn up to CALL_TIMEOUT (geany's
    // unrealized nodes did exactly that). Return whatever was collected
    // in time, but do not impose an index-based node cap: a cap silently
    // stripped frames from valid controls later in renderer trees and
    // made PX targeting depend on DOM order.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20).min(remaining_budget());
    // Bounds are independent read-only queries. Overlap a bounded number of
    // calls instead of serializing thousands of unrealized menu components.
    // Preserve original indices, all extents checks, and per-call timeouts.
    let origin_attested = bounded_blocking(move || native_wayland_origin_attested(pid, xid))
        .await
        .unwrap_or(false);
    let queries = scoped_component_nodes(&action_nodes, scoped_frame, |node| {
        (node.frame_ordinal, node.has_component)
    })
    .map(|(idx, node)| async move {
        let proxies = call(node.acc.proxies()).await?.ok()?;
        let comp = call(proxies.component()).await?.ok()?;
        // A trustworthy Screen answer wins over the Window reconstruction
        // (see `screen_extents_trusted`); web content keeps its document
        // origin path.
        if coord == CoordType::Window && !node.in_web_doc {
            if let Some(Ok(raw)) = call(comp.get_extents(CoordType::Screen)).await {
                if screen_answer_overrides_window(raw, display, origin_attested) {
                    return project_screen_extents(raw, (0, 0), None).map(|bounds| (idx, bounds));
                }
            }
        }
        if let Some(Ok((x, y, w, h))) = call(comp.get_extents(coord)).await {
            let document_origin = if node.in_web_doc {
                web_document_origin
            } else {
                None
            };
            return project_screen_extents((x, y, w, h), (offset_x, offset_y), document_origin)
                .map(|bounds| (idx, bounds));
        }
        None
    });
    let collected = crate::snapshot_queries::collect_indexed(queries, deadline).await;
    let complete = tokio::time::Instant::now() < deadline;
    if !complete {
        dlog!(
            "snapshot bounds: time budget exhausted; returning {} bound(s)",
            collected.len()
        );
    }
    (
        collected
            .into_iter()
            .map(|(idx, (x, y, w, h))| (idx, x, y, w, h))
            .collect(),
        complete,
    )
}

#[cfg(test)]
mod walk_bounds_tests {
    use super::walk_bounds_usable;

    #[test]
    fn degenerate_window_origin_is_rejected() {
        // A closed menu item projects (0,0,0,0) onto the window origin.
        assert_eq!(
            walk_bounds_usable(Some((40, 60, 0, 0)), Some((1920, 1080))),
            None
        );
        assert_eq!(walk_bounds_usable(None, Some((1920, 1080))), None);
    }

    #[test]
    fn off_screen_bounds_are_rejected() {
        assert_eq!(
            walk_bounds_usable(Some((2000, 10, 50, 20)), Some((1920, 1080))),
            None
        );
        assert_eq!(
            walk_bounds_usable(Some((-80, 10, 50, 20)), Some((1920, 1080))),
            None
        );
    }

    #[test]
    fn real_bounds_pass_through() {
        assert_eq!(
            walk_bounds_usable(Some((100, 200, 80, 24)), Some((1920, 1080))),
            Some((100, 200, 80, 24))
        );
        assert_eq!(
            walk_bounds_usable(Some((100, 200, 80, 24)), None),
            Some((100, 200, 80, 24))
        );
    }
}

#[cfg(test)]
mod screen_extents_tests {
    use super::screen_extents_trusted;

    #[test]
    fn trusts_plausible_on_screen_extents() {
        assert!(screen_extents_trusted(
            (70, 110, 848, 433),
            Some((1920, 1080))
        ));
        assert!(screen_extents_trusted(
            (76, 1045, 43, 32),
            Some((1920, 1080))
        ));
    }

    #[test]
    fn rejects_sentinels_and_off_screen_answers() {
        // GTK4 (newer a11y) collapses every element to the origin.
        assert!(!screen_extents_trusted((0, 0, 100, 20), Some((1920, 1080))));
        // Unrealized widgets report INT_MIN.
        assert!(!screen_extents_trusted(
            (i32::MIN, i32::MIN, 1, 1),
            Some((1920, 1080))
        ));
        // Entirely off the display.
        assert!(!screen_extents_trusted(
            (2000, 10, 40, 20),
            Some((1920, 1080))
        ));
        assert!(!screen_extents_trusted(
            (-500, 10, 40, 20),
            Some((1920, 1080))
        ));
    }
}

#[cfg(test)]
mod screen_override_tests {
    use super::{project_screen_extents, screen_answer_overrides_window};

    #[test]
    fn attested_origin_rejects_window_local_screen_answer() {
        // GTK3 on Hyprland reports window-local (12,211) as Screen; with no
        // display bounds it would otherwise pass `screen_extents_trusted`.
        let local = (12, 211, 917, 34);
        assert!(screen_answer_overrides_window(local, None, false));
        assert!(!screen_answer_overrides_window(local, None, true));
        // Window extents plus the attested nonzero origin give screen space.
        assert_eq!(
            project_screen_extents(local, (955, 349), None),
            Some((967, 560, 917, 34))
        );
    }

    #[test]
    fn xwayland_or_unattested_screen_answer_wins() {
        // XWayland GTK/LibreOffice report true screen extents; without an
        // exact native-client attestation they must not be discarded. A zero
        // address never attests, without consulting the compositor.
        assert!(!super::native_wayland_origin_attested(1, 0));
        assert!(super::screen_answer_overrides_window(
            (967, 560, 917, 34),
            None,
            super::native_wayland_origin_attested(1, 0)
        ));
    }

    #[test]
    fn native_attested_window_local_screen_answer_loses_to_window_extents() {
        assert!(!super::screen_answer_overrides_window(
            (10, 20, 800, 600),
            None,
            true
        ));
    }

    #[test]
    fn x11_screen_answer_still_wins_with_display_bounds() {
        let screen = (70, 110, 848, 433);
        assert!(screen_answer_overrides_window(
            screen,
            Some((1920, 1080)),
            false
        ));
    }

    #[test]
    fn invalid_screen_answers_never_override() {
        for attested in [false, true] {
            assert!(!screen_answer_overrides_window(
                (i32::MIN, i32::MIN, 1, 1),
                None,
                attested
            ));
            assert!(!screen_answer_overrides_window(
                (0, 0, 100, 20),
                None,
                attested
            ));
        }
        assert_eq!(
            project_screen_extents((i32::MIN, i32::MIN, 1, 1), (955, 349), None),
            None
        );
    }

    #[test]
    fn web_document_origin_is_added_after_window_offset() {
        assert_eq!(
            project_screen_extents((10, 20, 30, 40), (955, 349), Some((0, 80))),
            Some((965, 449, 30, 40))
        );
    }
}

#[cfg(test)]
mod stacking_z_index_tests {
    use super::z_index_from_stacking;
    use crate::x11::WindowInfo;

    fn window(pid: u32, title: &str, z: usize) -> WindowInfo {
        WindowInfo {
            xid: 100 + z as u64,
            pid: Some(pid),
            app_name: "app".into(),
            title: title.into(),
            is_on_screen: true,
            z_index: Some(z),
            x: 0,
            y: 0,
            width: 10,
            height: 10,
            native_window_id: None,
            target_id: None,
            helper_epoch: None,
            transient_for_window_id: None,
            transient_for_target_id: None,
            is_attached_dialog: None,
            is_modal: None,
            window_type: None,
            workspace_index: None,
            workspace_active: None,
            sticky: None,
            monitor: None,
            capture_current: None,
            identity_capabilities: None,
        }
    }

    #[test]
    fn frames_take_the_stacking_position_of_the_same_pid_and_title() {
        let stacking = vec![
            window(7, "GNU Image Manipulation Program", 2),
            window(9, "Other", 3),
            window(7, "Brightness-Contrast", 4),
        ];
        assert_eq!(
            z_index_from_stacking(&stacking, 7, "Brightness-Contrast"),
            Some(4)
        );
        assert_eq!(
            z_index_from_stacking(&stacking, 7, "GNU Image Manipulation Program"),
            Some(2)
        );
        // Unknown title with several candidates: no guess.
        assert_eq!(z_index_from_stacking(&stacking, 7, "Untitled"), None);
        // A pid with exactly one X toplevel maps regardless of the title.
        assert_eq!(z_index_from_stacking(&stacking, 9, "anything"), Some(3));
        assert_eq!(z_index_from_stacking(&stacking, 11, "anything"), None);
        assert_eq!(z_index_from_stacking(&[], 7, "anything"), None);
    }
}

#[cfg(test)]
mod frame_correlation_tests {
    use super::{correlate_frame_to_window, FRAME_MATCH_TOLERANCE_PX};
    use crate::x11::WindowInfo;

    fn window(x: i32, y: i32, width: u32, height: u32) -> WindowInfo {
        WindowInfo {
            xid: 4242,
            pid: Some(99),
            app_name: "Google-chrome".to_owned(),
            title: "Cua - Google Chrome".to_owned(),
            is_on_screen: true,
            z_index: Some(3),
            x,
            y,
            width,
            height,
            native_window_id: None,
            target_id: None,
            helper_epoch: None,
            transient_for_window_id: None,
            transient_for_target_id: None,
            is_attached_dialog: None,
            is_modal: None,
            window_type: None,
            workspace_index: None,
            workspace_active: None,
            sticky: None,
            monitor: None,
            capture_current: None,
            identity_capabilities: None,
        }
    }

    /// The configuration that made the existing-profile route unreachable: one
    /// browser process publishing three windows.
    #[test]
    fn picks_the_frame_matching_the_named_window_among_siblings() {
        let candidates = [
            (0usize, (144, 51, 1244, 953)),
            (1, (438, 80, 1050, 953)),
            (2, (550, 225, 500, 584)),
        ];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(438, 80, 1050, 953)),
            Some(1)
        );
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(550, 225, 500, 584)),
            Some(2)
        );
    }

    /// Server-side decorations shift a frame's reported origin; a small offset
    /// must still resolve rather than fall back to an application-wide walk.
    #[test]
    fn tolerates_decoration_offsets() {
        let candidates = [(0usize, (440, 108, 1050, 925)), (1, (144, 51, 1244, 953))];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(438, 80, 1050, 953)),
            Some(0)
        );
    }

    /// Two windows of the same geometry cannot be told apart this way, and the
    /// caller needs a refusal rather than a coin flip.
    #[test]
    fn refuses_when_two_frames_are_equally_plausible() {
        let candidates = [(0usize, (438, 80, 1050, 953)), (1, (438, 80, 1050, 953))];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(438, 80, 1050, 953)),
            None
        );
    }

    #[test]
    fn refuses_when_no_frame_is_close_enough() {
        let candidates = [(0usize, (0, 0, 200, 200))];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(438, 80, 1050, 953)),
            None
        );
    }

    #[test]
    fn refuses_when_the_application_publishes_no_frame_extents() {
        assert_eq!(
            correlate_frame_to_window(&[], &window(438, 80, 1050, 953)),
            None
        );
    }

    /// Zero-area extents are what a frame reports before it has been mapped;
    /// they must never be treated as a match for a real window.
    #[test]
    fn ignores_frames_without_usable_extents() {
        let candidates = [(0usize, (0, 0, 0, 0)), (1, (438, 80, 1050, 953))];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(438, 80, 1050, 953)),
            Some(1)
        );
    }

    /// Being the only candidate is not evidence of correspondence. (The walk
    /// does short-circuit a genuinely single-top-level application before it
    /// reaches this function — see `resolve_window_frame`.)
    #[test]
    fn a_sole_candidate_still_has_to_be_close_enough() {
        let far_away = i32::try_from(FRAME_MATCH_TOLERANCE_PX).unwrap() + 500;
        let candidates = [(0usize, (far_away, far_away, 1050, 953))];
        assert_eq!(
            correlate_frame_to_window(&candidates, &window(438, 80, 1050, 953)),
            None
        );
    }
}

#[cfg(test)]
mod coord_tests {
    use super::{
        activation_index, before_snapshot_deadline, bounded_blocking,
        combine_wayland_content_offsets, counts_as_showing,
        ensure_element_descends_from_exact_window, exact_window_top_level_key, is_enabled_state,
        is_indexable_capabilities, is_passive_role, is_showing_state, is_web_process_bus,
        listing_state, passive_marker, prefer_authoritative_wayland_origin, push_action_name_slot,
        rebase_renderer_window_offset, require_affirmative_ack, screen_extent_rebase,
        select_click_target, selected_key_at_point, wayland_window_origin, ApplicationSelection,
        OP_DEADLINE,
    };
    use super::{element_key_for_object, RawObjectRef};
    use super::{
        hyprland_document_top_inset, parse_gtk_frame_extents, project_screen_extents,
        scoped_component_nodes, select_web_document,
    };
    use atspi::{State, StateSet};
    use std::time::Duration;

    #[test]
    fn failed_action_name_lookup_preserves_the_original_action_index() {
        let mut actions = Vec::new();
        push_action_name_slot(&mut actions, Some("first".to_owned()));
        push_action_name_slot(&mut actions, None);
        push_action_name_slot(&mut actions, Some("third".to_owned()));

        assert_eq!(actions, ["first", "", "third"]);
        assert_eq!(actions.iter().position(|action| action == "third"), Some(2));
    }

    #[test]
    fn point_reorder_changes_recipient_and_must_refuse() {
        let keys = [(0, 0x41), (1, 0x42)];
        let initial = [(0, 10, 10, 20, 20, false), (1, 40, 10, 20, 20, false)];
        let reordered = [(0, 40, 10, 20, 20, false), (1, 10, 10, 20, 20, false)];
        assert_eq!(selected_key_at_point(&initial, &keys, 15, 15), Some(0x41));
        assert_eq!(selected_key_at_point(&reordered, &keys, 15, 15), Some(0x42));
    }

    #[test]
    fn false_native_acknowledgement_is_a_failure() {
        assert!(require_affirmative_ack(false, "SetTextContents").is_err());
        assert!(require_affirmative_ack(true, "SetTextContents").is_ok());
    }

    #[test]
    fn activation_selection_never_falls_back_to_a_destructive_first_action() {
        let actions = vec![
            "buffer.delete-line".to_owned(),
            "clipboard.copy".to_owned(),
            "activate".to_owned(),
        ];

        assert_eq!(activation_index("text", &actions), Some(2));
        assert_eq!(
            activation_index("text", &["buffer.delete-line".to_owned()]),
            None
        );
    }

    #[test]
    fn duplicate_pid_prefers_populated_application_after_empty_registration() {
        let target_pid = 4242;
        let candidates = [
            (Some(9000), "other-process", true),
            (Some(target_pid), "empty-root", false),
            (Some(target_pid), "live-tree", true),
        ];
        let mut selection = ApplicationSelection::new(target_pid);
        for (pid, app, has_children) in candidates {
            if selection.matches_pid(pid) {
                selection.consider_matching(app, has_children);
            }
        }

        assert_eq!(selection.into_selected(), Ok(Some("live-tree")));
    }

    #[test]
    fn childless_exact_pid_application_remains_the_fallback() {
        let mut selection = ApplicationSelection::new(4242);
        selection.consider_matching("first-empty", false);
        selection.consider_matching("second-empty", false);

        assert_eq!(selection.into_selected(), Ok(Some("first-empty")));
    }

    #[test]
    fn multiple_populated_exact_pid_applications_are_ambiguous() {
        let mut selection = ApplicationSelection::new(4242);
        selection.consider_matching("first-live-tree", true);
        selection.consider_matching("second-live-tree", true);

        assert_eq!(selection.into_selected(), Err(2));
    }

    #[tokio::test(start_paused = true)]
    async fn one_absolute_deadline_spans_traversal_and_bounds() {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);

        before_snapshot_deadline(deadline, tokio::time::sleep(Duration::from_millis(60)))
            .await
            .expect("traversal should fit the shared budget");
        before_snapshot_deadline(deadline, tokio::time::sleep(Duration::from_millis(60)))
            .await
            .expect_err("bounds must receive only the traversal's remaining budget");

        assert_eq!(tokio::time::Instant::now(), deadline);
    }

    // Regression coverage for #42: a synchronous, un-timed blocking call
    // (the shape of `window_to_screen_offset`/`x11_window_origin`'s raw X11
    // round-trips) executed directly inside the bounds phase must not be
    // able to hold the walk hostage past the operation's deadline, even
    // though the blocking work itself cannot be cancelled once started.
    #[tokio::test]
    async fn bounded_blocking_returns_none_once_the_operation_deadline_passes() {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
        let started = std::time::Instant::now();

        let result = OP_DEADLINE
            .scope(deadline, async {
                // Simulates a wedged X11 socket read: a plain blocking sleep
                // with no cooperative await points of its own, far longer
                // than the operation's deadline.
                bounded_blocking(|| {
                    std::thread::sleep(Duration::from_secs(2));
                    "would-be X11 reply"
                })
                .await
            })
            .await;

        assert_eq!(
            result, None,
            "bounded_blocking must give up once the deadline passes rather than \
             waiting for the blocking closure to finish"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the caller must not be held up by the still-running blocking thread; \
             took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn bounded_blocking_returns_none_immediately_once_budget_is_exhausted() {
        // A deadline already in the past means `remaining_budget()` is zero,
        // so `bounded_blocking` must refuse to even spawn the closure —
        // exercising the fast-bail branch the per-node `call()` helper shares.
        let deadline = tokio::time::Instant::now();
        tokio::time::sleep(Duration::from_millis(1)).await;

        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ran_in_closure = ran.clone();
        let result = OP_DEADLINE
            .scope(deadline, async {
                bounded_blocking(move || {
                    ran_in_closure.store(true, std::sync::atomic::Ordering::SeqCst);
                    42
                })
                .await
            })
            .await;

        assert_eq!(result, None);
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "an already-exhausted budget must skip the closure entirely"
        );
    }

    #[test]
    fn operable_nodes_are_addressable() {
        assert!(is_indexable_capabilities(
            "entry",
            false,
            true,
            false,
            false,
            true,
            Some(true)
        ));
        assert!(is_indexable_capabilities(
            "button",
            true,
            false,
            false,
            false,
            false,
            Some(true)
        ));
        assert!(is_indexable_capabilities(
            "slider",
            false,
            false,
            true,
            false,
            true,
            Some(true)
        ));
        assert!(is_indexable_capabilities(
            "list item",
            false,
            false,
            false,
            true,
            true,
            Some(true)
        ));
        assert!(!is_indexable_capabilities(
            "label",
            false,
            false,
            false,
            false,
            true,
            Some(true)
        ));
        assert!(!is_indexable_capabilities(
            "button",
            true,
            false,
            false,
            false,
            true,
            Some(false)
        ));
        assert!(!is_indexable_capabilities(
            "button", true, false, false, false, true, None
        ));
        assert!(!is_indexable_capabilities(
            "label",
            true,
            false,
            false,
            false,
            true,
            Some(true)
        ));
    }

    #[test]
    fn hidden_and_disabled_controls_are_not_indexed_but_are_named() {
        // `Showing` is the on-screen state.
        assert!(!is_showing_state(&StateSet::new(
            State::Enabled | State::Visible
        )));
        assert!(is_showing_state(&StateSet::new(
            State::Enabled | State::Visible | State::Showing
        )));
        // A Visible-but-not-Showing control (scrolled out of its viewport, a
        // hover-only eject button) stays listed, marked off-screen; a hidden
        // one (not Visible) is left out.
        assert_eq!(
            listing_state(
                "push button",
                &StateSet::new(State::Enabled | State::Visible)
            ),
            (true, true)
        );
        assert_eq!(
            listing_state("push button", &StateSet::new(State::Enabled)),
            (false, false)
        );
        assert_eq!(
            listing_state(
                "push button",
                &StateSet::new(State::Enabled | State::Visible | State::Showing)
            ),
            (true, false)
        );
        // ...but a GTK2 menu item is never `Showing`, and stays indexed while `Visible`.
        assert!(!counts_as_showing(
            "push button",
            &StateSet::new(State::Enabled | State::Visible)
        ));
        assert!(counts_as_showing(
            "menu item",
            &StateSet::new(State::Enabled | State::Visible)
        ));
        assert!(counts_as_showing(
            "check menu item",
            &StateSet::new(State::Visible)
        ));
        assert!(!counts_as_showing(
            "menu item",
            &StateSet::new(State::Enabled)
        ));
        // A visible but insensitive button keeps a marker in the markdown.
        assert_eq!(
            passive_marker("push button", true, true, Some(false)),
            " (disabled)"
        );
        assert_eq!(
            passive_marker("push button", false, true, Some(false)),
            " (disabled)"
        );
        assert_eq!(passive_marker("push button", true, true, Some(true)), "");
        assert_eq!(passive_marker("label", false, true, Some(false)), "");
    }

    #[test]
    fn gtk_state_sets_establish_operability_from_enabled_or_sensitive() {
        assert!(is_enabled_state(&StateSet::new(State::Enabled)));
        assert!(is_enabled_state(&StateSet::new(State::Sensitive)));
        assert!(is_enabled_state(&StateSet::new(
            State::Enabled | State::Sensitive
        )));
        assert!(!is_enabled_state(&StateSet::empty()));
    }

    #[test]
    fn enabled_component_backed_buttons_are_pixel_addressable() {
        for role in ["button", "push button", " Button "] {
            assert!(is_indexable_capabilities(
                role,
                false,
                false,
                false,
                false,
                true,
                Some(true)
            ));
        }
    }

    #[test]
    fn component_role_fallback_rejects_unverified_or_passive_nodes() {
        for enabled in [None, Some(false)] {
            assert!(!is_indexable_capabilities(
                "button", false, false, false, false, true, enabled
            ));
        }
        assert!(!is_indexable_capabilities(
            "button",
            false,
            false,
            false,
            false,
            false,
            Some(true)
        ));
        for role in ["label", "application", "panel", "frame", "window"] {
            assert!(!is_indexable_capabilities(
                role,
                false,
                false,
                false,
                false,
                true,
                Some(true)
            ));
        }
    }

    #[test]
    fn element_keys_bind_bus_destination_and_object_path() {
        let first = RawObjectRef {
            name: ":1.44".to_owned(),
            path: "/org/a11y/atspi/accessible/7".to_owned(),
        };
        let same = first.clone();
        let other_path = RawObjectRef {
            path: "/org/a11y/atspi/accessible/8".to_owned(),
            ..first.clone()
        };
        let other_destination = RawObjectRef {
            name: ":1.45".to_owned(),
            ..first.clone()
        };

        assert_eq!(
            element_key_for_object(&first),
            element_key_for_object(&same)
        );
        assert_ne!(
            element_key_for_object(&first),
            element_key_for_object(&other_path)
        );
        assert_ne!(
            element_key_for_object(&first),
            element_key_for_object(&other_destination)
        );
    }

    #[test]
    fn exact_window_ancestry_accepts_the_single_accessible_toplevel() {
        assert_eq!(
            exact_window_top_level_key(&[(0xa11, 0)], 42, 0xdead_beef).unwrap(),
            0xa11
        );
    }

    #[test]
    fn exact_window_ancestry_uses_matching_native_ordinal() {
        let second_window_id = ((42_u64) << 16) | 1;
        assert_eq!(
            exact_window_top_level_key(&[(0xa11, 0), (0xb22, 1)], 42, second_window_id).unwrap(),
            0xb22
        );
    }

    #[test]
    fn exact_window_ancestry_fails_closed_when_roots_are_ambiguous() {
        let error =
            exact_window_top_level_key(&[(0xa11, 0), (0xb22, 1)], 42, 0xdead_beef).unwrap_err();
        assert!(error.to_string().contains("multiple top-level ancestries"));
    }

    #[test]
    fn exact_window_ancestry_fails_closed_on_duplicate_ordinal() {
        let first_window_id = (42_u64) << 16;
        let error =
            exact_window_top_level_key(&[(0xa11, 0), (0xb22, 0)], 42, first_window_id).unwrap_err();
        assert!(error.to_string().contains("window ancestry is ambiguous"));
    }

    #[test]
    fn verified_element_ancestry_rejects_same_pid_other_window() {
        assert!(ensure_element_descends_from_exact_window(0xc33, 0xb22, 0xa11, 99).is_err());
        assert!(ensure_element_descends_from_exact_window(0xc33, 0xa11, 0xa11, 99).is_ok());
    }

    #[test]
    fn screen_extents_are_rebased_from_accessible_frame_to_x11_origin() {
        let frame = Some((600, 50, 800, 600));
        assert_eq!(
            screen_extent_rebase((604, 80), (0, 0, 792, 566), frame),
            Some((604, 80))
        );
        assert_eq!(
            screen_extent_rebase((604, 100), (604, 80, 792, 566), frame),
            None
        );
        assert_eq!(
            screen_extent_rebase((604, 80), (604, 80, 792, 566), frame),
            None
        );
    }

    #[test]
    fn sentinel_frame_extents_are_not_a_renderer_origin() {
        assert_eq!(
            screen_extent_rebase((155, 149), (-1, -1, -1, -1), None),
            None
        );
        assert_eq!(screen_extent_rebase((155, 149), (0, 0, 0, 0), None), None);
    }

    #[test]
    fn a_renderer_local_frame_at_the_corner_keeps_its_rebase() {
        assert_eq!(
            screen_extent_rebase((5, 29), (0, 0, 640, 480), Some((0, 0, 650, 514))),
            Some((5, 29))
        );
        assert_eq!(
            screen_extent_rebase((5, 56), (0, 0, 640, 480), Some((0, 27, 650, 514))),
            Some((5, 56))
        );
        assert_eq!(
            screen_extent_rebase((5, 29), (0, 0, 640, 480), None),
            Some((5, 29))
        );
    }

    #[test]
    fn a_gtk_frame_matching_its_window_manager_frame_at_the_corner_is_not_rebased() {
        assert_eq!(
            screen_extent_rebase((5, 29), (0, 0, 650, 514), Some((0, 0, 650, 514))),
            None
        );
    }

    #[test]
    fn renderer_window_offset_only_rebases_negative_frame_origins() {
        assert_eq!(
            rebase_renderer_window_offset((100, 50), Some((-8, -29))),
            (108, 79)
        );
        assert_eq!(
            rebase_renderer_window_offset((100, 50), Some((0, 29))),
            (100, 50)
        );
    }

    #[test]
    fn point_hit_combines_negative_renderer_origin_and_web_process_inset() {
        let offset = rebase_renderer_window_offset((100, 50), Some((-8, -29)));
        let document = combine_wayland_content_offsets(None, Some((0, 47)), true);
        let (x, y, w, h) = project_screen_extents((450, 270, 40, 20), offset, document).unwrap();
        assert_eq!((x, y, w, h), (558, 396, 40, 20));
        // Keep the application-wide index and prefer the button over its label.
        let frames = [(82, x, y, w, h, false), (83, x + 1, y + 1, 38, 18, true)];
        assert_eq!(select_click_target(&frames, 569, 404), Some(82));
        assert_eq!(select_click_target(&frames, 569, 375), None);
        assert_eq!(
            project_screen_extents((450, 270, 40, 20), offset, None),
            Some((558, 349, 40, 20))
        );
    }

    #[test]
    fn point_hit_projection_rejects_unrealized_extents_before_offsets() {
        // The -16390 rows fall below the -16384 sentinel floor only before
        // the offsets are applied, so they fail if the check moves after them.
        for raw in [
            (i32::MIN, 0, 40, 20),
            (0, i32::MIN, 40, 20),
            (-16390, 0, 40, 20),
            (0, -16390, 40, 20),
            (0, 0, 1, 1),
        ] {
            assert_eq!(project_screen_extents(raw, (108, 79), Some((0, 47))), None);
        }
    }

    #[test]
    fn scoped_bounds_keep_application_indices_and_original_target_nodes() {
        let nodes = [
            (0, true, "sibling button"),
            (1, false, "target without geometry"),
            (1, true, "target button"),
            (0, true, "sibling entry"),
            (1, true, "target entry"),
        ];
        let scoped: Vec<_> = scoped_component_nodes(&nodes, Some(1), |node| (node.0, node.1))
            .map(|(index, node)| (index, node.2))
            .collect();
        assert_eq!(scoped, [(2, "target button"), (4, "target entry")]);
        assert_eq!(nodes[scoped[0].0].2, "target button");
        let all: Vec<_> = scoped_component_nodes(&nodes, None, |node| (node.0, node.1))
            .map(|(index, _)| index)
            .collect();
        assert_eq!(all, [0, 2, 3, 4]);
        assert_eq!(
            scoped_component_nodes(&nodes, Some(2), |node| (node.0, node.1)).count(),
            0
        );
    }

    #[test]
    fn compositor_origin_wins_over_stale_accessibility_observation() {
        assert_eq!(
            prefer_authoritative_wayland_origin(Some((0, 0)), Some((120, 120))),
            Some((0, 0))
        );
        assert_eq!(
            prefer_authoritative_wayland_origin(None, Some((120, 120))),
            Some((120, 120))
        );
    }

    #[test]
    fn an_attested_shell_without_the_exact_frame_admits_no_pid_observation() {
        let observed = || Some((120, 120));
        assert_eq!(
            wayland_window_origin(Some((600, 0)), observed, || true),
            Some((600, 0))
        );
        assert_eq!(wayland_window_origin(None, observed, || true), None);
        // Without a Shell helper (Sway, KDE, GNOME without it) the
        // observation stays the last fallback.
        assert_eq!(
            wayland_window_origin(None, observed, || false),
            Some((120, 120))
        );
        assert_eq!(
            wayland_window_origin(Some((0, 0)), observed, || false),
            Some((0, 0))
        );
    }

    #[test]
    fn wayland_compositor_and_document_offsets_are_additive() {
        assert_eq!(
            combine_wayland_content_offsets(Some((0, 47)), Some((0, 0)), true),
            Some((0, 47))
        );
        assert_eq!(
            combine_wayland_content_offsets(Some((2, 20)), Some((0, 47)), true),
            Some((2, 67))
        );
        assert_eq!(
            combine_wayland_content_offsets(None, Some((0, 47)), true),
            Some((0, 47))
        );
        assert_eq!(combine_wayland_content_offsets(None, None, true), None);
    }

    #[test]
    fn hyprland_document_selection_requires_its_own_web_process_bus() {
        let mixed_bus_documents = [("application", 1, false), ("web-process", 3, true)];
        assert_eq!(
            select_web_document(mixed_bus_documents.into_iter(), true),
            Some("web-process")
        );
        // Sway retains its existing shallowest-document selection.
        assert_eq!(
            select_web_document(mixed_bus_documents.into_iter(), false),
            Some("application")
        );
        assert_eq!(
            select_web_document([("application", 1, false)].into_iter(), true),
            None
        );
    }

    #[test]
    fn hyprland_document_infers_csd_without_double_counting() {
        let target = (0x1234_5678_9abc, 42);
        let window = Some((target.0, target.1, 940, 700));
        for y in [0, 20, 47] {
            assert_eq!(
                hyprland_document_top_inset(target, window, (0, y, 940, 653)),
                Some(47)
            );
        }
        assert_eq!(
            combine_wayland_content_offsets(None, Some((0, 47)), false),
            None
        );
    }

    #[test]
    fn hyprland_document_rejects_invalid_or_partial_dimensions() {
        let target = (0x1234_5678_9abc, 42);
        let window = Some((target.0, target.1, 940, 700));
        for document in [
            (0, 0, 0, 653),
            (0, 0, 940, 0),
            (0, 0, -1, 653),
            (0, 0, 940, -1),
            (0, 0, 935, 653),
            (0, 0, 945, 653),
            (0, 0, 940, 700),
            (0, 0, 940, 701),
            (0, 0, 940, 500),
            (1, 0, 940, 653),
            (0, -1, 940, 653),
        ] {
            assert_eq!(hyprland_document_top_inset(target, window, document), None);
        }
    }

    #[test]
    fn hyprland_document_requires_full_address_and_pid() {
        let target = (0x1234_5678_9abc, 42);
        let document = (0, 0, 940, 653);
        for window in [
            None,
            Some((0x5678_9abc, 42, 940, 700)),
            Some((0x1234_5678_9abd, 42, 940, 700)),
            Some((target.0, 43, 940, 700)),
        ] {
            assert_eq!(hyprland_document_top_inset(target, window, document), None);
        }
        assert_eq!(
            hyprland_document_top_inset((0, 42), Some((0, 42, 940, 700)), document),
            None
        );
    }

    #[test]
    fn chromium_window_extents_do_not_double_count_document_origin() {
        assert_eq!(
            combine_wayland_content_offsets(Some((2, 20)), Some((22, 55)), false),
            Some((2, 20))
        );
        assert_eq!(
            combine_wayland_content_offsets(None, Some((22, 55)), false),
            None
        );
    }

    #[test]
    fn webkit_web_process_bus_marks_roleless_document_subtrees() {
        assert!(is_web_process_bus("org.webkitgtk.WebProcess.1234"));
        assert!(is_web_process_bus("org.example.WebProcess"));
        assert!(!is_web_process_bus(":1.42"));
    }

    #[test]
    fn click_target_prefers_button_over_its_inner_label() {
        // The exact live GTK4 gnome-calculator case this fixes: button "7"
        // (idx 6, role 'button', 82,331 64x44) wraps a slightly smaller inner
        // label (idx 7, role 'label', 82,331 56x40). A click at the shared
        // center must actuate the BUTTON (idx 6) — area alone would pick the
        // smaller inert label (idx 7) → silent no-op "false success".
        let frames = vec![
            (6usize, 82, 331, 64, 44, false), // button "7"
            (7usize, 82, 331, 56, 40, true),  // inner label "7"
        ];
        assert_eq!(select_click_target(&frames, 114, 353), Some(6));
    }

    #[test]
    fn click_target_smallest_active_over_enclosing_panel() {
        let frames = vec![
            (0usize, 0, 0, 400, 600, false),   // panel
            (3usize, 80, 320, 64, 44, false),  // button "7"
            (5usize, 150, 320, 64, 44, false), // button "8"
        ];
        assert_eq!(select_click_target(&frames, 100, 340), Some(3));
        assert_eq!(select_click_target(&frames, 180, 340), Some(5));
    }

    #[test]
    fn click_target_falls_back_to_label_when_no_actuator_covers() {
        // A lone clickable label (no button covers the point) is still a valid
        // last-resort target — don't drop the click entirely.
        let frames = vec![(9usize, 10, 10, 30, 20, true)];
        assert_eq!(select_click_target(&frames, 20, 15), Some(9));
    }

    #[test]
    fn click_target_edges_exclusive_and_misses_return_none() {
        let frames = vec![(7usize, 10, 10, 20, 20, false)];
        assert_eq!(select_click_target(&frames, 10, 10), Some(7)); // top-left inclusive
        assert_eq!(select_click_target(&frames, 29, 29), Some(7)); // inside
        assert_eq!(select_click_target(&frames, 30, 20), None); // right edge exclusive
        assert_eq!(select_click_target(&frames, 20, 30), None); // bottom edge exclusive
        assert_eq!(select_click_target(&frames, 5, 5), None); // outside
        assert_eq!(select_click_target(&[], 0, 0), None); // no frames
    }

    #[test]
    fn passive_roles_classified() {
        assert!(is_passive_role("label"));
        assert!(is_passive_role("static text"));
        assert!(!is_passive_role("button"));
        assert!(!is_passive_role("push button"));
        assert!(!is_passive_role("text box")); // editable display is a real target
    }

    #[test]
    fn frame_extents_maps_left_and_top_not_right_or_bottom() {
        // _GTK_FRAME_EXTENTS = [left, right, top, bottom]; we need (left, top).
        assert_eq!(parse_gtk_frame_extents(&[61, 61, 55, 67]), Some((61, 55)));
        // Asymmetric values prove we don't accidentally read right([1])/bottom([3]).
        assert_eq!(parse_gtk_frame_extents(&[10, 20, 30, 40]), Some((10, 30)));
        // Maximized GTK4 window: zero inset, but property present.
        assert_eq!(parse_gtk_frame_extents(&[0, 0, 0, 0]), Some((0, 0)));
    }

    #[test]
    fn frame_extents_absent_or_short_is_none() {
        assert_eq!(parse_gtk_frame_extents(&[]), None);
        assert_eq!(parse_gtk_frame_extents(&[61, 61]), None);
        assert_eq!(parse_gtk_frame_extents(&[61, 61, 55]), None);
    }

    #[test]
    fn screen_reconstruction_matches_live_gnome_calculator() {
        // Regression anchor for the whole GTK4 fix, from a live-verified capture:
        // gnome-calculator button "7" = x11_window_origin (55,27)
        //   + _GTK_FRAME_EXTENTS inset (61,55) + atspi WINDOW coords (16,293)
        //   = screen (132,375).
        let (fl, ft) = parse_gtk_frame_extents(&[61, 61, 55, 67]).unwrap();
        let origin = (55, 27); // x11_window_origin
        let window = (16, 293); // atspi CoordType::Window
        let offset = (origin.0 + fl, origin.1 + ft); // window_to_screen_offset
        let screen = (offset.0 + window.0, offset.1 + window.1);
        assert_eq!(screen, (132, 375));
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn application_attributes_wake_tracks_exact_incarnation_and_bus_with_bounded_memory() {
        let key = ApplicationWakeKey {
            pid: 42,
            incarnation: 1,
            bus: "bus-one".into(),
            destination: ":1.2".into(),
            path: "/app".into(),
        };
        let mut wakes = ApplicationWakes::default();
        assert!(!wakes.contains(&key));
        wakes.remember(key.clone());
        assert!(wakes.contains(&key));
        for changed in [
            ApplicationWakeKey {
                incarnation: 2,
                ..key.clone()
            },
            ApplicationWakeKey {
                bus: "bus-two".into(),
                ..key.clone()
            },
            ApplicationWakeKey {
                destination: ":1.3".into(),
                ..key.clone()
            },
            ApplicationWakeKey {
                path: "/other".into(),
                ..key.clone()
            },
        ] {
            assert!(!wakes.contains(&changed));
        }
        for incarnation in 2..=ApplicationWakes::CAPACITY as u64 + 1 {
            wakes.remember(ApplicationWakeKey {
                incarnation,
                ..key.clone()
            });
        }
        assert_eq!(wakes.0.len(), ApplicationWakes::CAPACITY);
        assert!(!wakes.contains(&key));
    }

    #[test]
    fn focus_log_tracks_latest_focused_object_per_bus_and_clears_on_blur() {
        let bus = ":9.test-focus-log";
        let map = Mutex::new(std::collections::HashMap::new());
        lifecycle::note_focus_event(&map, bus, "/a", true);
        assert_eq!(map.lock().unwrap().get(bus).map(String::as_str), Some("/a"));
        lifecycle::note_focus_event(&map, bus, "/b", true);
        assert_eq!(map.lock().unwrap().get(bus).map(String::as_str), Some("/b"));
        // A blur for a stale object must not erase the newer focus.
        lifecycle::note_focus_event(&map, bus, "/a", false);
        assert_eq!(map.lock().unwrap().get(bus).map(String::as_str), Some("/b"));
        lifecycle::note_focus_event(&map, bus, "/b", false);
        assert!(map.lock().unwrap().get(bus).is_none());
    }

    #[test]
    fn per_call_timeout_clamps_to_operation_deadline() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // No deadline in scope: the historical per-call timeout applies.
            assert_eq!(remaining_budget(), CALL_TIMEOUT);
            assert!(!deadline_passed());
            let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
            OP_DEADLINE
                .scope(deadline, async {
                    assert!(remaining_budget() <= Duration::from_millis(200));
                    // A call that would outlive the remaining budget is cut at
                    // the deadline, not at CALL_TIMEOUT.
                    let started = std::time::Instant::now();
                    let got = call(tokio::time::sleep(Duration::from_secs(10))).await;
                    assert!(got.is_none());
                    assert!(started.elapsed() < Duration::from_secs(2));
                    assert!(deadline_passed());
                    // Once the deadline has passed, calls short-circuit.
                    let started = std::time::Instant::now();
                    assert!(call(tokio::time::sleep(Duration::from_secs(10)))
                        .await
                        .is_none());
                    assert!(started.elapsed() < Duration::from_millis(50));
                })
                .await;
        });
    }

    #[test]
    fn bounded_for_returns_partial_via_on_timeout_when_work_ignores_deadline() {
        let started = std::time::Instant::now();
        let result: Result<&'static str> = bounded_for(
            Duration::from_millis(100),
            async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok("finished")
            },
            || Ok("partial"),
        );
        assert_eq!(result.unwrap(), "partial");
        // deadline + 500 ms backstop, well under the old 25 s.
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}

#[cfg(test)]
mod at_point_rules_tests {
    use super::*;

    fn acts(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn container_activation_never_bubbles() {
        // Nautilus: canvas item (no Action) under `layered pane "Icon View"`
        // [activate, menu]: the container is an ancestor -> not fired.
        assert_eq!(
            at_point_activation_index("layered pane", &acts(&["activate", "menu"]), false),
            None
        );
        for role in [
            "list",
            "table",
            "tree",
            "tree table",
            "icon view",
            "panel",
            "list box",
        ] {
            assert_eq!(
                at_point_activation_index(role, &acts(&["activate"]), false),
                None
            );
            // A click on the container's own empty area must not open its
            // current selection either.
            assert_eq!(
                at_point_activation_index(role, &acts(&["activate"]), true),
                None
            );
        }
        // A canvas is a surface: as an ancestor its activate is the
        // container's; as the deepest hit it is the widget itself.
        assert_eq!(
            at_point_activation_index("canvas", &acts(&["activate"]), false),
            None
        );
        assert_eq!(
            at_point_activation_index("canvas", &acts(&["activate"]), true),
            Some(0)
        );
    }

    #[test]
    fn top_level_frame_default_action_is_never_a_pixel_click() {
        // Chrome 151 on X11: the hit test over page content resolves only to
        // `frame "… - Google Chrome"` [doDefault]. Firing it changes nothing,
        // so it must not be reported as the click, as ancestor or deepest hit.
        for role in ["frame", "window", "application", "desktop frame"] {
            for deepest in [false, true] {
                assert_eq!(
                    at_point_activation_index(role, &acts(&["doDefault"]), deepest),
                    None,
                    "{role} deepest={deepest}"
                );
            }
        }
        // The page content itself remains actionable once exposed.
        assert_eq!(
            at_point_activation_index("push button", &acts(&["doDefault"]), true),
            Some(0)
        );
    }

    #[test]
    fn real_actuators_still_fire_from_any_depth() {
        assert_eq!(
            at_point_activation_index("push button", &acts(&["click"]), false),
            Some(0)
        );
        assert_eq!(
            at_point_activation_index("menu item", &acts(&["click"]), true),
            Some(0)
        );
        assert_eq!(
            at_point_activation_index("check box", &acts(&["check"]), true),
            Some(0)
        );
        // Non-activation verbs never fire, container or not.
        assert_eq!(
            at_point_activation_index("push button", &acts(&["buffer.delete-line"]), true),
            None
        );
    }

    #[test]
    fn cell_roles_take_a_real_press() {
        for role in ["table cell", "cell", "Table Cell"] {
            assert!(is_cell_role(role), "{role}");
            assert!(is_selectable_item_role(role), "{role}");
        }
        for role in [
            "canvas",
            "list item",
            "icon",
            "tree item",
            "row",
            "table row",
        ] {
            assert!(!is_cell_role(role), "{role}");
        }
    }

    #[test]
    fn real_press_rule_covers_editable_text_focus_controls_and_cells() {
        for role in [
            "text",
            "entry",
            "password text",
            "spin button",
            "slider",
            "combo box",
            "table cell",
            "cell",
        ] {
            assert!(requires_real_pointer_press(false, role), "{role}");
        }
        // Any node that advertises EditableText, whatever its role.
        assert!(requires_real_pointer_press(true, "document web"));
        for role in [
            "push button",
            "toggle button",
            "check box",
            "menu item",
            "list item",
            "link",
        ] {
            assert!(!requires_real_pointer_press(false, role), "{role}");
        }
    }

    #[test]
    fn point_actions_refuse_nodes_that_take_a_real_press_before_firing() {
        let source = include_str!("native.rs");
        for (start, end) in [
            (
                concat!("pub fn perform_action_", "at_point("),
                concat!("invoke_live_", "activation(&ap"),
            ),
            (
                concat!("pub fn perform_action_", "at_screen_point("),
                concat!("invoke_exact_live_", "activation("),
            ),
        ] {
            let body = &source[source.find(start).expect(start)..];
            let body = &body[..body.find(end).expect(end)];
            let hit = body
                .find(concat!("select_click_", "target(&frames"))
                .expect("hit test");
            let guard = body
                .find(concat!(
                    "requires_real_pointer_",
                    "press(target.has_editable, &target.role)"
                ))
                .unwrap_or_else(|| panic!("{start}: missing real-press guard"));
            assert!(hit < guard, "{start}: the guard must follow the hit test");
            assert!(
                body[guard..].contains(concat!("ElementClick", "NeedsForeground.into()")),
                "{start}: the guard must refuse with the no-input error"
            );
        }
    }

    #[test]
    fn selectable_item_roles() {
        for role in [
            "canvas",
            "list item",
            "table cell",
            "tree item",
            "icon",
            "row",
        ] {
            assert!(is_selectable_item_role(role), "{role}");
        }
        for role in ["push button", "layered pane", "label", "menu item"] {
            assert!(!is_selectable_item_role(role), "{role}");
        }
    }

    #[test]
    fn hit_description_names_what_was_fired() {
        let hit = AtPointHit::fired(
            "activate".into(),
            "push button".into(),
            "Restore".into(),
            "/org/a11y/atspi/accessible/42".into(),
        );
        assert_eq!(
            hit.describe(),
            "hit: push button \"Restore\" action=activate path=/org/a11y/atspi/accessible/42"
        );
        let selected = AtPointHit {
            action: "select_child".into(),
            role: "canvas".into(),
            name: "file.txt".into(),
            path: "/x".into(),
            selected: Some("file.txt".into()),
            selection_verified: true,
        };
        assert!(selected
            .describe()
            .starts_with("selected: canvas \"file.txt\""));
        assert!(selected.describe().contains("read back as selected"));
    }
}

pub fn focused_control(pid: u32) -> Option<(String, String)> {
    bounded_for(
        Duration::from_millis(400),
        async {
            let connection = shared_connection().await?;
            let conn = &connection;
            let Some((acc, focused_ref)) = focused_accessible_via_events(conn, pid).await else {
                return Ok(None);
            };
            let (role, name) = tokio::join!(call(acc.get_role_name()), call(acc.name()));
            let (Some(Ok(role)), Some(Ok(name))) = (role, name) else {
                return Ok(None);
            };
            // LibreOffice keeps the focus on the sheet's `table`; the current
            // cell is its active descendant (event log), else its selected
            // child.
            if role.eq_ignore_ascii_case("table") {
                let active = active_descendant_map()
                    .lock()
                    .unwrap()
                    .get(&focused_ref.bus)
                    .cloned();
                if let Some(path) = active {
                    let raw = RawObjectRef {
                        name: focused_ref.bus.clone(),
                        path,
                    };
                    if let Some(Ok(cell)) = call(accessible_for(conn, &raw)).await {
                        let (cell_role, cell_name) =
                            tokio::join!(call(cell.get_role_name()), call(cell.name()));
                        if let (Some(Ok(cell_role)), Some(Ok(cell_name))) = (cell_role, cell_name) {
                            if !cell_name.trim().is_empty() {
                                return Ok(Some((cell_role, cell_name)));
                            }
                        }
                    }
                }
                if let Some(Ok(proxies)) = call(acc.proxies()).await {
                    if let Some(Ok(selection)) = call(proxies.selection()).await {
                        if let Some(Ok(n)) = call(selection.n_selected_children()).await {
                            if n > 0 {
                                if let Some(Ok(child)) = call(selection.get_selected_child(0)).await
                                {
                                    if let Some(raw) = RawObjectRef::from_atspi(&child) {
                                        if let Some(Ok(cell)) =
                                            call(accessible_for(conn, &raw)).await
                                        {
                                            let (cell_role, cell_name) = tokio::join!(
                                                call(cell.get_role_name()),
                                                call(cell.name())
                                            );
                                            if let (Some(Ok(cell_role)), Some(Ok(cell_name))) =
                                                (cell_role, cell_name)
                                            {
                                                return Ok(Some((cell_role, cell_name)));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(Some((role, name)))
        },
        || Ok(None),
    )
    .ok()
    .flatten()
}

async fn element_bounds_for_visited(
    visited: &[Visited<'_>],
    pid: u32,
    xid: u64,
    only_frame: Option<usize>,
) -> Vec<(usize, i32, i32, u32, u32)> {
    element_bounds_for_visited_with_completeness(visited, pid, xid, only_frame)
        .await
        .0
}

pub(crate) fn walk_bounds_usable(
    bounds: Option<(i32, i32, u32, u32)>,
    display: Option<(u32, u32)>,
) -> Option<(i32, i32, u32, u32)> {
    let (x, y, w, h) = bounds?;
    if w == 0 || h == 0 {
        return None;
    }
    if let Some((dw, dh)) = display {
        let right = i64::from(x) + i64::from(w);
        let bottom = i64::from(y) + i64::from(h);
        if x >= dw as i32 || y >= dh as i32 || right <= 0 || bottom <= 0 {
            return None;
        }
    }
    Some((x, y, w, h))
}

// --- Upstream retained-object dispatch ----------------------------------------
// These address the D-Bus object a snapshot observed. They do not consult
// the PID-only side index and they do not inherit AT_SPI_BUS_ADDRESS.
// Blocking work stays on cua_driver_core::blocking::spawn via bounded_for.

fn require_exact_origin(offset: Option<(i32, i32)>, xid: u64, required: bool) -> Result<()> {
    if offset.is_none() && required {
        return Err(anyhow!(
            "window {xid} has no exact frame from the GNOME Shell helper"
        ));
    }
    Ok(())
}

const REF_ACTION_BUDGET: Duration = Duration::from_secs(3);

fn raw_ref(object_ref: &ObjectRef) -> RawObjectRef {
    RawObjectRef {
        name: object_ref.bus.clone(),
        path: object_ref.path.clone(),
    }
}

/// Current action names of an Action proxy, index-aligned with `do_action`.
async fn action_names(ap: &atspi::proxy::action::ActionProxy<'_>) -> Vec<String> {
    let n = call(ap.n_actions()).await.and_then(|r| r.ok()).unwrap_or(0);
    let mut names = Vec::with_capacity(n.max(0) as usize);
    for i in 0..n {
        names.push(
            call(ap.get_name(i))
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default(),
        );
    }
    names
}

/// The snapshot-cached object no longer answers on the bus (or never did):
/// the observation is stale and must not be retargeted by index.
#[derive(Debug)]
pub struct CachedElementGone(pub String);
impl std::fmt::Display for CachedElementGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for CachedElementGone {}

/// Open a cached accessible and prove it is still alive (one role read).
async fn live_accessible<'a>(
    conn: &'a AccessibilityConnection,
    object_ref: &ObjectRef,
) -> Result<(AccessibleProxy<'a>, String)> {
    let acc = match call(accessible_for(conn, &raw_ref(object_ref))).await {
        Some(Ok(acc)) => acc,
        Some(Err(error)) => {
            return Err(CachedElementGone(format!("cached element unreachable: {error}")).into())
        }
        None => return Err(anyhow!("cached element did not answer in time")),
    };
    let role = match call(acc.get_role_name()).await {
        Some(Ok(role)) => role,
        Some(Err(error)) => {
            return Err(CachedElementGone(format!("cached element is gone: {error}")).into())
        }
        None => return Err(anyhow!("cached element did not answer in time")),
    };
    Ok((acc, role))
}

/// [`perform_action`] on a snapshot-cached element identity. Same contract:
/// `Ok((action_name, suspected_noop))`. Errors when the object no longer
/// exists so the caller can fall back to resolving the index afresh.
pub fn perform_action_ref(object_ref: &ObjectRef) -> Result<(String, bool, bool)> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, role) = live_accessible(&conn, object_ref).await?;
            let proxies = acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let ap = proxies
                .action()
                .await
                .map_err(|e| anyhow!("Action unavailable: {e}"))?;
            let actions = action_names(&ap).await;
            let suspected_noop = actions.is_empty() || is_passive_role(&role);
            let chosen = activation_index(&role, &actions)
                .ok_or_else(|| anyhow!("element does not advertise a safe activation action"))?;
            // See `perform_action`'s `unacknowledged`: an unanswered doAction
            // is unknown, not dispatched-implies-success, and must not be
            // folded into `rejected`/`suspected_noop` (which would claim the
            // toolkit declined it) nor silently reported as a plain success.
            let mut unacknowledged = false;
            let rejected = match call(ap.do_action(chosen as i32)).await {
                Some(Ok(accepted)) => !accepted,
                Some(Err(e)) => return Err(anyhow!("doAction failed: {e}")),
                // The request was delivered; a GTK item whose action opens a
                // dialog (nested main loop) only replies once that dialog
                // closes. Treat the timeout as dispatched-but-unconfirmed:
                // falling back to another route would fire the item twice.
                None => {
                    dlog!("doAction dispatched but not acknowledged in time");
                    unacknowledged = true;
                    false
                }
            };
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok((
                actions.get(chosen).cloned().unwrap_or_default(),
                suspected_noop || rejected,
                unacknowledged,
            ))
        },
        || Err(anyhow!("perform_action (cached element) timed out")),
    )
}

/// Screen bounds of a snapshot-cached element identity: one `GetExtents`
/// round-trip, no tree re-walk. `Err` when the object is gone or does not
/// answer within [`REF_ACTION_BUDGET`]; the caller decides whether to re-walk.
/// The X11 origin lookup runs on the caller's (blocking) thread before the
/// runtime is entered, so no synchronous X11 I/O happens under `block_on`.
pub fn element_bounds_ref(
    object_ref: &ObjectRef,
    pid: u32,
    xid: u64,
    in_web_content: bool,
) -> Result<(i32, i32, u32, u32)> {
    if in_web_content {
        return Err(anyhow!(
            "web-content bounds need the document origin; re-walk"
        ));
    }
    let offset = window_to_screen_offset(pid, xid, None);
    require_exact_origin(offset, xid, exact_wayland_origin_required(xid))?;
    let coord = if offset.is_some() {
        CoordType::Window
    } else {
        CoordType::Screen
    };
    let display = (!crate::wayland::is_wayland())
        .then(x11_display_size)
        .flatten();
    let origin_attested = native_wayland_origin_attested(pid, xid);
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(&conn, object_ref).await?;
            let proxies = match call(acc.proxies()).await {
                Some(Ok(proxies)) => proxies,
                Some(Err(e)) => return Err(anyhow!("interface proxies unavailable: {e}")),
                None => return Err(anyhow!("cached element did not answer in time")),
            };
            let component = match call(proxies.component()).await {
                Some(Ok(component)) => component,
                Some(Err(e)) => return Err(anyhow!("Component unavailable: {e}")),
                None => return Err(anyhow!("cached element did not answer in time")),
            };
            if coord == CoordType::Window {
                if let Some(Ok(raw)) = call(component.get_extents(CoordType::Screen)).await {
                    if screen_answer_overrides_window(raw, display, origin_attested) {
                        return project_screen_extents(raw, (0, 0), None)
                            .ok_or_else(|| anyhow!("cached element reports no on-screen extents"));
                    }
                }
            }
            let extents = match call(component.get_extents(coord)).await {
                Some(Ok(extents)) => extents,
                Some(Err(e)) => return Err(anyhow!("getExtents failed: {e}")),
                None => return Err(anyhow!("getExtents did not answer in time")),
            };
            project_screen_extents(extents, offset.unwrap_or((0, 0)), None)
                .ok_or_else(|| anyhow!("cached element reports no on-screen extents"))
        },
        || Err(anyhow!("element_bounds (cached element) timed out")),
    )
}

/// Whether a snapshot-cached element is currently on screen: its own AT-SPI
/// state set carries `Showing` (a GTK menu item whose menu is closed, or a
/// widget on an unmapped notebook page, lacks it even though the object
/// still exists). `Ok(true)` when the toolkit exposes no state set.
pub fn element_showing_ref(object_ref: &ObjectRef) -> Result<bool> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(&conn, object_ref).await?;
            match call(acc.get_state()).await {
                Some(Ok(state)) => Ok(is_showing_state(&state)),
                Some(Err(_)) => Ok(true),
                None => Err(anyhow!("cached element did not answer in time")),
            }
        },
        || Err(anyhow!("element_showing (cached element) timed out")),
    )
}

/// [`focus_element`] on a snapshot-cached element identity.
pub fn focus_element_ref(object_ref: &ObjectRef) -> Result<bool> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(&conn, object_ref).await?;
            let proxies = acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let component = proxies
                .component()
                .await
                .map_err(|e| anyhow!("Component interface unavailable: {e}"))?;
            let accepted = match call(component.grab_focus()).await {
                Some(Ok(focused)) => focused,
                Some(Err(e)) => return Err(anyhow!("Component.GrabFocus failed: {e}")),
                None => return Err(anyhow!("Component.GrabFocus timed out")),
            };
            if !accepted {
                return Ok(false);
            }
            let settle_deadline = tokio::time::Instant::now() + Duration::from_millis(500);
            while tokio::time::Instant::now() < settle_deadline {
                match tokio::time::timeout(Duration::from_millis(100), acc.get_state()).await {
                    Ok(Ok(state)) if state.contains(State::Focused) => return Ok(true),
                    _ => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
            Ok(false)
        },
        || Err(anyhow!("focus_element (cached element) timed out")),
    )
}

/// Focus-free EditableText write for a retained object reference. GrabFocus is
/// only the fallback after a direct write, matching the observed route, and
/// never carries a second mutation permit.
async fn write_into_editable_acc(
    acc: &AccessibleProxy<'_>,
    has_component: bool,
    text: &str,
) -> Result<bool> {
    let proxies = acc
        .proxies()
        .await
        .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
    if write_through_editable_proxies(&proxies, text, None).await? {
        return Ok(true);
    }
    if has_component {
        if let Ok(comp) = proxies.component().await {
            match call(comp.grab_focus()).await {
                Some(Ok(true)) => dlog!("GrabFocus succeeded"),
                Some(Ok(false)) => dlog!("GrabFocus returned false"),
                Some(Err(e)) => dlog!("GrabFocus failed: {e}"),
                None => dlog!("GrabFocus timed out"),
            }
        }
    }
    write_through_editable_proxies(&proxies, text, None).await
}

/// [`type_into_editable_at`] on a snapshot-cached element identity.
pub fn type_into_editable_ref(object_ref: &ObjectRef, text: &str) -> Result<()> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(&conn, object_ref).await?;
            let ifaces = match call(acc.get_interfaces()).await {
                Some(Ok(ifaces)) => ifaces,
                _ => return Err(anyhow!("cached element interfaces unavailable")),
            };
            if !ifaces.contains(Interface::EditableText) {
                return Err(anyhow!("cached element is not editable"));
            }
            if write_into_editable_acc(&acc, ifaces.contains(Interface::Component), text).await? {
                Ok(())
            } else {
                Err(anyhow!("EditableText write was rejected"))
            }
        },
        || Err(anyhow!("type_into_editable (cached element) timed out")),
    )
}

/// Invoke the original target's semantic actions, pacing multi-page requests
/// with descendant motion. `Err` means no mutation was attempted; an incomplete
/// `ScrollProgress` preserves uncertainty after an attempted mutation. An
/// identity the fresh walk no longer has is [`CachedElementGone`].
pub fn scroll_element(
    pid: u32,
    element: &WindowElement,
    direction: &str,
    amount: usize,
    by: cua_driver_contract::ScrollBy,
) -> Result<ScrollProgress> {
    let attempted = std::cell::Cell::new(false);
    let acknowledged = std::cell::Cell::new(0);
    let result = bounded(
        async {
            let conn = shared_connection().await?;
            let visited = collect_visited(&conn, pid)
                .await?
                .ok_or_else(|| anyhow!("no AT-SPI application for pid {pid}"))?;
            let target = element.find(&visited).ok_or_else(|| match element {
                WindowElement::Index(idx) => {
                    anyhow!("element {idx} not found (total: {})", visited.len())
                }
                WindowElement::Identity(_) => CachedElementGone(
                    "the observed element is no longer in the application's accessibility tree"
                        .into(),
                )
                .into(),
            })?;
            let proxies = target
                .acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let wanted = match direction {
                "up" => ["scrollup", "scrollbackward"],
                "left" => ["scrollleft", "scrollbackward"],
                "right" => ["scrollright", "scrollforward"],
                _ => ["scrolldown", "scrollforward"],
            };
            let mut selected = None;
            let mut action_proxy = None;
            if let Ok(action) = proxies.action().await {
                let count = call(action.n_actions())
                    .await
                    .and_then(|result| result.ok())
                    .unwrap_or(0);
                for action_index in 0..count {
                    if let Some(Ok(name)) = call(action.get_name(action_index)).await {
                        let normalized: String = name
                            .chars()
                            .filter(|ch| ch.is_ascii_alphanumeric())
                            .flat_map(|ch| ch.to_lowercase())
                            .collect();
                        if wanted.iter().any(|candidate| *candidate == normalized) {
                            selected = Some(action_index);
                            break;
                        }
                    }
                }
                action_proxy = Some(action);
            }

            if let (Some(action), Some(action_index)) = (action_proxy, selected) {
                let mut observation = None;
                if by == cua_driver_contract::ScrollBy::Page && amount > 1 {
                    let component = proxies.component().await?;
                    let target_index = visited
                        .iter()
                        .position(|node| std::ptr::eq(node, target))
                        .unwrap();
                    let mut probes = Vec::new();
                    // Fix probe identities before dispatch; never replace vanished descendants.
                    for index in
                        descendant_indices(visited.iter().map(|node| node.depth), target_index)
                    {
                        if !visited[index].has_component {
                            continue;
                        }
                        if let Some(Ok(proxies)) = call(visited[index].acc.proxies()).await {
                            if let Some(Ok(probe)) = call(proxies.component()).await {
                                probes.push(probe);
                            }
                        }
                    }
                    for coord in [CoordType::Window, CoordType::Screen] {
                        let mut usable = Vec::new();
                        for probe in &probes {
                            if sample_page_probes(&component, std::slice::from_ref(probe), coord)
                                .await
                                .is_ok()
                            {
                                usable.push(probe.clone());
                                if usable.len() == 8 {
                                    break;
                                }
                            }
                        }
                        if !usable.is_empty() {
                            let baseline = sample_page_probes(&component, &usable, coord).await?;
                            observation = Some((component.clone(), usable, coord, baseline));
                            break;
                        }
                    }
                    if observation.is_none() {
                        return Err(anyhow!("no usable descendant scroll probes"));
                    }
                }
                scroll_sequence(
                    amount,
                    observation.as_ref().map(|o| o.3.clone()),
                    &attempted,
                    &acknowledged,
                    || async { action.do_action(action_index).await.map_err(Into::into) },
                    |baseline| async {
                        let (component, probes, coord, _) = observation
                            .as_ref()
                            .expect("page probes selected before dispatch");
                        let mut motion = PageMotion::new(baseline);
                        let started = tokio::time::Instant::now();
                        loop {
                            let sample = sample_page_probes(component, probes, *coord).await?;
                            let settled = motion.observe(&sample, direction);
                            dlog!(
                                "scroll sequence sample elapsed_ms={} positions={sample:?} settled={settled}",
                                started.elapsed().as_millis()
                            );
                            if settled {
                                return Ok(sample);
                            }
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    },
                )
                .await?;
                return Ok(());
            }

            if target.has_value {
                let value = proxies
                    .value()
                    .await
                    .map_err(|e| anyhow!("Value interface unavailable: {e}"))?;
                let current = call(value.current_value())
                    .await
                    .and_then(|result| result.ok())
                    .ok_or_else(|| anyhow!("scroll value lookup timed out"))?;
                let minimum = call(value.minimum_value())
                    .await
                    .and_then(|result| result.ok())
                    .unwrap_or(current);
                let maximum = call(value.maximum_value())
                    .await
                    .and_then(|result| result.ok())
                    .unwrap_or(current);
                let increment = call(value.minimum_increment())
                    .await
                    .and_then(|result| result.ok())
                    .filter(|increment| *increment > 0.0)
                    .unwrap_or(1.0);
                let sign = if matches!(direction, "up" | "left") {
                    -1.0
                } else {
                    1.0
                };
                let next =
                    (current + sign * increment * amount.max(1) as f64).clamp(minimum, maximum);
                attempted.set(true);
                call(value.set_current_value(next))
                    .await
                    .and_then(|result| result.ok())
                    .ok_or_else(|| anyhow!("scroll value update timed out"))?;
                acknowledged.set(1);
                return Ok(());
            }

            Err(anyhow!(
                "the element exposes neither directional scroll actions nor Value"
            ))
        },
        || Err(anyhow!("scroll_element timed out for pid {pid}")),
    );
    finish_scroll(result, attempted.get(), acknowledged.get())
}

/// Give an indexed element keyboard focus through AT-SPI Component.GrabFocus
/// without activating or raising its toplevel window.
///
/// `GrabFocus` acknowledges the request before Chromium/Electron necessarily
/// updates its renderer-owned focused control. Sending key events immediately
/// after the acknowledgement can therefore split one string between the old
/// and new controls. Wait for the target's Focused state to become observable;
/// an acknowledgement without read-back is not sufficient for global input.

/// Error-message prefix when an element offers neither `EditableText` nor
/// `Value`: the tool layer branches to the keyboard fallback on it.
pub const NO_VALUE_ROUTE: &str = "no_value_route";

pub fn is_no_value_route(error: &anyhow::Error) -> bool {
    error.to_string().starts_with(NO_VALUE_ROUTE)
}

/// A spin button / spin scale only parses its entry text on activate (Return
/// or focus-out); after an EditableText write, fire the element's `activate`
/// action when it exposes one so the numeric value commits. Best effort.
async fn commit_editable_write(proxies: &atspi::proxy::proxy_ext::Proxies<'_>) {
    let Some(Ok(ap)) = call(proxies.action()).await else {
        return;
    };
    let names = action_names(&ap).await;
    if let Some(index) = names
        .iter()
        .position(|name| normalized_action_verb(name) == "activate")
    {
        let _ = call(ap.do_action(index as i32)).await;
    }
}

/// The focus-free write shared by the index and cached-ref entry points.
/// SetValue never calls Component.GrabFocus: GTK may activate and raise the
/// toplevel in response, violating the background contract. Toolkits that
/// expose EditableText only while focused get an honest `NO_VALUE_ROUTE`.
async fn set_value_on(
    acc: &AccessibleProxy<'_>,
    has_value: bool,
    value: &str,
    label: &str,
) -> Result<()> {
    let proxies = acc
        .proxies()
        .await
        .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
    if let Ok(et) = proxies.editable_text().await {
        // Replace whole contents (parity with the Windows/macOS set_value,
        // which overwrite rather than insert at the caret).
        if et.set_text_contents(value).await.unwrap_or(false) {
            commit_editable_write(&proxies).await;
            return Ok(());
        }
        // Some toolkits reject SetTextContents but accept an insert at the
        // caret offset; clear-then-insert as a fallback.
        let off = match proxies.text().await {
            Ok(tp) => tp.caret_offset().await.unwrap_or(0),
            Err(_) => 0,
        };
        let len = value.chars().count() as i32;
        if et.insert_text(off, value, len).await.unwrap_or(false) {
            commit_editable_write(&proxies).await;
            return Ok(());
        }
    }
    if has_value {
        let v: f64 = value
            .parse()
            .map_err(|_| anyhow!("value '{value}' is not numeric for a Value element"))?;
        proxies
            .value()
            .await
            .map_err(|e| anyhow!("Value unavailable: {e}"))?
            .set_current_value(v)
            .await
            .map_err(|e| anyhow!("setCurrentValue failed: {e}"))?;
        return Ok(());
    }
    Err(anyhow!(
        "{NO_VALUE_ROUTE}: {label} exposes neither EditableText nor Value"
    ))
}

/// [`set_value`] on a snapshot-cached element identity (the exact object the
/// caller's `element_index` named, not the same ordinal in a fresh walk).
pub fn set_value_ref(object_ref: &ObjectRef, value: &str) -> Result<()> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(&conn, object_ref).await?;
            let ifaces = match call(acc.get_interfaces()).await {
                Some(Ok(ifaces)) => ifaces,
                _ => return Err(anyhow!("cached element interfaces unavailable")),
            };
            set_value_on(
                &acc,
                ifaces.contains(Interface::Value),
                value,
                "cached element",
            )
            .await
        },
        || Err(anyhow!("set_value (cached element) timed out")),
    )
}

/// Error prefix: a selection container (combo box, list) has no option
/// with the requested text.
pub const NO_SUCH_OPTION: &str = "no_such_option";

pub fn is_no_such_option(error: &anyhow::Error) -> bool {
    error.to_string().starts_with(NO_SUCH_OPTION)
}

/// Options scanned under one combo box or list.
const OPTION_SCAN_CAP: usize = 200;

fn is_option_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "menu item" | "check menu item" | "radio menu item" | "list item" | "option"
    )
}

/// A combo box's options live in a child menu (GTK) or list (Qt).
fn is_option_container_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "menu" | "popup menu" | "list" | "list box" | "window"
    )
}

/// Index of the option `value` names: an exact match first, then one that
/// differs only in case or surrounding space.
pub fn match_option(names: &[String], value: &str) -> Option<usize> {
    names.iter().position(|name| name == value).or_else(|| {
        let wanted = value.trim().to_lowercase();
        names
            .iter()
            .position(|name| name.trim().to_lowercase() == wanted)
    })
}

/// Outcome of picking an option of a selection container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionPick {
    /// The option was selected or activated.
    Picked { name: String },
    /// No option has that text; nothing was changed.
    NoMatch { options: Vec<String> },
    /// The element lists no options (or cannot select them).
    Unavailable,
}

struct OptionNode {
    oref: RawObjectRef,
    name: String,
    /// Position among its container's children: the model row of a GTK
    /// combo box, which its `Selection.SelectChild` takes.
    index: i32,
}

/// The options under `oref`: item children, or the items of its popup menu
/// or list child (one level down), in order.
async fn list_options(conn: &AccessibilityConnection, oref: &RawObjectRef) -> Vec<OptionNode> {
    let zconn = conn.connection();
    let mut options = Vec::new();
    let Some(Ok(children)) = call(raw_children(zconn, oref)).await else {
        return options;
    };
    for (position, child) in children.into_iter().enumerate() {
        if options.len() >= OPTION_SCAN_CAP {
            break;
        }
        let Some(Ok(acc)) = call(accessible_for(conn, &child)).await else {
            continue;
        };
        let role = call(acc.get_role_name())
            .await
            .and_then(|r| r.ok())
            .unwrap_or_default();
        if is_option_role(&role) {
            let name = call(acc.name())
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default();
            options.push(OptionNode {
                oref: child,
                name,
                index: position as i32,
            });
            continue;
        }
        if !is_option_container_role(&role) {
            continue;
        }
        let Some(Ok(items)) = call(raw_children(zconn, &child)).await else {
            continue;
        };
        for (index, item) in items.into_iter().enumerate() {
            if options.len() >= OPTION_SCAN_CAP {
                break;
            }
            let Some(Ok(item_acc)) = call(accessible_for(conn, &item)).await else {
                continue;
            };
            let item_role = call(item_acc.get_role_name())
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default();
            if !is_option_role(&item_role) {
                continue;
            }
            let name = call(item_acc.name())
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default();
            options.push(OptionNode {
                oref: item,
                name,
                index: index as i32,
            });
        }
    }
    options
}

/// Pick the option `value` names on a snapshot-cached selection container
/// (a GTK combo box) through its own `Selection.SelectChild`: the toolkit
/// sets the active row and emits its `changed` signal, focus-free and
/// without opening the popup.
pub fn select_option_ref(object_ref: &ObjectRef, value: &str) -> Result<OptionPick> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(&conn, object_ref).await?;
            let ifaces = match call(acc.get_interfaces()).await {
                Some(Ok(ifaces)) => ifaces,
                _ => return Ok(OptionPick::Unavailable),
            };
            if !ifaces.contains(Interface::Selection) {
                return Ok(OptionPick::Unavailable);
            }
            let options = list_options(&conn, &raw_ref(object_ref)).await;
            if options.is_empty() {
                return Ok(OptionPick::Unavailable);
            }
            let names: Vec<String> = options.iter().map(|o| o.name.clone()).collect();
            let Some(found) = match_option(&names, value) else {
                return Ok(OptionPick::NoMatch { options: names });
            };
            let option = &options[found];
            let proxies = acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let selection = proxies
                .selection()
                .await
                .map_err(|e| anyhow!("Selection unavailable: {e}"))?;
            match call(selection.select_child(option.index)).await {
                Some(Ok(true)) => Ok(OptionPick::Picked {
                    name: option.name.clone(),
                }),
                Some(Ok(false)) => Err(anyhow!(
                    "Selection.SelectChild({}) was refused for option {:?}",
                    option.index,
                    option.name
                )),
                Some(Err(e)) => Err(anyhow!("Selection.SelectChild failed: {e}")),
                None => Err(anyhow!("Selection.SelectChild did not answer in time")),
            }
        },
        || Err(anyhow!("select option (cached element) timed out")),
    )
}

/// Activate the option `value` names in the open popup of a snapshot-cached
/// combo box (its items exist and act only while it is open): the option's
/// own click/activate action selects it and closes the popup.
pub fn activate_option_ref(object_ref: &ObjectRef, value: &str) -> Result<OptionPick> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            live_accessible(&conn, object_ref).await?;
            let options = list_options(&conn, &raw_ref(object_ref)).await;
            if options.is_empty() {
                return Ok(OptionPick::Unavailable);
            }
            let names: Vec<String> = options.iter().map(|o| o.name.clone()).collect();
            let Some(found) = match_option(&names, value) else {
                return Ok(OptionPick::NoMatch { options: names });
            };
            let option = &options[found];
            let acc = match call(accessible_for(&conn, &option.oref)).await {
                Some(Ok(acc)) => acc,
                _ => return Err(anyhow!("option {:?} is gone", option.name)),
            };
            let role = call(acc.get_role_name())
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let proxies = acc
                .proxies()
                .await
                .map_err(|e| anyhow!("interface proxies unavailable: {e}"))?;
            let ap = proxies
                .action()
                .await
                .map_err(|e| anyhow!("option {:?} has no Action: {e}", option.name))?;
            let actions = action_names(&ap).await;
            let chosen = activation_index(&role, &actions).ok_or_else(|| {
                anyhow!("option {:?} advertises no activation action", option.name)
            })?;
            match call(ap.do_action(chosen as i32)).await {
                Some(Ok(true)) => Ok(OptionPick::Picked {
                    name: option.name.clone(),
                }),
                Some(Ok(false)) => Err(anyhow!("option {:?} refused its action", option.name)),
                Some(Err(e)) => Err(anyhow!("option action failed: {e}")),
                None => Err(anyhow!("option action did not answer in time")),
            }
        },
        || Err(anyhow!("activate option (cached element) timed out")),
    )
}

/// Whether a snapshot-cached combo box's popup is open: any of its options
/// is SHOWING.
pub fn options_showing_ref(object_ref: &ObjectRef) -> bool {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            for option in list_options(&conn, &raw_ref(object_ref)).await {
                if let Some(Ok(acc)) = call(accessible_for(&conn, &option.oref)).await {
                    if let Some(Ok(state)) = call(acc.get_state()).await {
                        if is_showing_state(&state) {
                            return Ok(true);
                        }
                    }
                }
            }
            Ok(false)
        },
        || Ok(false),
    )
    .unwrap_or(false)
}

/// Name of the selected child of a `Selection` element (a combo box's
/// active option), for the `set_value` read-back.
async fn selected_child_name(
    conn: &AccessibilityConnection,
    proxies: &atspi::proxy::proxy_ext::Proxies<'_>,
) -> Option<String> {
    let selection = call(proxies.selection()).await?.ok()?;
    if call(selection.n_selected_children()).await?.ok()? < 1 {
        return None;
    }
    let child = call(selection.get_selected_child(0)).await?.ok()?;
    let raw = RawObjectRef::from_atspi(&child)?;
    let acc = call(accessible_for(conn, &raw)).await?.ok()?;
    call(acc.name()).await?.ok()
}

/// Current value of a cached element for read-back after a write: the
/// `Value` interface's number, else the `Text` content. `None` when the
/// element exposes neither (or does not answer in time).
pub fn read_value_ref(object_ref: &ObjectRef) -> Result<Option<String>> {
    bounded_for(
        REF_ACTION_BUDGET,
        async {
            let conn = shared_connection().await?;
            let (acc, _) = live_accessible(&conn, object_ref).await?;
            let ifaces = match call(acc.get_interfaces()).await {
                Some(Ok(ifaces)) => ifaces,
                _ => return Ok(None),
            };
            let proxies = match call(acc.proxies()).await {
                Some(Ok(proxies)) => proxies,
                _ => return Ok(None),
            };
            if ifaces.contains(Interface::Value) {
                if let Some(Ok(vp)) = call(proxies.value()).await {
                    if let Some(Ok(v)) = call(vp.current_value()).await {
                        return Ok(Some(format_value(v)));
                    }
                }
            }
            if ifaces.contains(Interface::Text) {
                if let Some(Ok(tp)) = call(proxies.text()).await {
                    let count = call(tp.character_count())
                        .await
                        .and_then(|r| r.ok())
                        .unwrap_or(0);
                    if let Some(Ok(t)) = call(tp.get_text(0, count.min(4096))).await {
                        return Ok(Some(t));
                    }
                }
            }
            if ifaces.contains(Interface::Selection) {
                return Ok(selected_child_name(&conn, &proxies).await);
            }
            Ok(None)
        },
        || Err(anyhow!("read_value (cached element) timed out")),
    )
}

mod window_scope_tests {
    use super::{
        accessibility_window_id, gnome_window_scope_proven, lone_top_level_renders,
        project_screen_extents, scoped_component_nodes, select_click_target, window_bound_element,
        ELEMENT_OUTSIDE_WINDOW, WINDOW_SCOPE_UNPROVEN,
    };

    /// `(frame_ordinal, has_component, window-local extents)`.
    type Node = (usize, bool, (i32, i32, i32, i32));

    // One PID owns Shell windows 1 (sibling A, AT-SPI frame 0) and 2
    // (requested B, frame 1). Both frames have a button at window-local
    // (10,10); B sits at compositor origin (600,0).
    const PID: u32 = 6079;
    const SIBLING_A: u64 = 1;
    const REQUESTED_B: u64 = 2;
    const NODES: [Node; 2] = [(0, true, (10, 10, 20, 20)), (1, true, (10, 10, 40, 40))];
    const B_ORIGIN: (i32, i32) = (600, 0);

    fn project(
        nodes: &[Node],
        scoped_frame: Option<usize>,
    ) -> Vec<(usize, i32, i32, u32, u32, bool)> {
        scoped_component_nodes(nodes, scoped_frame, |node| (node.0, node.1))
            .filter_map(|(idx, node)| {
                project_screen_extents(node.2, B_ORIGIN, None)
                    .map(|(x, y, w, h)| (idx, x, y, w, h, false))
            })
            .collect()
    }

    /// `perform_action_at_screen_point` on window B in a GNOME session: the
    /// scope gate, then the projection of admitted nodes with B's origin,
    /// then the hit-test.
    fn click_b(
        nodes: &[Node],
        scoped_frame: Option<usize>,
        pid_shell_windows: Option<Vec<u64>>,
        x: i32,
        y: i32,
    ) -> Result<Option<usize>, &'static str> {
        if !gnome_window_scope_proven(
            REQUESTED_B,
            scoped_frame,
            false,
            || true,
            || pid_shell_windows,
        ) {
            return Err(WINDOW_SCOPE_UNPROVEN);
        }
        Ok(select_click_target(&project(nodes, scoped_frame), x, y))
    }

    #[test]
    fn without_the_gate_the_unresolved_walk_selects_the_sibling() {
        assert_eq!(
            select_click_target(&project(&NODES, None), 615, 15),
            Some(0)
        );
    }

    #[test]
    fn unresolved_frame_with_a_same_pid_sibling_refuses() {
        for shell_windows in [
            Some(vec![SIBLING_A, REQUESTED_B]),
            Some(vec![REQUESTED_B, SIBLING_A]),
            Some(vec![REQUESTED_B, REQUESTED_B]),
            Some(vec![SIBLING_A]),
            Some(vec![]),
            None,
        ] {
            assert_eq!(
                click_b(&NODES, None, shell_windows.clone(), 615, 15),
                Err(WINDOW_SCOPE_UNPROVEN),
                "{shell_windows:?}"
            );
        }
    }

    #[test]
    fn proven_scope_selects_the_requested_window() {
        assert_eq!(
            click_b(&NODES, Some(1), Some(vec![SIBLING_A, REQUESTED_B]), 615, 15),
            Ok(Some(1))
        );
    }

    #[test]
    fn a_single_shell_window_keeps_the_unscoped_walk() {
        // One Shell window whose app publishes a second top-level (a popover)
        // that frame correlation cannot place.
        let nodes = [(0, true, (10, 10, 40, 40)), (1, true, (200, 200, 30, 30))];
        let single = Some(vec![REQUESTED_B]);
        assert_eq!(click_b(&nodes, None, single.clone(), 615, 15), Ok(Some(0)));
        assert_eq!(click_b(&nodes, None, single, 815, 215), Ok(Some(1)));
    }

    #[test]
    fn element_bounds_apply_the_same_rule() {
        let siblings = || {
            gnome_window_scope_proven(
                REQUESTED_B,
                None,
                false,
                || true,
                || Some(vec![SIBLING_A, REQUESTED_B]),
            )
        };
        for frame in [0, 1] {
            assert_eq!(
                window_bound_element(frame, None, siblings)
                    .unwrap_err()
                    .to_string(),
                WINDOW_SCOPE_UNPROVEN
            );
        }

        let correlated = || {
            gnome_window_scope_proven(
                REQUESTED_B,
                Some(1),
                false,
                || panic!("a correlated frame needs no session check"),
                || panic!("a correlated frame needs no Shell window list"),
            )
        };
        assert!(window_bound_element(1, Some(1), correlated).is_ok());
        assert_eq!(
            window_bound_element(0, Some(1), correlated)
                .unwrap_err()
                .to_string(),
            ELEMENT_OUTSIDE_WINDOW
        );

        let single = || {
            gnome_window_scope_proven(
                REQUESTED_B,
                None,
                false,
                || true,
                || Some(vec![REQUESTED_B]),
            )
        };
        assert!(window_bound_element(0, None, single).is_ok());

        // No window context keeps application-wide bounds.
        assert!(gnome_window_scope_proven(
            0,
            None,
            false,
            || panic!("no window, no session check"),
            || panic!("no window, no Shell query")
        ));
    }

    #[test]
    fn the_rule_follows_the_attested_shell_not_the_desktop_name() {
        // An attested Shell applies the rule whatever the session reports.
        let siblings = Some(vec![SIBLING_A, REQUESTED_B]);
        assert!(!gnome_window_scope_proven(
            REQUESTED_B,
            None,
            false,
            || false,
            || siblings
        ));
        // GNOME without an attested Shell refuses; compositors without a
        // GNOME Shell keep their own route.
        assert!(!gnome_window_scope_proven(
            REQUESTED_B,
            None,
            false,
            || true,
            || None
        ));
        assert!(gnome_window_scope_proven(
            REQUESTED_B,
            None,
            false,
            || false,
            || None
        ));
    }

    // AT-SPI exports one top-level (frame 0) for the PID. Window B's tree is
    // not exported, so when the Shell also lists sibling A, that top-level is
    // A's; its button at window-local (10,10) sits under B's click (615,15).
    const LONE_TOP_LEVEL: [Node; 1] = [(0, true, (10, 10, 20, 20))];

    /// `resolve_window_frame` for `xid` on a Wayland session outside
    /// Hyprland. Geometry correlation adds nothing: a Shell stable sequence
    /// names no X11 window.
    fn resolve(
        xid: u64,
        top_levels: usize,
        gnome_session: bool,
        pid_shell_windows: Option<Vec<u64>>,
    ) -> Option<usize> {
        (top_levels == 1
            && lone_top_level_renders(PID, xid, || gnome_session, || pid_shell_windows))
        .then_some(0)
    }

    /// `element_bounds_by_walk` for an element of `frame` in B on GNOME.
    fn bounds_b(
        frame: usize,
        scoped_frame: Option<usize>,
        pid_shell_windows: Option<Vec<u64>>,
    ) -> Result<(), String> {
        window_bound_element(frame, scoped_frame, || {
            gnome_window_scope_proven(
                REQUESTED_B,
                scoped_frame,
                false,
                || true,
                || pid_shell_windows,
            )
        })
        .map_err(|error| error.to_string())
    }

    #[test]
    fn without_the_shell_rule_a_lone_top_level_selects_the_sibling() {
        // Before: one top-level was proof on every session.
        let scope = Some(0);
        let siblings = Some(vec![SIBLING_A, REQUESTED_B]);
        assert_eq!(
            click_b(&LONE_TOP_LEVEL, scope, siblings, 615, 15),
            Ok(Some(0))
        );
    }

    #[test]
    fn a_lone_top_level_proves_nothing_while_the_shell_lists_another_window() {
        for shell_windows in [
            vec![SIBLING_A, REQUESTED_B],
            vec![REQUESTED_B, SIBLING_A],
            vec![REQUESTED_B, REQUESTED_B],
            vec![SIBLING_A],
            vec![],
        ] {
            let scope = resolve(REQUESTED_B, 1, true, Some(shell_windows.clone()));
            assert_eq!(scope, None, "{shell_windows:?}");
            assert_eq!(
                click_b(&LONE_TOP_LEVEL, scope, Some(shell_windows.clone()), 615, 15),
                Err(WINDOW_SCOPE_UNPROVEN),
                "{shell_windows:?}"
            );
            assert_eq!(
                bounds_b(0, scope, Some(shell_windows.clone())),
                Err(WINDOW_SCOPE_UNPROVEN.to_owned()),
                "{shell_windows:?}"
            );
            // The attested Shell decides even when the session check says
            // the compositor is not GNOME.
            assert_eq!(
                resolve(REQUESTED_B, 1, false, Some(shell_windows.clone())),
                None,
                "{shell_windows:?}"
            );
        }
        // The AT-SPI handle for frame 0 is no proof while a Shell answers.
        let handle = accessibility_window_id(PID, 0);
        assert_eq!(resolve(handle, 1, true, Some(vec![SIBLING_A])), None);
    }

    #[test]
    fn a_lone_top_level_resolves_when_the_shell_lists_only_the_requested_window() {
        let only_b = Some(vec![REQUESTED_B]);
        let scope = resolve(REQUESTED_B, 1, true, only_b.clone());
        assert_eq!(scope, Some(0));
        assert_eq!(
            click_b(&LONE_TOP_LEVEL, scope, only_b.clone(), 615, 15),
            Ok(Some(0))
        );
        assert_eq!(bounds_b(0, scope, only_b), Ok(()));
    }

    #[test]
    fn without_an_attested_shell_gnome_trusts_only_the_accessibility_handle() {
        // A Shell sequence ID while no attested Shell answers proves nothing.
        let scope = resolve(REQUESTED_B, 1, true, None);
        assert_eq!(scope, None);
        assert_eq!(
            click_b(&LONE_TOP_LEVEL, scope, None, 615, 15),
            Err(WINDOW_SCOPE_UNPROVEN)
        );
        assert_eq!(
            bounds_b(0, scope, None),
            Err(WINDOW_SCOPE_UNPROVEN.to_owned())
        );
        // The AT-SPI window list's own handle for the PID's first top-level
        // names it, so a single-window app keeps working without the helper.
        assert_eq!(
            resolve(accessibility_window_id(PID, 0), 1, true, None),
            Some(0)
        );
        assert_eq!(
            resolve(accessibility_window_id(PID, 1), 1, true, None),
            None
        );
        assert_eq!(
            resolve(accessibility_window_id(PID + 1, 0), 1, true, None),
            None
        );
    }

    #[test]
    fn other_sessions_keep_the_lone_top_level_route() {
        // Sway and KDE have no GNOME session and no Shell helper.
        assert_eq!(resolve(REQUESTED_B, 1, false, None), Some(0));
        // Several top-levels, or none, go to geometry correlation.
        for top_levels in [0, 2, 3] {
            assert_eq!(resolve(REQUESTED_B, top_levels, true, None), None);
        }
    }

    #[test]
    fn accessibility_handles_match_the_window_list() {
        assert_eq!(accessibility_window_id(6079, 0), 6079 << 16);
        assert_eq!(accessibility_window_id(6079, 3), (6079 << 16) | 3);
        assert_eq!(accessibility_window_id(0, 0), 1);
    }
}

#[cfg(test)]
mod token_scope_tests {
    use super::{
        gnome_window_scope_proven, is_window_membership_unproven, lone_top_level_renders,
        snapshot_binds_window, window_element_membership, ELEMENT_OUTSIDE_WINDOW,
        WINDOW_SCOPE_UNPROVEN,
    };

    // One PID owns Shell windows A and B; `get_window_state` reads B.
    const PID: u32 = 6079;
    const SIBLING_A: u64 = 1;
    const REQUESTED_B: u64 = 2;

    /// The binding decision `get_window_state` makes for B on GNOME with an
    /// attested Shell: the walk's frame resolution, then the token rule.
    fn binds_b(shell_windows: &[u64], top_levels: usize) -> bool {
        let shell = || Some(shell_windows.to_vec());
        let window_scoped =
            top_levels == 1 && lone_top_level_renders(PID, REQUESTED_B, || true, shell);
        snapshot_binds_window(false, window_scoped, true, || {
            gnome_window_scope_proven(REQUESTED_B, None, false, || true, shell)
        })
    }

    #[test]
    fn an_unproven_gnome_snapshot_binds_no_tokens() {
        for (shell_windows, top_levels) in [
            (&[SIBLING_A, REQUESTED_B][..], 1),
            (&[REQUESTED_B, SIBLING_A][..], 1),
            (&[SIBLING_A, REQUESTED_B][..], 2),
            (&[SIBLING_A][..], 1),
            (&[][..], 1),
        ] {
            assert!(
                !binds_b(shell_windows, top_levels),
                "{shell_windows:?} with {top_levels} top-level(s)"
            );
        }
    }

    #[test]
    fn a_single_window_snapshot_binds() {
        assert!(binds_b(&[REQUESTED_B], 1));
        // A popover top-level beside the window: the Shell still vouches.
        assert!(binds_b(&[REQUESTED_B], 2));
    }

    #[test]
    fn x11_hyprland_and_other_compositors_bind_as_before() {
        for window_scoped in [false, true] {
            for trusted in [false, true] {
                // X11 and compositors without a GNOME Shell: the uncorrelated
                // walk rule holds, so the previous `true` stands.
                assert!(snapshot_binds_window(false, window_scoped, trusted, || {
                    gnome_window_scope_proven(REQUESTED_B, None, false, || false, || None)
                }));
                // Hyprland keeps its correlated-top-level rule.
                assert_eq!(
                    snapshot_binds_window(true, window_scoped, trusted, || {
                        panic!("Hyprland asks no Shell")
                    }),
                    window_scoped
                );
            }
        }
    }

    #[test]
    fn action_time_membership_uses_the_bounds_rule() {
        let proven = |scoped: Option<usize>, shell: Vec<u64>| {
            move || gnome_window_scope_proven(REQUESTED_B, scoped, false, || true, || Some(shell))
        };
        let gone = window_element_membership(None, Some(0), || true).unwrap_err();
        assert!(is_window_membership_unproven(&gone));
        let sibling =
            window_element_membership(Some(0), None, proven(None, vec![SIBLING_A, REQUESTED_B]))
                .unwrap_err();
        assert!(is_window_membership_unproven(&sibling));
        assert_eq!(sibling.to_string(), WINDOW_SCOPE_UNPROVEN);
        let outside = window_element_membership(
            Some(0),
            Some(1),
            proven(Some(1), vec![SIBLING_A, REQUESTED_B]),
        )
        .unwrap_err();
        assert!(is_window_membership_unproven(&outside));
        assert_eq!(outside.to_string(), ELEMENT_OUTSIDE_WINDOW);
        assert!(window_element_membership(
            Some(1),
            Some(1),
            proven(Some(1), vec![SIBLING_A, REQUESTED_B])
        )
        .is_ok());
        assert!(window_element_membership(Some(0), None, proven(None, vec![REQUESTED_B])).is_ok());
    }
}
