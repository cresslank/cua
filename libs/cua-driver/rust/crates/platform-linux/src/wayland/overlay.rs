//! Native-Wayland agent-cursor overlay via `zwlr_layer_shell_v1`.
//!
//! Replaces the X11-only `overlay.rs` render loop on wlroots compositors
//! (sway, labwc, kwin 5.27+, hyprland) by creating a full-screen,
//! click-through, always-on-top `wl_surface` on every enabled output
//! via `zwlr_layer_shell_v1`. The surface renders the same gradient-arrow
//! cursor as the X11 path by sharing `cursor_overlay::RenderStateCore` —
//! bloom, click-pulse, idle-fade, and motion all work identically.
//!
//! GNOME mutter does not expose `zwlr_layer_shell_v1` — those sessions
//! either fall through to the X11 path (XWayland) or the nested-compositor
//! mode that spawns labwc internally.
//!
//! Architecture mirrors the existing `wayland/persistent_vptr.rs`: one
//! owner thread (`cua-overlay-wl`) holds the wayland Connection +
//! EventQueue + layer surface; commands flow in over a `crossbeam-channel`.
//! The render core wakes at frame cadence only while pixels can change;
//! stable or hidden state performs only a cheap, one-second topology check.

use std::collections::{HashMap, HashSet};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, OnceLock,
};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};
use cursor_overlay::{
    CursorConfig, CursorKey, CursorMap, MsgOutcome, OverlayCommand, OverlayMsg, RenderStateCore,
    ScreenFrame,
};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_output::WlOutput,
        wl_region::WlRegion,
        wl_registry,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

/// Commands the overlay owner thread accepts. The richer commands the
/// cross-platform [`RenderStateCore`] understands (MoveTo, ClickPulse,
/// SetPressed) are forwarded as-is so the layer-shell overlay matches the
/// X11 visual: bloom + animated arrow + click pulse + press ring.
enum WlOverlayCmd {
    Cmd {
        key: CursorKey,
        cmd: OverlayCommand,
    },
    SetDefaultGlideDuration {
        session: Option<CursorKey>,
        value: f64,
    },
    Remove(CursorKey),
    Revive(CursorKey),
    Probe(Sender<anyhow::Result<bool>>),
    #[cfg(test)]
    Shutdown,
}

// Shared by the two native Wayland owners. The registry owns the join handle;
// callers get generation-bound handles, never a borrowed static Sender.
pub(super) const RETIRE_TIMEOUT: Duration = Duration::from_secs(1);

pub(super) struct WorkerControl {
    stopped: AtomicBool,
    stop_tx: Mutex<Option<Sender<()>>>,
    pub(super) stop_rx: Receiver<()>,
    sockets: Mutex<HashMap<usize, (std::os::unix::net::UnixStream, bool)>>,
    next_socket: std::sync::atomic::AtomicUsize,
    available: OnceLock<bool>,
}

impl WorkerControl {
    pub(super) fn new() -> Arc<Self> {
        let (tx, rx) = bounded(0);
        Arc::new(Self {
            stopped: AtomicBool::new(false),
            stop_tx: Mutex::new(Some(tx)),
            stop_rx: rx,
            sockets: Mutex::new(HashMap::new()),
            next_socket: std::sync::atomic::AtomicUsize::new(0),
            available: OnceLock::new(),
        })
    }

    pub(super) fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub(super) fn stop(&self) {
        // Install/protect and stop share this lock: no late socket can escape
        // retirement, and no input can start on a socket already interrupted.
        let sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        self.stopped.store(true, Ordering::Release);
        self.stop_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        for (socket, input_pending) in sockets.values() {
            if !input_pending {
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    pub(super) fn track(self: &Arc<Self>, conn: &Connection) -> anyhow::Result<TrackedSocket> {
        use std::os::fd::AsFd;
        // Connection::as_fd is the public Wayland socket FD. Duplicate it while
        // borrowed and keep the duplicate alive: shutdown never targets a reused
        // integer FD, never closes a foreign handle, and does not take the
        // backend's read/dispatch lock while interrupting a blocked roundtrip.
        let socket = std::os::unix::net::UnixStream::from(conn.as_fd().try_clone_to_owned()?);
        let mut sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(!self.stopped(), "Wayland worker generation retired");
        let id = self.next_socket.fetch_add(1, Ordering::Relaxed);
        sockets.insert(id, (socket, false));
        Ok(TrackedSocket {
            control: self.clone(),
            id,
        })
    }
}

/// Connect through the standard named socket with bounded local admission.
/// WAYLAND_SOCKET is already connected; let wayland-client consume its inherited
/// descriptor via its public API rather than constructing a borrowed raw FD.
pub(super) fn connect_tracked(
    control: &Arc<WorkerControl>,
) -> anyhow::Result<(Connection, TrackedSocket)> {
    anyhow::ensure!(!control.stopped(), "Wayland worker generation retired");
    let conn = if std::env::var_os("WAYLAND_SOCKET").is_some() {
        Connection::connect_to_env()?
    } else {
        let display = std::path::PathBuf::from(
            std::env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into()),
        );
        let path = if display.is_absolute() {
            display
        } else {
            let runtime = std::path::PathBuf::from(
                std::env::var_os("XDG_RUNTIME_DIR")
                    .ok_or_else(|| anyhow::anyhow!("missing XDG_RUNTIME_DIR"))?,
            );
            anyhow::ensure!(runtime.is_absolute(), "XDG_RUNTIME_DIR must be absolute");
            runtime.join(display)
        };
        let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
        socket.connect_timeout(&socket2::SockAddr::unix(path)?, Duration::from_millis(250))?;
        Connection::from_socket(socket.into())?
    };
    let socket = control.track(&conn)?;
    Ok((conn, socket))
}

pub(super) struct TrackedSocket {
    control: Arc<WorkerControl>,
    id: usize,
}

impl TrackedSocket {
    // Once input might be queued, disconnect is NOT a consumption/cancellation
    // acknowledgement. Protect the socket until a successful final sync; a
    // stuck compositor then causes an explicit bounded shutdown error while
    // the owned worker keeps its focus/raw-input lease. Never detach it.
    pub(super) fn protect_input(&self) -> anyhow::Result<()> {
        let mut sockets = self
            .control
            .sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(!self.control.stopped(), "Wayland worker generation retired");
        sockets.get_mut(&self.id).expect("tracked socket").1 = true;
        Ok(())
    }
}

impl Drop for TrackedSocket {
    fn drop(&mut self) {
        self.control
            .sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

pub(super) struct WorkerSender<C> {
    tx: Sender<C>,
    pub(super) control: Arc<WorkerControl>,
}

impl<C> Clone for WorkerSender<C> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            control: self.control.clone(),
        }
    }
}

impl<C> WorkerSender<C> {
    pub(super) fn send(&self, cmd: C) -> anyhow::Result<()> {
        anyhow::ensure!(!self.control.stopped(), "Wayland worker generation retired");
        // Preserve ordinary pointer backpressure, but cancellation always wakes
        // admission, even when the worker is blocked and its queue is full.
        crossbeam_channel::select_biased! {
            recv(self.control.stop_rx) -> _ => anyhow::bail!("Wayland worker generation retired"),
            send(self.tx, cmd) -> result => result.map_err(|_| anyhow::anyhow!("Wayland worker channel closed")),
        }
    }

    fn try_send(&self, cmd: C) -> anyhow::Result<()> {
        anyhow::ensure!(!self.control.stopped(), "Wayland worker generation retired");
        self.tx
            .try_send(cmd)
            .map_err(|_| anyhow::anyhow!("Wayland worker queue full or closed"))
    }

    pub(super) fn reply<R>(&self, rx: Receiver<anyhow::Result<R>>) -> anyhow::Result<R> {
        crossbeam_channel::select_biased! {
            recv(self.control.stop_rx) -> _ => Err(anyhow::anyhow!("Wayland worker generation retired; in-flight work remains owned until confirmed")),
            recv(rx) -> result => result.map_err(|_| anyhow::anyhow!("Wayland worker reply channel closed"))?,
        }
    }
}

struct WorkerOwner<C> {
    sender: WorkerSender<C>,
    join: thread::JoinHandle<()>,
}

struct WorkerState<C> {
    admitted: bool,
    retiring: usize,
    owner: Option<WorkerOwner<C>>,
}

pub(super) struct WorkerRegistry<C> {
    state: Mutex<WorkerState<C>>,
}

impl<C: Send + 'static> WorkerRegistry<C> {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(WorkerState {
                admitted: true,
                retiring: 0,
                owner: None,
            }),
        }
    }

    pub(super) fn sender(
        &self,
        name: &str,
        capacity: usize,
        run: impl FnOnce(Receiver<C>, Arc<WorkerControl>) + Send + 'static,
    ) -> anyhow::Result<WorkerSender<C>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            state.admitted,
            "Wayland worker generation retired; explicit restart required"
        );
        if state.owner.is_none() {
            let (tx, rx) = bounded(capacity);
            let control = WorkerControl::new();
            let worker_control = control.clone();
            let join = thread::Builder::new()
                .name(name.to_owned())
                .spawn(move || {
                    struct OnExit(Arc<WorkerControl>);
                    impl Drop for OnExit {
                        fn drop(&mut self) {
                            self.0.stop();
                        }
                    }
                    let _exit = OnExit(worker_control.clone());
                    run(rx, worker_control);
                })?;
            state.owner = Some(WorkerOwner {
                sender: WorkerSender { tx, control },
                join,
            });
        }
        let sender = &state.owner.as_ref().expect("worker owner").sender;
        anyhow::ensure!(
            !sender.control.stopped(),
            "Wayland worker generation stopped"
        );
        Ok(sender.clone())
    }

    pub(super) fn shutdown(&self) -> anyhow::Result<()> {
        let deadline = Instant::now() + RETIRE_TIMEOUT;
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.admitted = false;
            state.retiring += 1;
            if let Some(owner) = &state.owner {
                owner.sender.control.stop();
            }
        }
        let result = (|| loop {
            {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                match state.owner.as_ref() {
                    None => return Ok(()),
                    Some(owner) if owner.join.is_finished() => {
                        let owner = state.owner.take().expect("finished worker");
                        return owner
                            .join
                            .join()
                            .map_err(|_| anyhow::anyhow!("Wayland worker panicked"));
                    }
                    _ => {}
                }
            }
            anyhow::ensure!(Instant::now() < deadline,
                "Wayland worker retirement exceeded 1s; generation fenced, join handle and any pending input/focus lease retained");
            thread::sleep(Duration::from_millis(2));
        })();
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retiring -= 1;
        result
    }

    pub(super) fn restart(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            state.retiring == 0,
            "Wayland worker retirement still in progress"
        );
        if state.admitted
            && state
                .owner
                .as_ref()
                .is_none_or(|owner| !owner.sender.control.stopped())
        {
            return Ok(());
        }
        state.admitted = false;
        if let Some(owner) = &state.owner {
            anyhow::ensure!(
                owner.join.is_finished(),
                "previous Wayland worker has not retired"
            );
            let owner = state.owner.take().expect("finished worker");
            owner
                .join
                .join()
                .map_err(|_| anyhow::anyhow!("Wayland worker panicked"))?;
        }
        state.admitted = true;
        Ok(())
    }
}

static OWNER: OnceLock<WorkerRegistry<WlOverlayCmd>> = OnceLock::new();
// Starts false deliberately: the platform registration path must explicitly
// opt the native Wayland overlay in with the daemon's CursorConfig. This keeps
// lazy forwarding from bypassing --no-overlay before any window exists.
static CONFIG_ENABLED: AtomicBool = AtomicBool::new(false);
static CONFIG_TEMPLATE: OnceLock<CursorConfig> = OnceLock::new();

struct LayerShellProbe;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for LayerShellProbe {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Probe once so a queued command is never mistaken for a usable layer-shell
/// backend. This lets an older compositor helper remain a real fallback when
/// the compositor does not advertise `zwlr_layer_shell_v1`.
pub fn available() -> bool {
    let Ok(sender) = tx() else { return false };
    if let Some(available) = sender.control.available.get() {
        return !sender.control.stopped() && *available;
    }
    let (reply, rx) = bounded(1);
    if sender.send(WlOverlayCmd::Probe(reply)).is_err() {
        return false;
    }
    match sender.reply(rx) {
        Ok(available) if !sender.control.stopped() => {
            let _ = sender.control.available.set(available);
            available
        }
        _ => {
            sender.control.stop();
            false
        }
    }
}

fn probe_layer_shell(control: &Arc<WorkerControl>) -> anyhow::Result<bool> {
    let (conn, _socket) = connect_tracked(control)?;
    let (globals, _queue) = registry_queue_init::<LayerShellProbe>(&conn)?;
    Ok(globals.contents().with_list(|list| {
        list.iter()
            .any(|global| global.interface == "zwlr_layer_shell_v1")
    }))
}

pub fn set_config(config: CursorConfig) {
    CONFIG_ENABLED.store(config.enabled, Ordering::Release);
    let _ = CONFIG_TEMPLATE.set(config);
}

/// Queue defaults with cursor/lifecycle commands, including before the first
/// positioned cursor starts the compositor connection. The shared render map
/// preserves explicit motion overrides and remembers defaults for future keys.
pub fn set_default_glide_duration(session: Option<&str>, value: f64) -> anyhow::Result<()> {
    if CONFIG_ENABLED.load(Ordering::Acquire) {
        let tx = tx()?;
        // A stalled compositor must not block a config call. Report
        // failed admission instead of silently losing the new default.
        tx.try_send(WlOverlayCmd::SetDefaultGlideDuration {
            session: session.map(str::to_owned),
            value,
        })
        .map_err(|error| anyhow::anyhow!("Wayland cursor config queue unavailable: {error}"))?;
    }
    Ok(())
}

fn registry() -> &'static WorkerRegistry<WlOverlayCmd> {
    OWNER.get_or_init(WorkerRegistry::new)
}

fn tx() -> anyhow::Result<WorkerSender<WlOverlayCmd>> {
    registry().sender("cua-overlay-wl", 64, |rx, control| {
        if let Err(e) = owner_thread_owned(rx, control) {
            tracing::warn!("cua-overlay-wl thread exited with error: {e}");
        }
        crate::overlay::release_arrivals();
    })
}

/// Lazy startup is allowed only within the explicitly admitted SDK generation.
pub fn ensure_started() -> bool {
    CONFIG_ENABLED.load(Ordering::Acquire) && tx().is_ok()
}

/// Translate a generic [`OverlayMsg`] (the cross-platform command shape)
/// to the layer-shell owner thread. The owner-thread render core consumes
/// every variant the X11 path handles; only `ShowFocusRect` (macOS-only)
/// is silently dropped here.
pub fn forward(msg: &OverlayMsg) -> bool {
    if !should_forward(CONFIG_ENABLED.load(Ordering::Acquire), msg) {
        return false;
    }
    // All lifecycle transitions share the owner's command queue so a cold
    // Remove still suppresses late commands, and Revive preserves ordering.
    // Starting this thread does not connect to Wayland: lifecycle-only cleanup
    // must never create layer surfaces just to destroy them again.
    let Ok(tx) = tx() else { return false };
    map_overlay_msg(msg).is_some_and(|cmd| tx.try_send(cmd).is_ok())
}

fn map_overlay_msg(msg: &OverlayMsg) -> Option<WlOverlayCmd> {
    match msg {
        OverlayMsg::Remove(key) => Some(WlOverlayCmd::Remove(key.clone())),
        OverlayMsg::Cmd(kc) if matches!(&kc.cmd, OverlayCommand::ShowFocusRect(_)) => None,
        OverlayMsg::Cmd(kc) => Some(WlOverlayCmd::Cmd {
            key: kc.key.clone(),
            cmd: kc.cmd.clone(),
        }),
        OverlayMsg::Revive(key) => Some(WlOverlayCmd::Revive(key.clone())),
        OverlayMsg::Wake => None,
    }
}

fn should_forward(config_enabled: bool, msg: &OverlayMsg) -> bool {
    config_enabled
        && !matches!(
            msg,
            OverlayMsg::Cmd(kc) if matches!(&kc.cmd, OverlayCommand::ShowFocusRect(_))
        )
}

/// Fence admission, interrupt compositor reads, and join within one second.
/// A timeout keeps the join handle owned and admission closed; retry shutdown.
pub fn shutdown() -> anyhow::Result<()> {
    registry().shutdown()
}

/// Called only for a new SDK lifetime, never by lazy forwarding/configuration.
pub fn restart() -> anyhow::Result<()> {
    registry().restart()
}

// ── owner thread ─────────────────────────────────────────────────────────

struct OverlayState {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    xdg_output_manager: Option<ZxdgOutputManagerV1>,
    outputs: HashMap<u32, NativeOutput>,
    /// Outputs whose most recently committed buffer contains cursor pixels.
    /// Usually this contains exactly one output; keeping a set makes hide,
    /// removal, and topology changes clear every previously painted surface.
    painted_outputs: HashSet<u32>,
    /// Configured layer surfaces that have received at least one wl_buffer.
    /// This is intentionally separate from `painted_outputs`: every new or
    /// reconfigured surface needs a one-shot transparent commit to complete
    /// its layer-shell handshake, but stable empty surfaces must not trigger
    /// recurring full-output SHM redraws.
    initialized_outputs: HashSet<u32>,
    topology_dirty: bool,
    /// The shared keyed render map (`cursor_overlay::RenderMap`) gives this
    /// backend the same lifecycle as every other platform: removing a named
    /// key tombstones it, so already-queued late commands cannot recreate a
    /// cursor after end_session.
    render: WlRenderMap,
    /// In-flight wl_shm buffers awaiting `wl_buffer.release` from the
    /// compositor. Keyed by `WlBuffer` object id; value is the
    /// `(mmap ptr, mmap size, memfd fd)` triple that must be unmapped +
    /// closed once the compositor signals it's done with the buffer.
    /// Replaces the per-redraw `mem::forget` leak: the previous frame's
    /// memory is reclaimed as soon as the compositor releases it.
    pending_buffers: HashMap<u32, (*mut libc::c_void, usize, i32)>,
}

struct NativeOutput {
    output: WlOutput,
    xdg_output: Option<ZxdgOutputV1>,
    surface: Option<WlSurface>,
    layer_surface: Option<ZwlrLayerSurfaceV1>,
    wl_origin: Option<(i32, i32)>,
    logical_origin: Option<(i32, i32)>,
    logical_size: Option<(u32, u32)>,
    configured_size: Option<(u32, u32)>,
    mode_size: Option<(u32, u32)>,
    scale: i32,
    name: Option<String>,
    closed: bool,
}

impl NativeOutput {
    fn new(output: WlOutput) -> Self {
        Self {
            output,
            xdg_output: None,
            surface: None,
            layer_surface: None,
            wl_origin: None,
            logical_origin: None,
            logical_size: None,
            configured_size: None,
            mode_size: None,
            scale: 1,
            name: None,
            closed: false,
        }
    }

    fn layout(&self, id: u32) -> Option<OutputLayout> {
        if self.layer_surface.is_none() || self.configured_size.is_none() {
            return None;
        }
        let (origin_x, origin_y) = self.logical_origin.or(self.wl_origin).unwrap_or((0, 0));
        let (width, height) = self.logical_size.or(self.configured_size).or_else(|| {
            let scale = self.scale.max(1) as u32;
            self.mode_size
                .map(|(width, height)| ((width / scale).max(1), (height / scale).max(1)))
        })?;
        (width > 0 && height > 0).then_some(OutputLayout {
            id,
            origin_x,
            origin_y,
            width,
            height,
        })
    }

    fn destroy_surfaces(&mut self) {
        self.close_layer();
        if let Some(xdg_output) = self.xdg_output.take() {
            xdg_output.destroy();
        }
    }

    fn close_layer(&mut self) {
        if let Some(layer_surface) = self.layer_surface.take() {
            layer_surface.destroy();
        }
        if let Some(surface) = self.surface.take() {
            surface.destroy();
        }
        self.configured_size = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutputLayout {
    id: u32,
    origin_x: i32,
    origin_y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct SelectedOutput {
    id: u32,
    local_x: f64,
    local_y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameTarget {
    id: u32,
}

#[derive(Clone, Copy)]
struct OutputData {
    id: u32,
}

#[derive(Clone, Copy)]
struct LayerData {
    id: u32,
}

// SAFETY: the raw pointers in pending_buffers point at mmap regions owned
// exclusively by this thread (the owner thread). OverlayState is never
// shared across threads — wayland-client's EventQueue<State> is !Send so
// it stays pinned to the owner thread. The Send/Sync bounds wayland-client
// requires for State types apply to the struct as a whole, hence the
// explicit assertion.
unsafe impl Send for OverlayState {}

impl Drop for OverlayState {
    fn drop(&mut self) {
        for (_, (ptr, size, fd)) in std::mem::take(&mut self.pending_buffers) {
            super::cleanup_mmap(ptr, size, fd);
        }
    }
}

impl OverlayState {
    fn new(template: CursorConfig) -> Self {
        Self {
            compositor: None,
            shm: None,
            layer_shell: None,
            xdg_output_manager: None,
            outputs: HashMap::new(),
            painted_outputs: HashSet::new(),
            initialized_outputs: HashSet::new(),
            topology_dirty: false,
            // The default slot preserves the launch-time cursor id, while
            // lazily-created named slots take their key as the id.
            render: WlRenderMap::new(template, ()),
            pending_buffers: HashMap::new(),
        }
    }
}

type WlRenderMap = cursor_overlay::RenderMap<RenderStateCore>;

/// Apply one keyed command. A never-shown cursor is first seeded near its
/// target (clamped into `frame`, the union of the configured outputs) so a
/// spring animation begins on screen, as on every other platform.
fn apply_keyed_command(
    render: &mut WlRenderMap,
    frame: Option<ScreenFrame>,
    key: CursorKey,
    cmd: OverlayCommand,
) -> bool {
    let seed_target = match &cmd {
        OverlayCommand::MoveTo { x, y, .. }
        | OverlayCommand::SnapTo { x, y, .. }
        | OverlayCommand::ClickPulse { x, y } => Some((*x, *y)),
        _ => None,
    };
    if let Some((target_x, target_y)) = seed_target {
        render.seed_start_if_unplaced(&key, target_x, target_y, frame);
    }

    let disabling = matches!(&cmd, OverlayCommand::SetEnabled(false));
    let outcome = render.apply_command(key, cmd);
    if disabling {
        if let Some(key) = outcome.applied_key() {
            if let Some(core) = render.cursors.get_mut(key) {
                quiesce_hidden(core);
            }
        }
    }
    outcome.is_dirty()
}

fn remove_keyed_core(render: &mut WlRenderMap, key: CursorKey) -> bool {
    matches!(
        render.remove(key),
        MsgOutcome::Removed { existed: true, .. }
    )
}

fn revive_key(render: &mut WlRenderMap, key: CursorKey) {
    render.revive(key);
}

/// Bounding frame of every configured output, for seeding.
fn output_frame(state: &OverlayState) -> Option<ScreenFrame> {
    ScreenFrame::union(state.outputs.iter().filter_map(|(&id, output)| {
        output.layout(id).map(|layout| {
            ScreenFrame::new(
                f64::from(layout.origin_x),
                f64::from(layout.origin_y),
                f64::from(layout.width),
                f64::from(layout.height),
            )
        })
    }))
}

fn dbg(msg: &str) {
    if std::env::var_os("CUA_OVERLAY_DEBUG").is_some() {
        eprintln!("[cua-overlay-wl] {msg}");
    }
}

fn select_output(layouts: &[OutputLayout], x: f64, y: f64) -> Option<SelectedOutput> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    layouts
        .iter()
        .filter(|layout| output_contains(layout, x, y))
        .min_by_key(|layout| layout.id)
        .map(|layout| SelectedOutput {
            id: layout.id,
            local_x: x - f64::from(layout.origin_x),
            local_y: y - f64::from(layout.origin_y),
        })
}

fn output_contains(layout: &OutputLayout, x: f64, y: f64) -> bool {
    x.is_finite()
        && y.is_finite()
        && x >= f64::from(layout.origin_x)
        && y >= f64::from(layout.origin_y)
        && x < f64::from(layout.origin_x) + f64::from(layout.width)
        && y < f64::from(layout.origin_y) + f64::from(layout.height)
}

fn frame_plan(
    layouts: &[OutputLayout],
    painted_outputs: &HashSet<u32>,
    initialized_outputs: &HashSet<u32>,
    cursor_positions: impl IntoIterator<Item = (f64, f64)>,
) -> (HashSet<u32>, Vec<FrameTarget>) {
    let selected: HashSet<u32> = cursor_positions
        .into_iter()
        .filter_map(|(x, y)| select_output(layouts, x, y).map(|output| output.id))
        .collect();
    let mut ids: Vec<u32> = painted_outputs.iter().copied().collect();
    ids.extend(
        layouts
            .iter()
            .map(|layout| layout.id)
            .filter(|id| !initialized_outputs.contains(id)),
    );
    for id in &selected {
        if !ids.contains(id) {
            ids.push(*id);
        }
    }
    ids.sort_unstable();
    ids.dedup();
    let targets = ids.into_iter().map(|id| FrameTarget { id }).collect();
    (selected, targets)
}

/// Whether a global logical `[x, y, w, h]` effect rect overlaps an output.
fn effect_rect_intersects_output(layout: &OutputLayout, [x, y, w, h]: [f64; 4]) -> bool {
    let left = f64::from(layout.origin_x);
    let top = f64::from(layout.origin_y);
    w > 0.0
        && h > 0.0
        && x < left + f64::from(layout.width)
        && x + w > left
        && y < top + f64::from(layout.height)
        && y + h > top
}

/// Outputs touched by any effect rect, beyond the one each cursor sits on.
fn effect_spill_outputs(
    layouts: &[OutputLayout],
    effect_rects: impl IntoIterator<Item = [f64; 4]>,
) -> HashSet<u32> {
    let rects: Vec<[f64; 4]> = effect_rects.into_iter().collect();
    layouts
        .iter()
        .filter(|layout| {
            rects
                .iter()
                .any(|rect| effect_rect_intersects_output(layout, *rect))
        })
        .map(|layout| layout.id)
        .collect()
}

fn visible_cores_for_output<'a>(
    cores: &'a CursorMap<RenderStateCore>,
    layouts: &[OutputLayout],
    output_id: u32,
) -> Vec<(&'a CursorKey, &'a RenderStateCore)> {
    let output = layouts.iter().find(|layout| layout.id == output_id);
    let mut visible_cores: Vec<_> = cores
        .iter()
        .filter(|(_, core)| {
            core.is_revealed()
                && (select_output(layouts, core.pos.0, core.pos.1)
                    .is_some_and(|selected| selected.id == output_id)
                    // A cursor on another output whose trail or glow reaches
                    // this one is painted here too, at this output's origin.
                    || output.is_some_and(|layout| {
                        core.effect_bounds()
                            .is_some_and(|rect| effect_rect_intersects_output(layout, rect))
                    }))
        })
        .collect();
    visible_cores.sort_by_key(|(left, _)| *left);
    visible_cores
}

fn ensure_output_resources(state: &mut OverlayState, qh: &QueueHandle<OverlayState>) {
    let ids: Vec<u32> = state.outputs.keys().copied().collect();
    for id in ids {
        ensure_xdg_output(state, id, qh);
        ensure_layer_surface(state, id, qh);
    }
}

fn ensure_xdg_output(state: &mut OverlayState, id: u32, qh: &QueueHandle<OverlayState>) {
    let Some(manager) = state.xdg_output_manager.clone() else {
        return;
    };
    let Some(output) = state.outputs.get_mut(&id) else {
        return;
    };
    if output.xdg_output.is_none() {
        output.xdg_output = Some(manager.get_xdg_output(&output.output, qh, OutputData { id }));
    }
}

fn ensure_layer_surface(state: &mut OverlayState, id: u32, qh: &QueueHandle<OverlayState>) {
    let (Some(compositor), Some(layer_shell)) =
        (state.compositor.clone(), state.layer_shell.clone())
    else {
        return;
    };
    let Some(output) = state.outputs.get_mut(&id) else {
        return;
    };
    if output.layer_surface.is_some() || output.closed {
        return;
    }

    let surface = compositor.create_surface(qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        Some(&output.output),
        Layer::Overlay,
        "cua-agent-cursor".to_string(),
        qh,
        LayerData { id },
    );
    layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer_surface.set_size(0, 0);
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);

    let region: WlRegion = compositor.create_region(qh, ());
    surface.set_input_region(Some(&region));
    region.destroy();

    surface.commit();
    output.surface = Some(surface);
    output.layer_surface = Some(layer_surface);
}

/// Retain lifecycle and visual metadata without touching the compositor.
/// Session labels and BeginAction/EndAction also arrive for observation and
/// refused actions; none has a position to paint. Return the first positioned
/// command unapplied so the active loop can drain it and later transitions
/// together, in their original order.
#[cfg(test)]
fn wait_for_renderer_command(
    rx: &Receiver<WlOverlayCmd>,
    state: &mut OverlayState,
) -> Option<WlOverlayCmd> {
    wait_for_renderer_command_owned(rx, state, &WorkerControl::new())
}

fn wait_for_renderer_command_owned(
    rx: &Receiver<WlOverlayCmd>,
    state: &mut OverlayState,
    control: &Arc<WorkerControl>,
) -> Option<WlOverlayCmd> {
    loop {
        let command = crossbeam_channel::select_biased! {
            recv(control.stop_rx) -> _ => return None,
            recv(rx) -> result => result.ok()?,
        };
        if control.stopped() {
            return None;
        }
        match command {
            #[cfg(test)]
            WlOverlayCmd::Shutdown => return None,
            WlOverlayCmd::Probe(reply) => {
                let _ = reply.try_send(probe_layer_shell(control));
            }
            WlOverlayCmd::SetDefaultGlideDuration { session, value } => {
                state
                    .render
                    .set_default_glide_duration(session.as_deref(), value);
            }
            WlOverlayCmd::Remove(key) => {
                remove_keyed_core(&mut state.render, key);
            }
            WlOverlayCmd::Revive(key) => revive_key(&mut state.render, key),
            WlOverlayCmd::Cmd { ref key, ref cmd } if !state.render.ended.contains(key) => {
                if matches!(
                    cmd,
                    OverlayCommand::MoveTo { .. }
                        | OverlayCommand::SnapTo { .. }
                        | OverlayCommand::ClickPulse { .. }
                ) {
                    return Some(command);
                }
                apply_keyed_command(&mut state.render, None, key.clone(), cmd.clone());
            }
            WlOverlayCmd::Cmd { .. } => {}
        }
    }
}

#[cfg(test)]
fn owner_thread(rx: Receiver<WlOverlayCmd>) -> anyhow::Result<()> {
    owner_thread_owned(rx, WorkerControl::new())
}

fn owner_thread_owned(
    rx: Receiver<WlOverlayCmd>,
    control: Arc<WorkerControl>,
) -> anyhow::Result<()> {
    let template = CONFIG_TEMPLATE.get().cloned().unwrap_or_default();
    let mut state = OverlayState::new(template);
    let Some(first_command) = wait_for_renderer_command_owned(&rx, &mut state, &control) else {
        return Ok(());
    };

    dbg("connecting after positioned cursor command");
    let (conn, _socket) = connect_tracked(&control)?;
    let mut queue = conn.new_event_queue::<OverlayState>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    queue.roundtrip(&mut state)?;

    state
        .compositor
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor does not expose wl_compositor"))?;
    let shm = state
        .shm
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor does not expose wl_shm"))?;
    state
        .layer_shell
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor does not expose zwlr_layer_shell_v1"))?;
    if state.outputs.is_empty() {
        anyhow::bail!("compositor exposed no wl_output");
    }

    // Build one fullscreen, click-through layer surface per advertised output.
    ensure_output_resources(&mut state, &qh);

    // Give every enabled output a chance to configure. Disabled outputs may
    // stay advertised without configuring and are excluded from selection.
    for _ in 0..10 {
        queue.roundtrip(&mut state)?;
        ensure_output_resources(&mut state, &qh);
        if state
            .outputs
            .values()
            .filter(|output| output.layer_surface.is_some())
            .all(|output| output.configured_size.is_some())
        {
            break;
        }
    }

    let configured_outputs = state
        .outputs
        .values()
        .filter(|output| output.configured_size.is_some())
        .count();
    if configured_outputs == 0 {
        anyhow::bail!("no layer surface received a configure event");
    }
    dbg(&format!("configured outputs: {configured_outputs}"));
    state.topology_dirty = false;

    // A layer-shell surface is not fully initialized until the first buffer is
    // attached after configure. Commit one transparent buffer to every empty
    // output now, before entering the demand-driven loop; a cursor already
    // selected on an output can share this same first frame.
    redraw(&mut state, &shm, &qh)?;
    queue.roundtrip(&mut state)?;

    // Demand-driven main loop. Stable cursors perform only a cheap Wayland
    // maintenance roundtrip once per second so output topology changes are
    // observed; full-display SHM work remains limited to visual changes.
    let mut last_tick = Instant::now();
    let mut frame_tick_needed = false;
    let mut startup_command = Some(first_command);
    loop {
        if control.stopped() {
            break;
        }
        let wait = next_wait(
            &state.render.cursors,
            frame_tick_needed,
            state.topology_dirty,
        );
        let wake = startup_command
            .take()
            .map(WlWake::Command)
            .unwrap_or_else(|| wait_for_work_owned(&rx, wait, &control));
        let (first_cmd, timed_out) = match wake {
            WlWake::Command(cmd) => (Some(cmd), None),
            WlWake::Timeout => (None, Some(wait)),
            WlWake::Disconnected => break,
        };

        let now = Instant::now();
        let elapsed = now.duration_since(last_tick).as_secs_f64();
        last_tick = now;

        let mut dirty = false;
        let mut shutdown = false;
        let mut pending = first_cmd;
        let frame = output_frame(&state);
        loop {
            let received = pending.take().map(Ok).unwrap_or_else(|| rx.try_recv());
            if control.stopped() {
                shutdown = true;
                break;
            }
            match received {
                Ok(WlOverlayCmd::Probe(reply)) => {
                    let _ = reply.try_send(Ok(true));
                }
                #[cfg(test)]
                Ok(WlOverlayCmd::Shutdown) => {
                    shutdown = true;
                    break;
                }
                Ok(WlOverlayCmd::SetDefaultGlideDuration { session, value }) => {
                    state
                        .render
                        .set_default_glide_duration(session.as_deref(), value);
                }
                Ok(WlOverlayCmd::Cmd { key, cmd }) => {
                    dirty |= apply_keyed_command(&mut state.render, frame, key, cmd);
                }
                Ok(WlOverlayCmd::Remove(key)) => {
                    dirty |= remove_keyed_core(&mut state.render, key);
                }
                Ok(WlOverlayCmd::Revive(key)) => {
                    revive_key(&mut state.render, key);
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    shutdown = true;
                    break;
                }
            }
        }
        if shutdown {
            break;
        }

        // Advance only on a scheduled animation/fade wake. A command wake
        // applies the new state at dt=0, avoiding a jump proportional to how
        // long the loop was parked.
        let mut arrived = Vec::new();
        if let Some(timeout_kind) = timed_out {
            match timeout_kind {
                WlWait::Frame => {
                    arrived.extend(tick_all_cores(&mut state.render.cursors, elapsed.min(0.05)));
                    dirty = true;
                }
                WlWait::Deadline(_) => {
                    arrived.extend(tick_all_cores(&mut state.render.cursors, elapsed));
                    dirty = true;
                }
                WlWait::Maintenance(_) => {
                    let before: HashMap<CursorKey, f64> = state
                        .render
                        .cursors
                        .iter()
                        .map(|(key, core)| (key.clone(), core.idle_alpha))
                        .collect();
                    arrived.extend(tick_all_cores(&mut state.render.cursors, elapsed));
                    dirty |= state.render.cursors.iter().any(|(key, core)| {
                        before
                            .get(key)
                            .is_none_or(|alpha| *alpha != core.idle_alpha)
                            || needs_frame_tick(core)
                    });

                    let topology_was_dirty = state.topology_dirty;
                    state.topology_dirty = false;
                    queue.roundtrip(&mut state)?;
                    ensure_output_resources(&mut state, &qh);
                    dirty |= topology_was_dirty || state.topology_dirty;
                    state.topology_dirty = false;
                }
            }
        }
        let next_frame_tick_needed = any_core_needs_frame_tick(&state.render.cursors);
        if dirty || frame_tick_needed || next_frame_tick_needed {
            redraw(&mut state, &shm, &qh)?;
            // Flush the committed frame and dispatch wl_buffer.release before
            // parking. The buffer map remains authoritative until release, so
            // no mmap/fd can be reclaimed while the compositor still uses it.
            queue.roundtrip(&mut state)?;
        }
        // Report arrivals only once the frame at the target is committed, so
        // a pointer action lands after the viewer saw the glide end (the X11
        // renderer's contract).
        for key in arrived {
            crate::overlay::fire_arrival(&key);
        }
        frame_tick_needed = next_frame_tick_needed;
    }

    for output in state.outputs.values_mut() {
        output.destroy_surfaces();
    }
    if !control.stopped() {
        queue.roundtrip(&mut state)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WlWait {
    Frame,
    Deadline(Duration),
    Maintenance(Duration),
}

enum WlWake {
    Command(WlOverlayCmd),
    Timeout,
    Disconnected,
}

fn wait_for_work_owned(
    rx: &Receiver<WlOverlayCmd>,
    wait: WlWait,
    control: &WorkerControl,
) -> WlWake {
    let timeout = match wait {
        WlWait::Frame => Duration::from_millis(16),
        WlWait::Deadline(timeout) | WlWait::Maintenance(timeout) => timeout,
    };
    crossbeam_channel::select_biased! {
        recv(control.stop_rx) -> _ => WlWake::Disconnected,
        recv(rx) -> result => match result {
            Ok(cmd) => WlWake::Command(cmd),
            Err(_) => WlWake::Disconnected,
        },
        default(timeout) => WlWake::Timeout,
    }
}

#[cfg(test)]
fn wait_for_work(rx: &Receiver<WlOverlayCmd>, wait: WlWait) -> WlWake {
    match wait {
        WlWait::Frame => match rx.recv_timeout(Duration::from_millis(16)) {
            Ok(cmd) => WlWake::Command(cmd),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => WlWake::Timeout,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => WlWake::Disconnected,
        },
        WlWait::Deadline(timeout) | WlWait::Maintenance(timeout) => {
            match rx.recv_timeout(timeout) {
                Ok(cmd) => WlWake::Command(cmd),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => WlWake::Timeout,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => WlWake::Disconnected,
            }
        }
    }
}

const TOPOLOGY_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);

fn next_wait(
    cores: &CursorMap<RenderStateCore>,
    frame_tick_needed: bool,
    topology_dirty: bool,
) -> WlWait {
    if topology_dirty {
        return WlWait::Maintenance(Duration::ZERO);
    }
    if frame_tick_needed || any_core_needs_frame_tick(cores) {
        return WlWait::Frame;
    }
    match earliest_idle_fade_wait(cores) {
        Some(wait) if wait <= TOPOLOGY_MAINTENANCE_INTERVAL => WlWait::Deadline(wait),
        _ => WlWait::Maintenance(TOPOLOGY_MAINTENANCE_INTERVAL),
    }
}

fn any_core_needs_frame_tick(cores: &CursorMap<RenderStateCore>) -> bool {
    cores.values().any(needs_frame_tick)
}

fn earliest_idle_fade_wait(cores: &CursorMap<RenderStateCore>) -> Option<Duration> {
    cores
        .values()
        .filter_map(RenderStateCore::idle_fade_wait)
        .min()
}

/// Advance every cursor and return the keys whose planned glide just ended.
fn tick_all_cores(cores: &mut CursorMap<RenderStateCore>, dt: f64) -> Vec<CursorKey> {
    cursor_overlay::render_map::update_peer_presence(cores.values_mut());
    cores
        .iter_mut()
        .filter_map(|(key, core)| core.tick_motion(dt).then(|| key.clone()))
        .collect()
}

/// The shared frame-tick predicate, including resting motion (the float bob),
/// gated on a shown, placed cursor: a hidden cursor's motion is quiesced by
/// [`quiesce_hidden`] and never repaints a layer surface.
fn needs_frame_tick(core: &RenderStateCore) -> bool {
    core.is_enabled() && core.is_placed() && core.needs_frame_tick()
}

fn quiesce_hidden(core: &mut RenderStateCore) {
    core.cancel_motion();
    core.click_age = None;
    core.click_t = None;
}

/// Composite all visible cursor cores into their selected output and attach a
/// transparent clearing frame to every previously painted output now empty.
///
/// Pipeline:
/// 1. Allocate a memfd-backed wl_shm pool sized at output_w × output_h.
/// 2. Paint the cross-platform cursor (bloom + click pulse + gradient
///    arrow) into a `tiny_skia::Pixmap` via `cursor_overlay::paint_cursor`
///    — same call the X11 path uses.
/// 3. Channel-swap RGBA → BGRA into the wl_shm buffer (wl_shm Argb8888
///    is little-endian BGRA in memory). This is the inverse of the swap
///    in `ext_screencopy::encode_buffer_to_png`.
/// 4. Attach + damage + commit on the layer surface.
///
/// Hidden, idle-faded, or off-screen cores paint nothing.
fn redraw(
    state: &mut OverlayState,
    shm: &WlShm,
    qh: &QueueHandle<OverlayState>,
) -> anyhow::Result<()> {
    let layouts: Vec<OutputLayout> = state
        .outputs
        .iter()
        .filter_map(|(&id, output)| output.layout(id))
        .collect();
    let cursor_positions = state
        .render
        .cursors
        .values()
        .filter(|core| core.is_revealed())
        .map(|core| core.pos);
    let (mut selected, mut targets) = frame_plan(
        &layouts,
        &state.painted_outputs,
        &state.initialized_outputs,
        cursor_positions,
    );
    // Each surface is a full-output buffer with full damage, so effects on the
    // cursor's own output are drawn and erased with no extra bookkeeping. A
    // trail or glow can spill onto a neighbouring output, though: paint those
    // outputs too, and record them as painted so they are cleared next frame.
    let effect_rects = state
        .render
        .cursors
        .values()
        .filter(|core| core.is_revealed())
        .filter_map(|core| core.effect_bounds());
    for id in effect_spill_outputs(&layouts, effect_rects) {
        if selected.insert(id) && !targets.iter().any(|target| target.id == id) {
            targets.push(FrameTarget { id });
        }
    }

    for target in targets {
        redraw_output(state, shm, qh, target, &layouts)?;
    }

    state.painted_outputs = selected;
    Ok(())
}

fn redraw_output(
    state: &mut OverlayState,
    shm: &WlShm,
    qh: &QueueHandle<OverlayState>,
    target: FrameTarget,
    layouts: &[OutputLayout],
) -> anyhow::Result<()> {
    let Some((surface, layout)) = state
        .outputs
        .get(&target.id)
        .and_then(|output| Some((output.surface.clone()?, output.layout(target.id)?)))
    else {
        return Ok(());
    };
    let w = layout.width.max(1);
    let h = layout.height.max(1);
    let stride = w
        .checked_mul(4)
        .and_then(|stride| i32::try_from(stride).ok())
        .ok_or_else(|| anyhow::anyhow!("overlay output {w}x{h} has an invalid stride"))?;
    let size = usize::try_from(stride)
        .ok()
        .and_then(|stride| stride.checked_mul(h as usize))
        .ok_or_else(|| anyhow::anyhow!("overlay output {w}x{h} buffer size overflow"))?;

    // A clearing target intentionally stays transparent. Each cursor selected
    // for this output subtracts its logical compositor origin from the global
    // position; this is the same origin contract used by the Windows virtual
    // desktop.
    let pm_result = tiny_skia::Pixmap::new(w, h);
    let mut pm = match pm_result {
        Some(p) => p,
        // tiny_skia::Pixmap::new only fails on OOM at sizes that fit u32.
        // We refuse to fall back to a 1x1 pixmap because the subsequent
        // RGBA → BGRA loop indexes `src[i+3]` over the full `size` range —
        // a 1x1 source would crash. Surface the allocation failure
        // properly instead.
        None => anyhow::bail!(
            "tiny_skia::Pixmap::new({w}, {h}) failed — out of memory for the overlay buffer"
        ),
    };

    // Reuses the same anon_shm pattern as the screencopy path in mod.rs. The
    // pixmap is allocated first so a tiny-skia OOM cannot strand this mmap/fd.
    let (fd, ptr) =
        super::anon_shm(size).map_err(|e| anyhow::anyhow!("overlay shm allocation failed: {e}"))?;

    // SAFETY: ptr came from mmap of `size` bytes and is transferred to
    // `pending_buffers` before this function returns successfully.
    let pixels: &mut [u8] = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, size) };
    let mut painted_positions = Vec::new();
    {
        // HashMap iteration is intentionally normalized by key so overlapping
        // named cursors composite deterministically from frame to frame.
        for (_, core) in visible_cores_for_output(&state.render.cursors, layouts, target.id) {
            cursor_overlay::paint_cursor(
                &mut pm,
                core,
                f64::from(layout.origin_x),
                f64::from(layout.origin_y),
                None,
                1.0,
            );
            painted_positions.push(core.pos);
        }
    }

    // CUA_OVERLAY_DEBUG=1 paints a 60x60 magenta square at the cursor's
    // current pos on top of the gradient arrow. Useful when validating
    // layer-shell visibility on a new compositor — the gradient arrow is
    // small at native scale and easy to miss in a screenshot, while the
    // magenta block is impossible to miss.
    if std::env::var_os("CUA_OVERLAY_DEBUG").is_some() {
        for &(cursor_x, cursor_y) in &painted_positions {
            let cx = (cursor_x - f64::from(layout.origin_x)) as i32;
            let cy = (cursor_y - f64::from(layout.origin_y)) as i32;
            let half = 30i32;
            for dy in -half..half {
                for dx in -half..half {
                    let px = cx + dx;
                    let py = cy + dy;
                    if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                        continue;
                    }
                    let off = ((py as usize) * (w as usize) + (px as usize)) * 4;
                    pm.data_mut()[off] = 0xFF; // R
                    pm.data_mut()[off + 1] = 0x00; // G
                    pm.data_mut()[off + 2] = 0xFF; // B
                    pm.data_mut()[off + 3] = 0xFF; // A
                }
            }
        }
    }

    // RGBA → BGRA channel swap. tiny_skia stores pixels as RGBA8888
    // (premultiplied); wl_shm Argb8888 is little-endian = BGRA in memory.
    // Mirrors the inverse swap in ext_screencopy::encode_buffer_to_png.
    let src = pm.data();
    for i in (0..size).step_by(4) {
        // pm.data() is already RGBA premultiplied; just swap R↔B.
        pixels[i] = src[i + 2]; // B ← R
        pixels[i + 1] = src[i + 1]; // G
        pixels[i + 2] = src[i]; // R ← B
        pixels[i + 3] = src[i + 3]; // A
    }

    use std::os::fd::AsFd as _;
    let pool_fd = unsafe { super::borrowed_fd(fd) };
    let pool: WlShmPool = shm.create_pool(pool_fd.as_fd(), size as i32, qh, ());
    let buffer: WlBuffer = pool.create_buffer(
        0,
        w as i32,
        h as i32,
        stride,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );

    // Track the (mmap, fd) by buffer object id so the wl_buffer.release
    // event Dispatch handler can clean up exactly when the compositor is
    // done with the underlying memory — no leak, no use-after-free.
    let buffer_id = buffer.id().protocol_id();
    state.pending_buffers.insert(buffer_id, (ptr, size, fd));

    dbg(&format!(
        "redraw output={} origin=({}, {}) w={w} h={h} stride={stride} buf_id={buffer_id} cursors={}",
        output_label(state, target.id),
        layout.origin_x,
        layout.origin_y,
        painted_positions.len(),
    ));
    surface.attach(Some(&buffer), 0, 0);
    // Damage both coordinate spaces. Some wlroots compositors otherwise leave
    // stale transparent frames behind while an animated cursor is moving.
    surface.damage(0, 0, w as i32, h as i32);
    surface.damage_buffer(0, 0, w as i32, h as i32);
    surface.commit();
    // Mark initialized only after the buffer attach + surface commit succeed.
    // An early return above leaves the output pending for the next plan.
    state.initialized_outputs.insert(target.id);
    pool.destroy();
    Ok(())
}

fn output_label(state: &OverlayState, id: u32) -> String {
    state
        .outputs
        .get(&id)
        .and_then(|output| output.name.clone())
        .unwrap_or_else(|| format!("wl_output#{id}"))
}

// ── Wayland Dispatch impls ───────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, ()> for OverlayState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                match interface.as_str() {
                    "wl_compositor" => {
                        state.compositor =
                            Some(registry.bind::<WlCompositor, _, _>(name, version.min(6), qh, ()));
                    }
                    "wl_shm" => {
                        state.shm =
                            Some(registry.bind::<WlShm, _, _>(name, version.min(1), qh, ()));
                    }
                    "wl_output" => {
                        let output = registry.bind::<WlOutput, _, _>(
                            name,
                            version.min(4),
                            qh,
                            OutputData { id: name },
                        );
                        state.outputs.insert(name, NativeOutput::new(output));
                        state.topology_dirty = true;
                    }
                    "zwlr_layer_shell_v1" => {
                        state.layer_shell = Some(registry.bind::<ZwlrLayerShellV1, _, _>(
                            name,
                            version.min(4),
                            qh,
                            (),
                        ));
                    }
                    "zxdg_output_manager_v1" => {
                        state.xdg_output_manager =
                            Some(registry.bind::<ZxdgOutputManagerV1, _, _>(
                                name,
                                version.min(3),
                                qh,
                                (),
                            ));
                    }
                    _ => {}
                }
                ensure_output_resources(state, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.painted_outputs.remove(&name);
                state.initialized_outputs.remove(&name);
                if let Some(mut output) = state.outputs.remove(&name) {
                    output.destroy_surfaces();
                    // wl_output.release was added in v3. On an older generic
                    // wlroots compositor, dropping the client proxy is the only
                    // valid cleanup; issuing the newer request would be a
                    // protocol error.
                    if output.output.version() >= 3 {
                        output.output.release();
                    }
                    state.topology_dirty = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlCompositor, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _: &WlCompositor,
        _: <WlCompositor as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShm, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _: &WlShm,
        _: <WlShm as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, OutputData> for OverlayState {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        data: &OutputData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_output;
        let Some(output) = state.outputs.get_mut(&data.id) else {
            return;
        };
        match event {
            wl_output::Event::Geometry { x, y, .. } => {
                state.topology_dirty |= output.wl_origin != Some((x, y));
                output.wl_origin = Some((x, y));
            }
            wl_output::Event::Mode { width, height, .. } if width > 0 && height > 0 => {
                state.topology_dirty |= output.mode_size != Some((width as u32, height as u32));
                output.mode_size = Some((width as u32, height as u32));
            }
            wl_output::Event::Scale { factor } => {
                state.topology_dirty |= output.scale != factor.max(1);
                output.scale = factor.max(1);
            }
            wl_output::Event::Name { name } => {
                output.name = Some(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for OverlayState {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, OutputData> for OverlayState {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as Proxy>::Event,
        data: &OutputData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(&data.id) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                state.topology_dirty |= output.logical_origin != Some((x, y));
                output.logical_origin = Some((x, y));
            }
            zxdg_output_v1::Event::LogicalSize { width, height } if width > 0 && height > 0 => {
                state.topology_dirty |= output.logical_size != Some((width as u32, height as u32));
                output.logical_size = Some((width as u32, height as u32));
            }
            zxdg_output_v1::Event::Name { name } => {
                output.name = Some(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _: &ZwlrLayerShellV1,
        _: <ZwlrLayerShellV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, LayerData> for OverlayState {
    fn event(
        state: &mut Self,
        layer: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        data: &LayerData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                if let Some(output) = state.outputs.get_mut(&data.id) {
                    output.closed = false;
                    let fallback = output.logical_size.or(output.mode_size).unwrap_or((1, 1));
                    let configured_size = (
                        if width > 0 { width } else { fallback.0 },
                        if height > 0 { height } else { fallback.1 },
                    );
                    // Each configure starts a new layer-surface buffer cycle.
                    // Queue exactly one matching clear/paint frame even when
                    // the compositor repeats the same logical size.
                    state.initialized_outputs.remove(&data.id);
                    state.topology_dirty = true;
                    output.configured_size = Some(configured_size);
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.painted_outputs.remove(&data.id);
                state.initialized_outputs.remove(&data.id);
                if let Some(output) = state.outputs.get_mut(&data.id) {
                    output.closed = true;
                    output.close_layer();
                    state.topology_dirty = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSurface, ()> for OverlayState {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: <WlSurface as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for OverlayState {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: <WlShmPool as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for OverlayState {
    fn event(
        state: &mut Self,
        buffer: &WlBuffer,
        event: <WlBuffer as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_buffer;
        if matches!(event, wl_buffer::Event::Release) {
            // Compositor is done with the underlying mmap. Free it +
            // close the memfd + destroy the wayland object.
            let id = buffer.id().protocol_id();
            if let Some((ptr, size, fd)) = state.pending_buffers.remove(&id) {
                super::cleanup_mmap(ptr, size, fd);
            }
            buffer.destroy();
        }
    }
}

impl Dispatch<WlRegion, ()> for OverlayState {
    fn event(
        _: &mut Self,
        _: &WlRegion,
        _: <WlRegion as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cursor_overlay::KeyedOverlayCommand;

    #[test]
    fn owned_overlay_cold_double_retirement_and_fresh_generation() {
        let registry = WorkerRegistry::<WlOverlayCmd>::new();
        registry.shutdown().unwrap();
        registry.shutdown().unwrap();
        assert!(registry
            .sender("cold-denied", 1, |_, _| panic!("must stay cold"))
            .is_err());
        registry.restart().unwrap();
        let old = registry
            .sender("overlay-test-idle", 1, |rx, control| {
                owner_thread_owned(rx, control).unwrap();
            })
            .unwrap();
        old.send(WlOverlayCmd::Remove("fixture".into())).unwrap();
        registry.shutdown().unwrap();
        assert!(old.send(WlOverlayCmd::Revive("fixture".into())).is_err());
        assert!(old.control.sockets.lock().unwrap().is_empty());
        assert!(registry.state.lock().unwrap().owner.is_none());
        assert!(registry.sender("still-denied", 1, |_, _| {}).is_err());
        registry.restart().unwrap();
        let fresh = registry
            .sender("overlay-test-fresh", 1, |rx, control| {
                owner_thread_owned(rx, control).unwrap();
            })
            .unwrap();
        assert!(!Arc::ptr_eq(&old.control, &fresh.control));
        assert!(old.send(WlOverlayCmd::Shutdown).is_err());
        registry.shutdown().unwrap();
    }

    #[test]
    fn owned_concurrent_retirement_cannot_be_reopened_or_late_attach() {
        let registry = Arc::new(WorkerRegistry::<()>::new());
        let (release, wait) = bounded::<()>(0);
        let old = registry
            .sender("wayland-test-retire-race", 1, move |_, control| {
                let _ = control.stop_rx.recv();
                wait.recv().unwrap();
            })
            .unwrap();
        let retiring = registry.clone();
        let shutdown = thread::spawn(move || retiring.shutdown());
        assert!(old
            .control
            .stop_rx
            .recv_timeout(Duration::from_secs(3))
            .is_err());
        assert!(registry.restart().is_err());
        assert!(registry.sender("no-lazy-reopen", 1, |_, _| {}).is_err());
        let (client, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let conn = Connection::from_socket(client).unwrap();
        assert!(old.control.track(&conn).is_err());
        assert!(old.control.sockets.lock().unwrap().is_empty());
        release.send(()).unwrap();
        shutdown.join().unwrap().unwrap();
        registry.restart().unwrap();
        assert!(old.send(()).is_err());
        registry.shutdown().unwrap();
    }

    // A real Wayland Connection speaks only wl_display.sync to this private
    // socket pair. No environment changes, compositor, surface or input event.
    fn read_sync(peer: &mut std::os::unix::net::UnixStream) -> u32 {
        use std::io::Read;
        peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut request = [0u8; 12];
        peer.read_exact(&mut request).unwrap();
        assert_eq!(u32::from_ne_bytes(request[..4].try_into().unwrap()), 1);
        assert_eq!(
            u32::from_ne_bytes(request[4..8].try_into().unwrap()),
            12 << 16
        );
        u32::from_ne_bytes(request[8..].try_into().unwrap())
    }

    #[test]
    fn owned_stuck_roundtrip_full_queue_and_old_caller_cancel() {
        use std::io::Read;
        let registry = WorkerRegistry::<Sender<anyhow::Result<()>>>::new();
        let (client, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let old = registry
            .sender("wayland-test-stalled", 1, move |rx, control| {
                let reply = rx.recv().unwrap();
                let conn = Connection::from_socket(client).unwrap();
                let _socket = control.track(&conn).unwrap();
                let result = conn.roundtrip().map(|_| ()).map_err(anyhow::Error::from);
                assert!(
                    result.is_err(),
                    "shutdown must interrupt a blocked protocol read"
                );
                let _ = reply.try_send(result);
            })
            .unwrap();
        let (reply, receive) = bounded(1);
        old.send(reply).unwrap();
        read_sync(&mut peer); // proves the owner is stalled waiting for a reply
        let (queued_reply, queued_receive) = bounded(1);
        old.send(queued_reply).unwrap();
        assert!(
            old.try_send(bounded(1).0).is_err(),
            "best-effort queue admission never blocks"
        );
        let waiting = old.clone();
        let caller = thread::spawn(move || waiting.reply(receive));
        registry.shutdown().unwrap();
        assert!(caller.join().unwrap().is_err());
        assert!(queued_receive.try_recv().is_err());
        assert!(old.send(bounded(1).0).is_err());
        assert!(old.control.sockets.lock().unwrap().is_empty());
        let mut byte = [0u8; 1];
        assert_eq!(
            peer.read(&mut byte).unwrap(),
            0,
            "all client socket handles closed"
        );
    }

    #[test]
    fn owned_stuck_reply_wait_is_preserved_until_bounded_shutdown() {
        let registry = WorkerRegistry::<Sender<anyhow::Result<()>>>::new();
        let (client, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let sender = registry
            .sender("wayland-test-request-timeout", 1, move |rx, control| {
                let reply = rx.recv().unwrap();
                let conn = Connection::from_socket(client).unwrap();
                let _socket = control.track(&conn).unwrap();
                let _ = reply.try_send(conn.roundtrip().map(|_| ()).map_err(anyhow::Error::from));
            })
            .unwrap();
        let (reply, receive) = bounded(1);
        sender.send(reply).unwrap();
        read_sync(&mut peer);
        let waiting = sender.clone();
        let caller = thread::spawn(move || waiting.reply(receive));
        thread::sleep(RETIRE_TIMEOUT + Duration::from_millis(20));
        assert!(
            !caller.is_finished(),
            "ordinary requests retain their original wait semantics"
        );
        let start = Instant::now();
        registry.shutdown().unwrap();
        assert!(caller.join().unwrap().is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(sender.send(bounded(1).0).is_err());
    }

    #[test]
    fn owned_unconfirmed_input_keeps_lease_and_join_handle_until_sync() {
        use std::io::Write;
        struct FakeLease(Arc<AtomicBool>);
        impl Drop for FakeLease {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let worker_dropped = dropped.clone();
        let registry = WorkerRegistry::<()>::new();
        let (client, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let old = registry
            .sender("wayland-test-protected", 1, move |_, control| {
                let conn = Connection::from_socket(client).unwrap();
                let socket = control.track(&conn).unwrap();
                let _lease = FakeLease(worker_dropped);
                socket.protect_input().unwrap();
                // Only sync is transmitted; protection simulates queued input.
                conn.roundtrip().unwrap();
            })
            .unwrap();
        let callback = read_sync(&mut peer);
        let start = Instant::now();
        assert!(registry
            .shutdown()
            .unwrap_err()
            .to_string()
            .contains("retained"));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(!dropped.load(Ordering::Acquire));
        assert!(registry.state.lock().unwrap().owner.is_some());
        assert!(registry.restart().is_err());
        assert!(old.send(()).is_err());
        let mut response = Vec::new();
        response.extend_from_slice(&callback.to_ne_bytes());
        response.extend_from_slice(&(12u32 << 16).to_ne_bytes());
        response.extend_from_slice(&0u32.to_ne_bytes());
        peer.write_all(&response).unwrap();
        registry.shutdown().unwrap();
        assert!(dropped.load(Ordering::Acquire));
        assert!(old.control.sockets.lock().unwrap().is_empty());
        registry.restart().unwrap();
    }

    fn message(cmd: OverlayCommand) -> OverlayMsg {
        OverlayMsg::Cmd(KeyedOverlayCommand {
            key: "test".to_owned(),
            cmd,
        })
    }

    fn positioned_core() -> RenderStateCore {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.pos = (100.0, 100.0);
        core.placed = true;
        core.motion.idle_hide_ms = 1_000.0;
        core
    }

    fn three_monitor_layout() -> Vec<OutputLayout> {
        vec![
            // 1920x1080 logical laptop panel at 2x backing scale.
            OutputLayout {
                id: 1,
                origin_x: 0,
                origin_y: 0,
                width: 1920,
                height: 1080,
            },
            // 2560x1440 logical external display, raised above the laptop.
            OutputLayout {
                id: 2,
                origin_x: 1920,
                origin_y: -200,
                width: 2560,
                height: 1440,
            },
            // Fractionally scaled portrait display left of the laptop.
            OutputLayout {
                id: 3,
                origin_x: -1280,
                origin_y: 56,
                width: 1280,
                height: 1024,
            },
        ]
    }

    /// The layer-shell renderer reports a glide's end exactly once, for the
    /// key that moved, so pointer actions can wait for it like on X11.
    #[test]
    fn ticking_reports_each_glide_arrival_once() {
        let mut render = WlRenderMap::new(CursorConfig::default(), ());
        let frame = Some(ScreenFrame::new(0.0, 0.0, 1920.0, 1080.0));
        apply_keyed_command(
            &mut render,
            frame,
            "mover".to_owned(),
            OverlayCommand::MoveTo {
                x: 400.0,
                y: 300.0,
                end_heading_radians: std::f64::consts::FRAC_PI_4,
                target: None,
            },
        );
        let mut still = positioned_core();
        still.motion.idle_hide_ms = 0.0;
        render.cursors.insert("still".to_owned(), still);

        let mut arrivals = Vec::new();
        for _ in 0..600 {
            arrivals.extend(tick_all_cores(&mut render.cursors, 1.0 / 60.0));
        }
        assert_eq!(arrivals, vec!["mover".to_owned()]);
        assert!(
            render.cursors["mover"].trajectory.is_none(),
            "the planned glide has ended"
        );
    }

    fn initialized(layouts: &[OutputLayout]) -> HashSet<u32> {
        layouts.iter().map(|layout| layout.id).collect()
    }

    #[test]
    fn selects_output_and_converts_global_to_output_local_coordinates() {
        let layouts = three_monitor_layout();
        assert_eq!(
            select_output(&layouts, 2500.0, 100.0),
            Some(SelectedOutput {
                id: 2,
                local_x: 580.0,
                local_y: 300.0,
            })
        );
        assert_eq!(
            select_output(&layouts, -1000.0, 256.0),
            Some(SelectedOutput {
                id: 3,
                local_x: 280.0,
                local_y: 200.0,
            })
        );
        assert_eq!(
            select_output(&layouts, 1919.0, 500.0).map(|output| output.id),
            Some(1)
        );
        assert_eq!(
            select_output(&layouts, 1920.0, 500.0).map(|output| output.id),
            Some(2)
        );
        assert_eq!(select_output(&layouts, 5000.0, 5000.0), None);
    }

    #[test]
    fn overlapping_outputs_use_the_same_deterministic_selection_for_compositing() {
        let layouts = vec![
            OutputLayout {
                id: 8,
                origin_x: 0,
                origin_y: 0,
                width: 1920,
                height: 1080,
            },
            OutputLayout {
                id: 4,
                origin_x: 0,
                origin_y: 0,
                width: 1920,
                height: 1080,
            },
        ];
        let mut core = positioned_core();
        core.pos = (400.0, 300.0);
        core.placed = true;
        let cores = CursorMap::from([("session".to_owned(), core)]);

        assert_eq!(select_output(&layouts, 400.0, 300.0).unwrap().id, 4);
        assert_eq!(visible_cores_for_output(&cores, &layouts, 4).len(), 1);
        assert!(visible_cores_for_output(&cores, &layouts, 8).is_empty());
    }

    #[test]
    fn a_cursor_on_a_monitor_left_of_the_origin_is_drawn_there() {
        // A left-hand monitor puts real cursor positions far below zero; they
        // must not be mistaken for the never-placed sentinel.
        let layouts = vec![
            OutputLayout {
                id: 3,
                origin_x: -3840,
                origin_y: 0,
                width: 3840,
                height: 2160,
            },
            OutputLayout {
                id: 4,
                origin_x: 0,
                origin_y: 0,
                width: 3840,
                height: 2160,
            },
        ];
        let mut core = positioned_core();
        core.pos = (-2220.0, 980.0);
        core.placed = true;
        let cores = CursorMap::from([("session".to_owned(), core)]);
        assert_eq!(visible_cores_for_output(&cores, &layouts, 3).len(), 1);
        assert!(visible_cores_for_output(&cores, &layouts, 4).is_empty());
        // And it paints there: a 160 px crop of the left monitor around it.
        let mut crop = tiny_skia::Pixmap::new(160, 160).unwrap();
        cursor_overlay::paint_cursor(&mut crop, &cores["session"], -2300.0, 900.0, None, 1.0);
        assert!(crop.pixels().iter().any(|pixel| pixel.alpha() > 0));
        let unplaced = CursorMap::from([(
            "session".to_owned(),
            RenderStateCore::new(CursorConfig::default()),
        )]);
        assert!(visible_cores_for_output(&unplaced, &layouts, 3).is_empty());
    }

    #[test]
    fn initial_configured_outputs_each_plan_one_transparent_frame() {
        let layouts = three_monitor_layout();
        let (selected, targets) = frame_plan(
            &layouts,
            &HashSet::new(),
            &HashSet::new(),
            std::iter::empty::<(f64, f64)>(),
        );

        assert!(selected.is_empty());
        assert_eq!(
            targets.iter().map(|target| target.id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn initialized_stable_outputs_plan_no_idle_frame() {
        let layouts = three_monitor_layout();
        let (selected, targets) = frame_plan(
            &layouts,
            &HashSet::new(),
            &initialized(&layouts),
            std::iter::empty::<(f64, f64)>(),
        );

        assert!(selected.is_empty());
        assert!(targets.is_empty());
    }

    #[test]
    fn hotplugged_output_plans_one_initial_transparent_frame() {
        let layouts = three_monitor_layout();
        let already_initialized = HashSet::from([1, 2]);
        let (selected, targets) = frame_plan(
            &layouts,
            &HashSet::new(),
            &already_initialized,
            std::iter::empty::<(f64, f64)>(),
        );

        assert!(selected.is_empty());
        assert_eq!(targets, vec![FrameTarget { id: 3 }]);
    }

    #[test]
    fn selected_output_combines_initialization_and_cursor_paint_in_one_frame() {
        let layouts = three_monitor_layout();
        let already_initialized = HashSet::from([1, 3]);
        let (selected, targets) = frame_plan(
            &layouts,
            &HashSet::new(),
            &already_initialized,
            Some((2500.0, 100.0)),
        );

        assert_eq!(selected, HashSet::from([2]));
        assert_eq!(targets, vec![FrameTarget { id: 2 }]);
    }

    #[test]
    fn crossing_outputs_clears_the_old_surface_and_paints_the_new_one() {
        let layouts = three_monitor_layout();
        let painted = HashSet::from([1]);
        let (selected, targets) = frame_plan(
            &layouts,
            &painted,
            &initialized(&layouts),
            Some((2500.0, 100.0)),
        );

        assert_eq!(selected, HashSet::from([2]));
        assert_eq!(targets, vec![FrameTarget { id: 1 }, FrameTarget { id: 2 }]);
    }

    #[test]
    fn effects_spilling_across_an_output_edge_select_the_neighbour() {
        let layouts = three_monitor_layout();
        let first = layouts[0];
        let right_edge = f64::from(first.origin_x) + f64::from(first.width);
        let top = f64::from(first.origin_y);

        // A trail straddling the edge between output 1 and its neighbour.
        let straddle = [right_edge - 100.0, top + 10.0, 200.0, 50.0];
        assert_eq!(
            effect_spill_outputs(&layouts, Some(straddle)),
            HashSet::from([1, 2]),
            "the neighbour output must be painted too"
        );

        // An effect wholly inside output 1 touches no other output, and
        // degenerate rects touch nothing.
        let inside = [f64::from(first.origin_x) + 10.0, top + 10.0, 50.0, 50.0];
        assert_eq!(
            effect_spill_outputs(&layouts, Some(inside)),
            HashSet::from([first.id])
        );
        assert!(effect_spill_outputs(&layouts, Some([0.0, 0.0, 0.0, 0.0])).is_empty());
        assert!(effect_spill_outputs(&layouts, std::iter::empty()).is_empty());
    }

    #[test]
    fn hide_or_session_removal_clears_every_previously_painted_output() {
        let layouts = three_monitor_layout();
        let painted = HashSet::from([1, 2, 3]);
        let (selected, targets) = frame_plan(
            &layouts,
            &painted,
            &initialized(&layouts),
            std::iter::empty::<(f64, f64)>(),
        );

        assert!(selected.is_empty());
        assert_eq!(targets.len(), 3);
        assert_eq!(
            targets.iter().map(|target| target.id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn fresh_sentinel_overlay_uses_only_topology_maintenance() {
        let layouts = three_monitor_layout();
        let core = RenderStateCore::new(CursorConfig::default());
        let cores = CursorMap::from([("default".to_owned(), core)]);
        assert_eq!(
            next_wait(&cores, false, false),
            WlWait::Maintenance(TOPOLOGY_MAINTENANCE_INTERVAL)
        );
        assert!(!any_core_needs_frame_tick(&cores));
        assert_eq!(
            next_wait(&cores, false, true),
            WlWait::Maintenance(Duration::ZERO)
        );
        assert!(frame_plan(
            &layouts,
            &HashSet::new(),
            &initialized(&layouts),
            std::iter::empty::<(f64, f64)>()
        )
        .1
        .is_empty());
    }

    #[test]
    fn stable_reduced_motion_overlay_sleeps_until_idle_fade_deadline() {
        let mut core = positioned_core();
        core.visual.reduced_motion = cursor_overlay::ReducedMotion::On;
        core.idle_secs = 0.25;
        let cores = CursorMap::from([("cursor-a".to_owned(), core)]);
        assert_eq!(
            next_wait(&cores, false, false),
            WlWait::Deadline(Duration::from_millis(750))
        );
        assert!(!any_core_needs_frame_tick(&cores));
    }

    // Before the shared predicate, a settled Wayland cursor slept until its
    // idle-fade deadline and its resting bob froze mid-swing.
    #[test]
    fn stable_visible_overlay_keeps_frame_cadence_for_resting_motion() {
        let mut core = positioned_core();
        core.idle_secs = 0.25;
        core.pos = (-1.0e9, -1.0e9);
        assert!(needs_frame_tick(&core));
        core.placed = false;
        assert!(!needs_frame_tick(&core));
        core.placed = true;
        assert!(core.has_resting_motion());
        let mut cores = CursorMap::from([("cursor-a".to_owned(), core)]);
        assert_eq!(next_wait(&cores, false, false), WlWait::Frame);

        // Hidden cursors never tick, even with resting motion enabled.
        cores.get_mut("cursor-a").unwrap().visible = false;
        assert!(!any_core_needs_frame_tick(&cores));
    }

    // Mirrors the hosted `wayland_overlay_quiesces_and_recovers_after_capture_
    // and_cursor_activity` certification: a never-hiding cursor rests still,
    // so the overlay parks on topology maintenance instead of redrawing.
    #[test]
    fn never_hiding_resting_overlay_parks_on_maintenance() {
        let mut core = positioned_core();
        core.motion.idle_hide_ms = 0.0;
        let cores = CursorMap::from([("cursor-a".to_owned(), core)]);
        assert!(!any_core_needs_frame_tick(&cores));
        assert_eq!(
            next_wait(&cores, false, false),
            WlWait::Maintenance(TOPOLOGY_MAINTENANCE_INTERVAL)
        );
    }

    #[test]
    fn first_positioned_command_seeds_inside_the_output_frame() {
        let mut render = WlRenderMap::new(CursorConfig::default(), ());
        let frame = Some(ScreenFrame::new(0.0, 0.0, 1920.0, 1080.0));
        assert!(apply_keyed_command(
            &mut render,
            frame,
            "session-a".to_owned(),
            OverlayCommand::MoveTo {
                x: 50.0,
                y: 50.0,
                end_heading_radians: 0.0,
                target: None,
            },
        ));
        let core = &render.cursors["session-a"];
        assert_eq!(core.pos, (2.0, 2.0));
        assert!(core.trajectory.is_some());
    }

    #[test]
    fn animation_and_fade_use_frame_cadence() {
        let mut core = positioned_core();
        core.click_t = Some(0.0);
        assert!(needs_frame_tick(&core));
        let mut cores = CursorMap::from([("cursor-a".to_owned(), core)]);
        assert_eq!(next_wait(&cores, false, false), WlWait::Frame);

        let core = cores.get_mut("cursor-a").unwrap();
        core.click_t = None;
        core.idle_secs = 1.0;
        core.idle_alpha = 1.0;
        assert!(needs_frame_tick(core));
        assert_eq!(next_wait(&cores, false, false), WlWait::Frame);
    }

    #[test]
    fn hidden_and_disabled_state_quiesces_deterministically() {
        let mut core = positioned_core();
        core.click_t = Some(0.25);
        core.visible = false;
        quiesce_hidden(&mut core);
        assert!(core.click_t.is_none());
        assert!(core.trajectory.is_none());
        let cores = CursorMap::from([("cursor-a".to_owned(), core)]);
        assert_eq!(
            next_wait(&cores, false, false),
            WlWait::Maintenance(TOPOLOGY_MAINTENANCE_INTERVAL)
        );
    }

    #[test]
    fn forward_mapping_preserves_named_cursor_keys() {
        let snap = message(OverlayCommand::SnapTo {
            x: 10.0,
            y: 20.0,
            heading_radians: None,
        });
        assert!(matches!(
            map_overlay_msg(&snap),
            Some(WlOverlayCmd::Cmd { key, .. }) if key == "test"
        ));
        assert!(matches!(
            map_overlay_msg(&OverlayMsg::Remove("session-a".to_owned())),
            Some(WlOverlayCmd::Remove(key)) if key == "session-a"
        ));
        assert!(matches!(
            map_overlay_msg(&OverlayMsg::Revive("session-a".to_owned())),
            Some(WlOverlayCmd::Revive(key)) if key == "session-a"
        ));
    }

    fn snap_command(key: &str) -> WlOverlayCmd {
        WlOverlayCmd::Cmd {
            key: key.to_owned(),
            cmd: OverlayCommand::SnapTo {
                x: 100.0,
                y: 100.0,
                heading_radians: None,
            },
        }
    }

    #[test]
    fn cold_glide_defaults_reach_the_owner_without_connecting_to_the_compositor() {
        let (tx, rx) = bounded(8);
        let mut cfg = CursorConfig::default();
        cfg.motion.glide_duration_ms = 120.0;
        let mut state = OverlayState::new(cfg);
        assert_eq!(
            state.render.motion_for_key("future").glide_duration_ms,
            120.0
        );
        state.render.cursor_mut("existing").unwrap();
        let mut explicit = state.render.motion_for_key("explicit");
        explicit.glide_duration_explicit = true;
        explicit.glide_duration_ms = 40.0;
        tx.send(WlOverlayCmd::Cmd {
            key: "explicit".into(),
            cmd: OverlayCommand::SetMotion(explicit),
        })
        .unwrap();
        for (session, value) in [(Some("scoped"), 250.0), (None, 300.0)] {
            tx.send(WlOverlayCmd::SetDefaultGlideDuration {
                session: session.map(str::to_owned),
                value,
            })
            .unwrap();
        }
        tx.send(WlOverlayCmd::Shutdown).unwrap();
        assert!(wait_for_renderer_command(&rx, &mut state).is_none());
        assert_eq!(
            state.render.motion_for_key("existing").glide_duration_ms,
            300.0
        );
        assert_eq!(
            state.render.motion_for_key("future").glide_duration_ms,
            300.0
        );
        assert_eq!(
            state.render.motion_for_key("scoped").glide_duration_ms,
            250.0
        );
        assert_eq!(
            state.render.motion_for_key("explicit").glide_duration_ms,
            40.0
        );
        assert!(state.outputs.is_empty());
    }

    #[test]
    fn cold_remove_suppresses_stale_commands_without_renderer_startup() {
        let (tx, rx) = bounded(8);
        tx.send(WlOverlayCmd::Revive("session-a".to_owned()))
            .unwrap();
        tx.send(WlOverlayCmd::Remove("session-a".to_owned()))
            .unwrap();
        tx.send(snap_command("session-a")).unwrap();
        tx.send(WlOverlayCmd::Shutdown).unwrap();

        let mut state = OverlayState::new(CursorConfig::default());
        assert!(wait_for_renderer_command(&rx, &mut state).is_none());
        assert!(state.render.ended.contains("session-a"));
        assert!(!state.render.cursors.contains_key("session-a"));
        assert!(state.outputs.is_empty());
    }

    fn cold_metadata(key: &str) -> Vec<WlOverlayCmd> {
        use cursor_overlay::CursorAction;
        [
            OverlayCommand::SetSessionLabel("Synthetic session".into()),
            OverlayCommand::SetEnabled(true),
            OverlayCommand::PinAbove(42),
            OverlayCommand::SetPressed(false),
            OverlayCommand::BeginAction {
                action: CursorAction::Observe,
                delivery: None,
                target: None,
            },
            OverlayCommand::EndAction(CursorAction::Observe),
            OverlayCommand::BeginAction {
                action: CursorAction::Drag,
                delivery: None,
                target: None,
            },
            OverlayCommand::EndAction(CursorAction::Drag),
        ]
        .into_iter()
        .map(|cmd| WlOverlayCmd::Cmd {
            key: key.to_owned(),
            cmd,
        })
        .collect()
    }

    #[test]
    fn cold_observation_and_refused_action_metadata_never_connects_to_wayland() {
        for end_session in [false, true] {
            let (tx, rx) = bounded(16);
            for command in cold_metadata("session-a") {
                tx.send(command).unwrap();
            }
            if end_session {
                tx.send(WlOverlayCmd::Remove("session-a".to_owned()))
                    .unwrap();
            }
            drop(tx);
            // The real owner must finish without connecting, even when the
            // process exits with observation/action metadata still queued.
            assert!(owner_thread(rx).is_ok());
        }
    }

    #[test]
    fn cold_metadata_is_retained_before_first_positioned_command() {
        let (tx, rx) = bounded(16);
        for command in cold_metadata("session-a") {
            tx.send(command).unwrap();
        }
        tx.send(snap_command("session-a")).unwrap();
        tx.send(WlOverlayCmd::Remove("session-a".to_owned()))
            .unwrap();
        drop(tx);
        let mut state = OverlayState::new(CursorConfig::default());
        assert!(matches!(
            wait_for_renderer_command(&rx, &mut state),
            Some(WlOverlayCmd::Cmd {
                cmd: OverlayCommand::SnapTo { .. },
                ..
            })
        ));
        let core = state.render.cursors.get("session-a").unwrap();
        assert_eq!(core.session_label.as_deref(), Some("Synthetic session"));
        assert!(!core.placed);
        assert!(state.outputs.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Ok(WlOverlayCmd::Remove(key)) if key == "session-a"
        ));
    }

    #[test]
    fn cold_revive_allows_startup_and_preserves_other_tombstones_and_queue_order() {
        let (tx, rx) = bounded(8);
        tx.send(WlOverlayCmd::Remove("session-a".to_owned()))
            .unwrap();
        tx.send(WlOverlayCmd::Remove("session-b".to_owned()))
            .unwrap();
        tx.send(snap_command("session-a")).unwrap();
        tx.send(WlOverlayCmd::Revive("session-a".to_owned()))
            .unwrap();
        tx.send(snap_command("session-a")).unwrap();
        tx.send(WlOverlayCmd::Remove("session-a".to_owned()))
            .unwrap();
        tx.send(snap_command("session-a")).unwrap();
        drop(tx);

        let mut state = OverlayState::new(CursorConfig::default());
        assert!(matches!(
            wait_for_renderer_command(&rx, &mut state),
            Some(WlOverlayCmd::Cmd { key, .. }) if key == "session-a"
        ));
        assert!(!state.render.ended.contains("session-a"));
        assert!(state.render.ended.contains("session-b"));
        // Startup must not consume the later Remove or reorder it behind the
        // stale command. The active loop receives both in their original order.
        assert!(matches!(
            rx.try_recv(),
            Ok(WlOverlayCmd::Remove(key)) if key == "session-a"
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(WlOverlayCmd::Cmd { key, .. }) if key == "session-a"
        ));
    }

    #[test]
    fn cold_fresh_key_starts_with_ended_session_still_suppressed() {
        let (tx, rx) = bounded(4);
        tx.send(WlOverlayCmd::Remove("session-a".to_owned()))
            .unwrap();
        tx.send(snap_command("session-a")).unwrap();
        tx.send(snap_command("session-b")).unwrap();
        drop(tx);

        let mut state = OverlayState::new(CursorConfig::default());
        let Some(WlOverlayCmd::Cmd { key, cmd }) = wait_for_renderer_command(&rx, &mut state)
        else {
            panic!("fresh session should start the renderer");
        };
        assert_eq!(key, "session-b");
        assert!(apply_keyed_command(&mut state.render, None, key, cmd,));
        assert!(state.render.cursors.contains_key("session-b"));
        let WlOverlayCmd::Cmd { key, cmd } = snap_command("session-a") else {
            unreachable!();
        };
        assert!(!apply_keyed_command(&mut state.render, None, key, cmd,));
        assert!(!state.render.cursors.contains_key("session-a"));
    }

    #[test]
    fn cold_shutdown_and_channel_close_exit_owner_without_wayland() {
        for shutdown in [false, true] {
            let (tx, rx) = bounded(4);
            tx.send(WlOverlayCmd::Remove("session-a".to_owned()))
                .unwrap();
            tx.send(WlOverlayCmd::Revive("session-a".to_owned()))
                .unwrap();
            if shutdown {
                tx.send(WlOverlayCmd::Shutdown).unwrap();
            }
            drop(tx);
            // Exercise the actual owner entry point: it must return before
            // attempting Connection::connect_to_env, even without a compositor.
            assert!(owner_thread(rx).is_ok());
        }
    }

    #[test]
    fn named_cursors_render_on_independent_outputs_and_removal_clears_only_one() {
        let mut render = WlRenderMap::new(CursorConfig::default(), ());
        assert!(apply_keyed_command(
            &mut render,
            None,
            "session-a".to_owned(),
            OverlayCommand::SnapTo {
                x: 100.0,
                y: 100.0,
                heading_radians: None,
            },
        ));
        assert!(apply_keyed_command(
            &mut render,
            None,
            "session-b".to_owned(),
            OverlayCommand::SnapTo {
                x: 2500.0,
                y: 100.0,
                heading_radians: None,
            },
        ));
        assert_eq!(render.cursors["session-a"].cfg.cursor_id, "session-a");
        assert_eq!(render.cursors["session-b"].cfg.cursor_id, "session-b");

        let layouts = three_monitor_layout();
        assert_eq!(
            visible_cores_for_output(&render.cursors, &layouts, 1)
                .into_iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            vec!["session-a"]
        );
        assert_eq!(
            visible_cores_for_output(&render.cursors, &layouts, 2)
                .into_iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            vec!["session-b"]
        );
        let (painted, targets) = frame_plan(
            &layouts,
            &HashSet::new(),
            &initialized(&layouts),
            render
                .cursors
                .values()
                .filter(|core| core.placed)
                .map(|core| core.pos),
        );
        assert_eq!(painted, HashSet::from([1, 2]));
        assert_eq!(targets, vec![FrameTarget { id: 1 }, FrameTarget { id: 2 }]);

        assert!(remove_keyed_core(&mut render, "session-a".to_owned()));
        assert!(!render.cursors.contains_key("session-a"));
        assert!(render.cursors.contains_key("session-b"));
        assert!(visible_cores_for_output(&render.cursors, &layouts, 1).is_empty());
        assert_eq!(
            visible_cores_for_output(&render.cursors, &layouts, 2)
                .into_iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            vec!["session-b"]
        );
        let (selected, targets) = frame_plan(
            &layouts,
            &painted,
            &initialized(&layouts),
            render
                .cursors
                .values()
                .filter(|core| core.placed)
                .map(|core| core.pos),
        );
        assert_eq!(selected, HashSet::from([2]));
        assert_eq!(targets, vec![FrameTarget { id: 1 }, FrameTarget { id: 2 }]);

        // A queued command cannot resurrect an ended named session.
        assert!(!apply_keyed_command(
            &mut render,
            None,
            "session-a".to_owned(),
            OverlayCommand::SnapTo {
                x: 200.0,
                y: 200.0,
                heading_radians: None,
            },
        ));
        assert!(!render.cursors.contains_key("session-a"));

        revive_key(&mut render, "session-a".to_owned());
        assert!(!render.ended.contains("session-a"));
        assert!(apply_keyed_command(
            &mut render,
            None,
            "session-a".to_owned(),
            OverlayCommand::SnapTo {
                x: 200.0,
                y: 200.0,
                heading_radians: None,
            },
        ));
        assert!(render.cursors.contains_key("session-a"));
    }

    #[test]
    fn named_cursors_schedule_animation_and_idle_deadlines_independently() {
        let mut idle = positioned_core();
        idle.visual.reduced_motion = cursor_overlay::ReducedMotion::On;
        idle.idle_secs = 0.25;
        let mut animated = positioned_core();
        animated.visual.reduced_motion = cursor_overlay::ReducedMotion::On;
        animated.motion.idle_hide_ms = 4_000.0;
        animated.click_t = Some(0.0);
        let mut cores =
            CursorMap::from([("idle".to_owned(), idle), ("animated".to_owned(), animated)]);

        assert_eq!(next_wait(&cores, false, false), WlWait::Frame);
        cores.get_mut("animated").unwrap().click_t = None;
        assert_eq!(
            next_wait(&cores, false, false),
            WlWait::Deadline(Duration::from_millis(750))
        );
    }

    #[test]
    fn maintenance_scheduler_wakes_immediately_on_command_arrival() {
        let (tx, rx) = bounded(1);
        tx.send(WlOverlayCmd::Remove("test".to_owned())).unwrap();
        assert!(matches!(
            wait_for_work(&rx, WlWait::Maintenance(Duration::from_secs(1))),
            WlWake::Command(WlOverlayCmd::Remove(key)) if key == "test"
        ));
    }

    #[test]
    fn disabled_config_refuses_forwarding_and_thread_startup() {
        CONFIG_ENABLED.store(false, Ordering::Release);
        let msg = message(OverlayCommand::SnapTo {
            x: 10.0,
            y: 20.0,
            heading_radians: None,
        });
        let had_thread = OWNER
            .get()
            .is_some_and(|registry| registry.state.lock().unwrap().owner.is_some());
        assert!(!should_forward(false, &msg));
        assert!(!should_forward(
            false,
            &OverlayMsg::Remove("session-a".to_owned())
        ));
        assert!(!should_forward(
            false,
            &OverlayMsg::Revive("session-a".to_owned())
        ));
        assert!(!ensure_started());
        assert_eq!(
            OWNER
                .get()
                .is_some_and(|registry| registry.state.lock().unwrap().owner.is_some()),
            had_thread
        );
    }
}
