#!/usr/bin/env python3
# Transforms wlroots' upstream `tinywl/tinywl.c` (0.19) into `cua-compositor.c`:
# a minimal headless wlroots compositor that cua-driver spawns as its nested
# session (CUA_WAYLAND_NEST_COMPOSITOR=cua-compositor) to perform what stock
# Wayland forbids a client from doing —
#
#   * focus-FREE per-surface KEYBOARD injection (type into an unfocused window),
#   * MULTI-cursor pointer injection (N independent cursors on one window),
#
# both routed by an exact compositor-owned surface token, driven over a tiny line
# protocol on a unix control socket ($CUA_INJECT_SOCKET). It also exposes
# foreign-toplevel-management (so cua-driver's list_windows enumerates windows)
# and screencopy (so grim captures the output) — the same protocols labwc gives
# the non-nested cells — so the nested-injection cells keep working end to end.
#
# The injection primitives route by a compositor-instance epoch plus toplevel id.
# Logical pointer/keyboard indices above zero deliver directly to the exact target;
# device zero uses wlroots seat notification only where Chromium requires a
# protocol-complete pointer or already-focused keyboard path. This is the private
# compositor seat and never broadens target selection. Commands travel over a plain
# socket rather than libei/EIS, since cua owns both ends and the portal/libei layer
# buys nothing here. The socket speaks a versioned v2 line protocol: the
# client sends the `cua-inject v2` banner (echoed back on match), then every
# command line is answered by exactly one `ok` / `err <reason>` acknowledgement.
#
# Usage: cua_compositor_patch.py <tinywl.c in> <cua-compositor.c out>
import sys
import io

inp = sys.argv[1] if len(sys.argv) > 1 else "tinywl.c"
out = sys.argv[2] if len(sys.argv) > 2 else "cua-compositor.c"
src = io.open(inp, encoding="utf-8").read()

# memfd_create + SOCK_CLOEXEC etc. need _GNU_SOURCE before any system header.
src = "#define _GNU_SOURCE\n" + src

INCLUDES = r"""#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <unistd.h>
#include <errno.h>
#include <time.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/random.h>
#include <sys/stat.h>
#include <linux/input-event-codes.h>
#include <wayland-server-protocol.h>
#include <xkbcommon/xkbcommon.h>
#include <wlr/interfaces/wlr_keyboard.h>
#include <wlr/types/wlr_foreign_toplevel_management_v1.h>
#include <wlr/types/wlr_screencopy_v1.h>
#include <wlr/types/wlr_xdg_output_v1.h>

#define CUA_MAXDEV 64
#define CUA_MAX_CLICK_COUNT 3
#define CUA_MAX_PRESSED_BUTTONS 8
struct tinywl_server;
struct tinywl_toplevel;
struct cua_conn {
	struct tinywl_server *server;
	struct wl_event_source *src;
	struct wl_event_source *capture_timer;
	struct tinywl_toplevel *action_target;
	uint64_t capture_lease;
	int desktop_batch;
	int hello;
	char buf[16384];
	size_t len;
};
static void cua_assign_target(struct tinywl_toplevel *t);
static void cua_ftl_request_activate(struct wl_listener *listener, void *data);
/* Per-cursor / per-keyboard enter bookkeeping (idx = logical device). */
struct cua_devstate {
	struct wlr_surface *entered;
	struct tinywl_toplevel *entered_target;
	struct tinywl_server *pointer_server;
	int pointer_idx;
	uint32_t pressed_buttons[CUA_MAX_PRESSED_BUTTONS];
	size_t pressed_count;
	struct wl_listener surface_destroy;
	int surface_destroy_linked;
};
static struct cua_devstate cua_ptr[CUA_MAXDEV];
static struct cua_conn *cua_ptr_owner[CUA_MAXDEV];
static struct tinywl_toplevel *cua_ptr_target[CUA_MAXDEV];
static struct cua_devstate cua_kbd_state[CUA_MAXDEV];
static int g_keymap_fd = -1;
static size_t g_keymap_size = 0;
static struct wlr_keyboard g_keyboard;
static xkb_mod_mask_t g_shift_mask = 1;
static xkb_mod_mask_t g_ctrl_mask = 0;
static xkb_mod_mask_t g_alt_mask = 0;
static xkb_mod_mask_t g_logo_mask = 0;
struct cua_keyent { uint32_t keycode; int shift; int valid; };
static struct cua_keyent g_chartab[128];
static struct wlr_foreign_toplevel_manager_v1 *g_ftl_mgr = NULL;
static uint64_t g_cua_epoch = 0;
static uint64_t g_cua_next_id = 1;
static struct cua_conn *g_capture_owner = NULL;
static struct tinywl_toplevel *g_capture_target = NULL;
static void cua_ftl_request_activate(struct wl_listener *listener, void *data);
static void cua_maybe_focus_new_toplevel(struct tinywl_toplevel *toplevel);
static void cua_capture_restore(struct cua_conn *c);
static void cua_devstate_set_surface(struct cua_devstate *state, struct wlr_surface *surface);
static void cua_ptr_set_surface(struct tinywl_server *server, int idx,
	struct wlr_surface *surface, struct tinywl_toplevel *target);
static void cua_ptr_release_index(struct tinywl_server *server, int idx);
static pid_t cua_toplevel_pid(struct tinywl_toplevel *t);
static bool cua_pid_in_family(pid_t pid, pid_t root_pid);

"""

# A foreign-toplevel handle pointer on each toplevel (for list_windows).
STRUCT_FIELD = (
    "\tstruct wlr_xdg_toplevel *xdg_toplevel;\n"
    "\tuint64_t cua_id;\n"
    "\tchar cua_target[96];\n"
    "\tstruct cua_conn *cua_action_owner;\n"
    "\tbool cua_initial_activation_sent;\n"
    "\tstruct wlr_foreign_toplevel_handle_v1 *ftl;\n"
    "\tstruct wl_listener ftl_request_activate;\n"
)

FUNCS = r"""
/* v2 requires an exact compositor-owned toplevel token on every mutating
 * target command. The token includes this compositor instance's epoch. */
#define CUA_PROTO_HELLO "cua-inject v2"
struct cua_conn;

static uint64_t g_capture_lease_seq = 1;
static void cua_focus_toplevel(struct tinywl_toplevel *toplevel) {
	focus_toplevel(toplevel);
	/* tinywl only notifies seat keyboard focus when a physical wlr_keyboard is
	 * attached. Headless CI has none, so explicit activation establishes the
	 * logical focus without coupling it to the initial map/configure handshake. */
	struct wlr_surface *surface = toplevel->xdg_toplevel->base->surface;
	if (toplevel->server->seat->keyboard_state.focused_surface != surface) {
		struct wlr_keyboard_modifiers modifiers = {0};
		wlr_seat_keyboard_notify_enter(
			toplevel->server->seat, surface, NULL, 0, &modifiers);
	}
}
/* New child toplevels may request focus as part of their normal map sequence.
 * Preserve that behavior only when the current keyboard focus belongs to the
 * same process family. A background application opening a child must not steal
 * focus from the foreground sentinel (or from any other application). */
static void cua_maybe_focus_new_toplevel(struct tinywl_toplevel *toplevel) {
	struct wlr_surface *focused = toplevel->server->seat->keyboard_state.focused_surface;
	struct wlr_surface *focused_root = focused ? wlr_surface_get_root_surface(focused) : NULL;
	if (!focused_root) {
		cua_focus_toplevel(toplevel);
		return;
	}
	struct tinywl_toplevel *current;
	wl_list_for_each(current, &toplevel->server->toplevels, link) {
		struct wlr_surface *surface = current->xdg_toplevel ? current->xdg_toplevel->base->surface : NULL;
		if (surface != focused_root) continue;
		pid_t requested_pid = cua_toplevel_pid(toplevel);
		pid_t focused_pid = cua_toplevel_pid(current);
		if (requested_pid == focused_pid ||
			cua_pid_in_family(requested_pid, focused_pid) ||
			cua_pid_in_family(focused_pid, requested_pid))
			cua_focus_toplevel(toplevel);
		return;
	}
}
static void cua_ftl_request_activate(struct wl_listener *listener, void *data) {
	(void)data;
	struct tinywl_toplevel *t = wl_container_of(listener, t, ftl_request_activate);
	cua_focus_toplevel(t);
}
static uint32_t cua_now_ms(void) {
	struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint32_t)(ts.tv_sec * 1000 + ts.tv_nsec / 1000000);
}
/* Write a single acknowledgement line back to the control client. Best-effort:
 * a dead peer is torn down by the read side on the next loop iteration. */
static void cua_reply(int fd, const char *line) {
	char buf[256];
	int n = snprintf(buf, sizeof buf, "%s\n", line);
	if (n < 0) return;
	if (n >= (int)sizeof buf) n = (int)sizeof buf - 1;
	ssize_t off = 0;
	while (off < n) {
		ssize_t w = write(fd, buf + off, (size_t)n - (size_t)off);
		if (w <= 0) break;
		off += w;
	}
}
static void cua_pframe(struct wl_resource *res) {
	if (wl_resource_get_version(res) >= WL_POINTER_FRAME_SINCE_VERSION)
		wl_pointer_send_frame(res);
}
static pid_t cua_toplevel_pid(struct tinywl_toplevel *t);
static bool cua_pid_in_family(pid_t pid, pid_t root_pid) {
	if (pid <= 0 || root_pid <= 0) return false;
	for (int depth = 0; depth < 64 && pid > 1; depth++) {
		if (pid == root_pid) return true;
		char path[64], stat[4096];
		snprintf(path, sizeof path, "/proc/%d/stat", (int)pid);
		FILE *file = fopen(path, "r");
		if (!file) return false;
		size_t n = fread(stat, 1, sizeof stat - 1, file);
		fclose(file);
		if (!n) return false;
		stat[n] = '\0';
		/* comm is parenthesized and may contain spaces or ')', so parse the
		 * state + parent pid after its final closing parenthesis. */
		char *close = strrchr(stat, ')'), state = '\0';
		long parent = 0;
		if (!close || sscanf(close + 2, "%c %ld", &state, &parent) != 2 ||
			parent <= 0 || parent == pid) return false;
		pid = (pid_t)parent;
	}
	return pid == root_pid;
}
static void cua_init_epoch(void) {
	if (g_cua_epoch) return;
	if (getrandom(&g_cua_epoch, sizeof g_cua_epoch, 0) != sizeof g_cua_epoch)
		g_cua_epoch = ((uint64_t)time(NULL) << 32) ^ (uint64_t)getpid();
	if (!g_cua_epoch) g_cua_epoch = 1;
}
static void cua_assign_target(struct tinywl_toplevel *t) {
	if (t->cua_id) return;
	cua_init_epoch();
	t->cua_id = g_cua_next_id++;
	snprintf(t->cua_target, sizeof t->cua_target, "surface:%016llx:%016llx",
		(unsigned long long)g_cua_epoch, (unsigned long long)t->cua_id);
}
/* Resolve only the exact v2 token. PID, app-id, title, and newest-window
 * fallbacks are deliberately unsupported because one browser process may own
 * several independently controlled toplevels. */
static struct tinywl_toplevel *cua_resolve_target(struct tinywl_server *server, const char *target, const char **err) {
	struct tinywl_toplevel *t;
	if (strncmp(target, "surface:", 8)) { *err = "exact-target-required"; return NULL; }
	wl_list_for_each(t, &server->toplevels, link) {
		if (t->cua_target[0] && !strcmp(t->cua_target, target)) return t;
	}
	*err = "stale-or-unknown-target";
	return NULL;
}
static pid_t cua_toplevel_pid(struct tinywl_toplevel *t) {
	if (!t || !t->xdg_toplevel || !t->xdg_toplevel->base->surface) return 0;
	struct wl_client *client = wl_resource_get_client(t->xdg_toplevel->base->surface->resource);
	pid_t pid = 0; uid_t uid = 0; gid_t gid = 0;
	wl_client_get_credentials(client, &pid, &uid, &gid);
	return pid;
}
static const char *cua_activate_target(struct tinywl_server *server, const char *target) {
	const char *err = NULL;
	struct tinywl_toplevel *found = cua_resolve_target(server, target, &err);
	if (!found) return err;
	cua_focus_toplevel(found);
	return NULL;
}
/* Independent observer query used only by the Rust E2E testkit. The target is
 * selected by the Wayland client's process credentials, not by driver-owned
 * object ids. In this minimal compositor every mapped toplevel shares origin;
 * a non-focused target beneath another focused surface is therefore occluded. */
static void cua_query_state(struct tinywl_server *server, pid_t target_pid, char *out, size_t out_len) {
	struct wlr_surface *focused = server->seat->keyboard_state.focused_surface;
	struct wlr_surface *focused_root = focused ? wlr_surface_get_root_surface(focused) : NULL;
	struct tinywl_toplevel *t, *target = NULL, *focused_toplevel = NULL;
	wl_list_for_each(t, &server->toplevels, link) {
		struct wlr_surface *surface = t->xdg_toplevel ? t->xdg_toplevel->base->surface : NULL;
		if (surface == focused_root) focused_toplevel = t;
		if (target_pid > 0 && cua_toplevel_pid(t) == target_pid) target = t;
	}
	pid_t focused_pid = cua_toplevel_pid(focused_toplevel);
	const char *state = !target ? "not_found" :
		(target == focused_toplevel ? "foreground" :
		 (focused_toplevel ? "background_occluded" : "background_visible"));
	snprintf(out, out_len, "state %d %s", (int)focused_pid, state);
}
static const char *cua_query_geometry(struct tinywl_server *server, const char *selector, char *out, size_t out_len) {
	struct tinywl_toplevel *t, *target = NULL;
	int matches = 0;
	if (!strncmp(selector, "surface:", 8)) {
		const char *err = NULL;
		target = cua_resolve_target(server, selector, &err);
		if (!target) return err;
		matches = 1;
	} else {
		char *end = NULL; long target_pid = strtol(selector, &end, 10);
		if (target_pid <= 0 || !end || *end) return "exact-target-or-pid-required";
		wl_list_for_each(t, &server->toplevels, link) {
			/* Electron exposes accessibility under the browser root while its
			 * xdg_toplevel can be owned by a descendant GPU/renderer client. This
			 * PID selector is read-only observer compatibility; mutating v2 commands
			 * still require an exact compositor-owned surface token. */
			if (cua_pid_in_family(cua_toplevel_pid(t), target_pid)) { target = t; matches++; }
		}
	}
	if (!matches) return "target-not-found";
	if (matches > 1) return "ambiguous-pid";
	int x = 0, y = 0;
	if (!wlr_scene_node_coords(&target->scene_tree->node, &x, &y)) return "unmapped-target";
	/* AT-SPI Window coordinates for native GTK are rooted at the xdg window
	 * geometry, while scene coordinates and screencopy include the complete root
	 * surface (including client-side decorations). Rebase into that coordinate
	 * system so `origin + accessible_window_xy` lands on the captured pixel. */
	int scene_x = x, scene_y = y;
	struct wlr_box geo = target->xdg_toplevel->base->geometry;
	x -= geo.x; y -= geo.y;
	snprintf(out, out_len, "geometry %d %d %d %d", x, y, scene_x, scene_y);
	return NULL;
}
static void cua_devstate_surface_destroy(struct wl_listener *listener, void *data) {
	(void)data;
	struct cua_devstate *state = wl_container_of(listener, state, surface_destroy);
	/* The wl_surface is still valid while its destroy signal is emitted. Release
	 * any synthetic buttons to that original client before dropping identity;
	 * wlroots clears seat focus first without clearing button bookkeeping, so
	 * cua_ptr_release_index reconciles the seat then directly delivers release. */
	if (state->pointer_server)
		cua_ptr_release_index(state->pointer_server, state->pointer_idx);
	if (state->surface_destroy_linked) wl_list_remove(&state->surface_destroy.link);
	state->surface_destroy_linked = 0;
	state->entered = NULL;
	state->entered_target = NULL;
	state->pointer_server = NULL;
}
static void cua_devstate_set_surface(struct cua_devstate *state, struct wlr_surface *surface) {
	if (state->entered == surface) return;
	if (state->surface_destroy_linked) wl_list_remove(&state->surface_destroy.link);
	state->surface_destroy_linked = 0;
	state->entered = surface;
	state->entered_target = NULL;
	if (!surface) return;
	state->surface_destroy.notify = cua_devstate_surface_destroy;
	wl_signal_add(&surface->events.destroy, &state->surface_destroy);
	state->surface_destroy_linked = 1;
}
static void cua_ptr_set_surface(struct tinywl_server *server, int idx,
		struct wlr_surface *surface, struct tinywl_toplevel *target) {
	cua_ptr[idx].pointer_server = surface ? server : NULL;
	cua_ptr[idx].pointer_idx = idx;
	cua_devstate_set_surface(&cua_ptr[idx], surface);
	cua_ptr[idx].entered_target = surface ? target : NULL;
}
static void cua_ptr_leave(struct wlr_seat *seat, struct wlr_surface *surf) {
	if (!surf) return;
	struct wlr_seat_client *sc = wlr_seat_client_for_wl_client(seat, wl_resource_get_client(surf->resource));
	if (!sc) return;
	uint32_t serial = wlr_seat_client_next_serial(sc);
	struct wl_resource *res;
	wl_resource_for_each(res, &sc->pointers) { wl_pointer_send_leave(res, serial, surf->resource); cua_pframe(res); }
}
/* A logical pointer is compositor-global, so one action connection must own it
 * for the whole batch. Different exact targets may run concurrently only on
 * different logical indices; an interleaving batch fails closed instead of
 * inheriting another target's entered surface or wlroots seat pointer focus. */
static bool cua_ptr_claim(struct cua_conn *c, struct tinywl_toplevel *t, int idx) {
	if (!c || !t || idx < 0 || idx >= CUA_MAXDEV) return false;
	if (cua_ptr_owner[idx] && cua_ptr_owner[idx] != c) return false;
	if (cua_ptr_target[idx] && cua_ptr_target[idx] != t) return false;
	cua_ptr_owner[idx] = c;
	cua_ptr_target[idx] = t;
	return true;
}
static bool cua_ptr_owned(struct cua_conn *c, struct tinywl_toplevel *t, int idx) {
	if (!c || !t || idx < 0 || idx >= CUA_MAXDEV ||
		cua_ptr_owner[idx] != c || cua_ptr_target[idx] != t || !cua_ptr[idx].entered ||
		cua_ptr[idx].entered_target != t)
		return false;
	return true;
}
static void cua_ptr_release(struct cua_conn *c) {
	for (int idx = 0; idx < CUA_MAXDEV; idx++) {
		if (cua_ptr_owner[idx] != c) continue;
		cua_ptr_release_index(c->server, idx);
	}
}
/* Inject pointer motion from logical cursor `idx` into window `t` at window-
 * local (x,y). enter/leave is tracked per idx, so several idx values can drive
 * independent cursors against the same or different surfaces. Device zero may
 * update this private compositor's pointer focus for protocol-complete Chromium
 * delivery; it does not change keyboard focus, activate another toplevel, move a
 * host cursor, or weaken the exact v2 surface selection. */
static bool cua_motion(struct tinywl_server *server, struct cua_conn *c, struct tinywl_toplevel *t, int idx, double x, double y) {
	if (!cua_ptr_claim(c, t, idx)) return false;
	/* Public PX coordinates come from the cropped root-surface screenshot. Hit
	 * test that point through the scene so Chromium/WebKit child surfaces receive
	 * enter/motion in their own local coordinates instead of the top-level root. */
	int scene_x = 0, scene_y = 0;
	if (!wlr_scene_node_coords(&t->scene_tree->node, &scene_x, &scene_y)) return false;
	double local_x = 0, local_y = 0;
	/* Search only the requested toplevel's scene subtree. A global hit test
	 * would select the foreground sentinel when this target is occluded. */
	struct wlr_scene_node *node = wlr_scene_node_at(&t->scene_tree->node,
		scene_x + x, scene_y + y, &local_x, &local_y);
	if (!node || node->type != WLR_SCENE_NODE_BUFFER) return false;
	struct wlr_scene_buffer *buffer = wlr_scene_buffer_from_node(node);
	struct wlr_scene_surface *scene_surface = wlr_scene_surface_try_from_buffer(buffer);
	if (!scene_surface) return false;
	struct wlr_surface *surface = scene_surface->surface;
	struct wlr_seat_client *sc = wlr_seat_client_for_wl_client(server->seat, wl_resource_get_client(surface->resource));
	if (!sc || wl_list_empty(&sc->pointers)) {
		/* Chromium can compose a renderer-owned child surface whose wl_client
		 * never bound wl_pointer while the owning toplevel client did. Use that
		 * target root while retaining the caller's screenshot-local point. */
		surface = t->xdg_toplevel->base->surface;
		local_x = x; local_y = y;
		sc = wlr_seat_client_for_wl_client(server->seat, wl_resource_get_client(surface->resource));
		if (!sc || wl_list_empty(&sc->pointers)) return false;
	}
	/* wlroots resets button bookkeeping when pointer focus changes. Keep an
	 * active synthetic press pinned to its exact child/root surface; a drag that
	 * crosses a popup/subsurface boundary fails closed and batch cleanup releases
	 * the original client instead of silently stranding its pressed state. */
	if (cua_ptr[idx].pressed_count && cua_ptr[idx].entered != surface) return false;
	/* Chromium consumes pointer input through wlroots' seat pointer state. Raw
	 * wl_pointer resource sends are sufficient for GTK, but Chromium can ACK
	 * them without dispatching DOM mouse events because the compositor-side
	 * focus/grab state was never updated. Device 0 is the normal single-pointer
	 * route, so use the protocol-complete seat notifications there. Higher
	 * logical device indices retain direct delivery for independent cursors. */
	if (idx == 0) {
		wlr_seat_pointer_notify_enter(server->seat, surface, local_x, local_y);
		wlr_seat_pointer_notify_motion(server->seat, cua_now_ms(), local_x, local_y);
		/* Real cursors emit a separate frame event after the motion callback.
		 * Synthetic commands have no cursor-frame signal, so terminate the
		 * protocol batch here; Chromium buffers motion/button events until it. */
		wlr_seat_pointer_notify_frame(server->seat);
		cua_ptr_set_surface(server, idx, surface, t);
		return true;
	}
	wl_fixed_t sx = wl_fixed_from_double(local_x), sy = wl_fixed_from_double(local_y);
	struct wl_resource *res;
	if (cua_ptr[idx].entered != surface) {
		if (cua_ptr[idx].entered) cua_ptr_leave(server->seat, cua_ptr[idx].entered);
		uint32_t es = wlr_seat_client_next_serial(sc);
		wl_resource_for_each(res, &sc->pointers) { wl_pointer_send_enter(res, es, surface->resource, sx, sy); cua_pframe(res); }
		cua_ptr_set_surface(server, idx, surface, t);
	}
	uint32_t tm = cua_now_ms();
	wl_resource_for_each(res, &sc->pointers) { wl_pointer_send_motion(res, tm, sx, sy); cua_pframe(res); }
	return true;
}
/* Resolve an output-layout point through the compositor scene, preserving
 * subsurface offsets and output scaling. This is the desktop-scope path. */
static struct tinywl_toplevel *cua_desktop_motion(struct tinywl_server *server, struct cua_conn *c, double x, double y) {
	double sx = 0, sy = 0;
	struct wlr_surface *surface = NULL;
	struct tinywl_toplevel *t = desktop_toplevel_at(server, x, y, &surface, &sx, &sy);
	if (!t || !surface) return NULL;
	if (!cua_ptr_claim(c, t, 0)) return NULL;
	struct wlr_seat_client *sc = wlr_seat_client_for_wl_client(server->seat, wl_resource_get_client(surface->resource));
	if (!sc || wl_list_empty(&sc->pointers)) {
		/* Match exact-target motion: renderer-owned child surfaces may not have
		 * bound wl_pointer even though the owning toplevel client has. Rebase the
		 * desktop-global point into root-surface coordinates for that fallback. */
		surface = t->xdg_toplevel->base->surface;
		int scene_x = 0, scene_y = 0;
		if (!wlr_scene_node_coords(&t->scene_tree->node, &scene_x, &scene_y)) return NULL;
		sx = x - scene_x; sy = y - scene_y;
		sc = wlr_seat_client_for_wl_client(server->seat, wl_resource_get_client(surface->resource));
		if (!sc || wl_list_empty(&sc->pointers)) return NULL;
	}
	if (cua_ptr[0].pressed_count && cua_ptr[0].entered != surface) return NULL;
	/* Desktop scope also uses logical device 0. Keep wlroots' seat pointer
	 * focus in sync before cua_button() sends its protocol-complete seat
	 * notification; raw resource enters here would leave the seat targeting a stale
	 * surface, so the following button notification never reaches this point. */
	wlr_seat_pointer_notify_enter(server->seat, surface, sx, sy);
	wlr_seat_pointer_notify_motion(server->seat, cua_now_ms(), sx, sy);
	wlr_seat_pointer_notify_frame(server->seat);
	cua_ptr_set_surface(server, 0, surface, t);
	return t;
}
static int cua_pressed_button_index(struct cua_devstate *state, uint32_t button) {
	for (size_t i = 0; i < state->pressed_count; i++)
		if (state->pressed_buttons[i] == button) return (int)i;
	return -1;
}
static bool cua_button(struct tinywl_server *server, struct cua_conn *c, struct tinywl_toplevel *t, int idx, uint32_t button, bool pressed) {
	if (!cua_ptr_owned(c, t, idx)) return false;
	/* `cua_motion` establishes the exact child or root surface for this logical
	 * pointer. Button and axis events must use that same wl_pointer resource. */
	struct cua_devstate *state = &cua_ptr[idx];
	int pressed_idx = cua_pressed_button_index(state, button);
	if ((pressed && (pressed_idx >= 0 || state->pressed_count >= CUA_MAX_PRESSED_BUTTONS)) ||
		(!pressed && pressed_idx < 0)) return false;
	struct wlr_surface *surface = cua_ptr[idx].entered;
	struct wlr_seat_client *sc = wlr_seat_client_for_wl_client(server->seat, wl_resource_get_client(surface->resource));
	if (!sc || wl_list_empty(&sc->pointers)) return false;
	if (idx == 0) {
		if (server->seat->pointer_state.focused_surface != surface) return false;
		wlr_seat_pointer_notify_button(server->seat, cua_now_ms(), button,
			pressed ? WL_POINTER_BUTTON_STATE_PRESSED : WL_POINTER_BUTTON_STATE_RELEASED);
		/* See cua_motion: there is no hardware cursor-frame callback for the
		 * virtual device, so each injected command must close its own batch. */
		wlr_seat_pointer_notify_frame(server->seat);
	} else {
		uint32_t tm = cua_now_ms(), bs = wlr_seat_client_next_serial(sc);
		struct wl_resource *res;
		wl_resource_for_each(res, &sc->pointers) {
			wl_pointer_send_button(res, bs, tm, button, pressed ? WL_POINTER_BUTTON_STATE_PRESSED : WL_POINTER_BUTTON_STATE_RELEASED);
			cua_pframe(res);
		}
	}
	if (pressed) state->pressed_buttons[state->pressed_count++] = button;
	else {
		state->pressed_count--;
		state->pressed_buttons[pressed_idx] = state->pressed_buttons[state->pressed_count];
	}
	return true;
}
static void cua_ptr_release_index(struct tinywl_server *server, int idx) {
	if (!server || idx < 0 || idx >= CUA_MAXDEV) return;
	struct cua_devstate *state = &cua_ptr[idx];
	if (state->pressed_count && state->entered) {
		struct wlr_seat_client *sc = wlr_seat_client_for_wl_client(server->seat,
			wl_resource_get_client(state->entered->resource));
		struct wl_resource *res;
		bool notified = false;
		for (size_t i = 0; i < state->pressed_count; i++) {
			uint32_t button = state->pressed_buttons[i], tm = cua_now_ms();
			bool sent = false;
			/* wlroots' focus-surface destroy listener runs before ours and uses
			 * raw clear_focus(), which leaves button_count/grab state intact.
			 * Reconcile through notify_button while focus is still the original
			 * surface or has already been cleared; never notify a different
			 * focused client. With NULL focus notify_button consumes the tracked
			 * button and updates the active grab but cannot wire-deliver, so the
			 * original client still needs the direct release below. */
			if (idx == 0 &&
					(server->seat->pointer_state.focused_surface == state->entered ||
					 server->seat->pointer_state.focused_surface == NULL))
				sent = wlr_seat_pointer_notify_button(server->seat, tm, button,
					WL_POINTER_BUTTON_STATE_RELEASED) != 0;
			if (sent) notified = true;
			if (!sent && sc) {
				uint32_t bs = wlr_seat_client_next_serial(sc);
				wl_resource_for_each(res, &sc->pointers) {
					wl_pointer_send_button(res, bs, tm, button,
						WL_POINTER_BUTTON_STATE_RELEASED);
					cua_pframe(res);
				}
			}
		}
		if (notified) wlr_seat_pointer_notify_frame(server->seat);
	}
	state->pressed_count = 0;
	cua_ptr_owner[idx] = NULL;
	cua_ptr_target[idx] = NULL;
}
static bool cua_axis(struct tinywl_server *server, struct cua_conn *c, struct tinywl_toplevel *t, int idx, uint32_t axis, double value) {
	if (!cua_ptr_owned(c, t, idx)) return false;
	struct wlr_surface *surface = cua_ptr[idx].entered;
	struct wlr_seat_client *sc = wlr_seat_client_for_wl_client(server->seat, wl_resource_get_client(surface->resource));
	if (!sc || wl_list_empty(&sc->pointers)) return false;
	uint32_t tm = cua_now_ms();
	struct wl_resource *res;
	wl_resource_for_each(res, &sc->pointers) {
		int32_t step = value < 0 ? -1 : 1;
		if (wl_resource_get_version(res) >= WL_POINTER_AXIS_SOURCE_SINCE_VERSION)
			wl_pointer_send_axis_source(res, WL_POINTER_AXIS_SOURCE_WHEEL);
		if (wl_resource_get_version(res) >= WL_POINTER_AXIS_VALUE120_SINCE_VERSION)
			wl_pointer_send_axis_value120(res, axis, step * 120);
		else if (wl_resource_get_version(res) >= WL_POINTER_AXIS_DISCRETE_SINCE_VERSION)
			wl_pointer_send_axis_discrete(res, axis, step);
		wl_pointer_send_axis(res, tm, axis, wl_fixed_from_double(value));
		cua_pframe(res);
	}
	return true;
}
static void cua_init_keymap(struct tinywl_server *server) {
	struct xkb_context *ctx = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
	struct xkb_rule_names names = {0};
	struct xkb_keymap *km = xkb_keymap_new_from_names(ctx, &names, XKB_KEYMAP_COMPILE_NO_FLAGS);
	char *s = xkb_keymap_get_as_string(km, XKB_KEYMAP_FORMAT_TEXT_V1);
	g_keymap_size = strlen(s) + 1;
	g_keymap_fd = memfd_create("cua-keymap", MFD_CLOEXEC);
	if (ftruncate(g_keymap_fd, g_keymap_size) == 0) {
		void *p = mmap(NULL, g_keymap_size, PROT_READ | PROT_WRITE, MAP_SHARED, g_keymap_fd, 0);
		memcpy(p, s, g_keymap_size); munmap(p, g_keymap_size);
	}
	free(s);
	/* Build a US-layout char -> (evdev keycode, shift-level) table from the
	 * keymap so the `t` command can type arbitrary ASCII. xkb keycodes are
	 * evdev+8; wl_keyboard.key wants the evdev code. */
	xkb_mod_index_t shift = xkb_keymap_mod_get_index(km, XKB_MOD_NAME_SHIFT);
	if (shift != XKB_MOD_INVALID) g_shift_mask = (xkb_mod_mask_t)1 << shift;
	xkb_mod_index_t ctrl = xkb_keymap_mod_get_index(km, XKB_MOD_NAME_CTRL);
	if (ctrl != XKB_MOD_INVALID) g_ctrl_mask = (xkb_mod_mask_t)1 << ctrl;
	xkb_mod_index_t alt = xkb_keymap_mod_get_index(km, XKB_MOD_NAME_ALT);
	if (alt != XKB_MOD_INVALID) g_alt_mask = (xkb_mod_mask_t)1 << alt;
	xkb_mod_index_t logo = xkb_keymap_mod_get_index(km, XKB_MOD_NAME_LOGO);
	if (logo != XKB_MOD_INVALID) g_logo_mask = (xkb_mod_mask_t)1 << logo;
	for (xkb_keycode_t kc = 9; kc < 256; kc++) {
		for (int lvl = 0; lvl < 2; lvl++) {
			const xkb_keysym_t *syms;
			int n = xkb_keymap_key_get_syms_by_level(km, kc, 0, lvl, &syms);
			if (n != 1) continue;
			uint32_t cp = xkb_keysym_to_utf32(syms[0]);
			if (cp > 0 && cp < 128 && !g_chartab[cp].valid) {
				g_chartab[cp].keycode = (uint32_t)kc - 8;
				g_chartab[cp].shift = lvl;
				g_chartab[cp].valid = 1;
			}
		}
	}
	/* Control characters used by the protocol map to dedicated keys. */
	g_chartab['\n'] = (struct cua_keyent){ KEY_ENTER, 0, 1 };
	g_chartab['\t'] = (struct cua_keyent){ KEY_TAB, 0, 1 };
	/* A keyboard-capable seat must attach a real wlr_keyboard before clients
	 * bind. wlroots then sends keymap + repeat-info before any enter event;
	 * Chromium can stall if it receives an enter from a device-less seat. */
	wlr_keyboard_init(&g_keyboard, NULL, "cua-virtual-keyboard");
	wlr_keyboard_set_keymap(&g_keyboard, km);
	wlr_keyboard_set_repeat_info(&g_keyboard, 25, 600);
	wlr_seat_set_keyboard(server->seat, &g_keyboard);
	xkb_keymap_unref(km); xkb_context_unref(ctx);
	wlr_log(WLR_INFO, "[cua] xkb keymap + chartab ready (%zu bytes)", g_keymap_size);
}
/* Ensure the target client's keyboard has had keymap + enter sent once (focus
 * free). idx 0 is the typing keyboard. */
static struct wlr_seat_client *cua_kbd_enter(struct tinywl_server *server, struct tinywl_toplevel *t) {
	struct wlr_surface *surface = t->xdg_toplevel->base->surface;
	struct wlr_seat_client *sc = wlr_seat_client_for_wl_client(server->seat, wl_resource_get_client(surface->resource));
	if (!sc || wl_list_empty(&sc->keyboards)) return NULL;
	struct wlr_surface *focused = server->seat->keyboard_state.focused_surface;
	if (focused && wlr_surface_get_root_surface(focused) == surface) {
		/* Foreground delivery already has a protocol-complete seat enter. The
		 * key path below uses wlr_seat_keyboard_notify_key for this target. */
		cua_devstate_set_surface(&cua_kbd_state[0], surface);
		cua_kbd_state[0].entered_target = t;
		return sc;
	}
	if (cua_kbd_state[0].entered != surface) {
		struct wl_resource *res; struct wl_array keys; wl_array_init(&keys);
		wl_resource_for_each(res, &sc->keyboards) {
			wl_keyboard_send_keymap(res, WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, g_keymap_fd, g_keymap_size);
			wl_keyboard_send_enter(res, wlr_seat_client_next_serial(sc), surface->resource, &keys);
			wl_keyboard_send_modifiers(res, wlr_seat_client_next_serial(sc), 0, 0, 0, 0);
		}
		wl_array_release(&keys);
		cua_devstate_set_surface(&cua_kbd_state[0], surface);
		cua_kbd_state[0].entered_target = t;
	}
	return sc;
}
static void cua_kbd_mods(struct wlr_seat_client *sc, uint32_t depressed) {
	struct wl_resource *res;
	wl_resource_for_each(res, &sc->keyboards)
		wl_keyboard_send_modifiers(res, wlr_seat_client_next_serial(sc), depressed, 0, 0, 0);
}
static void cua_kbd_key(struct wlr_seat_client *sc, uint32_t keycode, bool pressed) {
	uint32_t tm = cua_now_ms();
	struct wl_resource *res;
	wl_resource_for_each(res, &sc->keyboards)
		wl_keyboard_send_key(res, wlr_seat_client_next_serial(sc), tm, keycode,
			pressed ? WL_KEYBOARD_KEY_STATE_PRESSED : WL_KEYBOARD_KEY_STATE_RELEASED);
}
/* Preserve compositor-native keyboard delivery when the addressed surface is
 * already the logical seat focus. Chromium relies on wlroots' focused-seat
 * state for foreground keyboard events; sending a raw wl_keyboard.key to its
 * resource can be acknowledged while never reaching the renderer. Background
 * targets retain the direct resource path that makes focus-free input possible. */
static void cua_kbd_key_target(struct tinywl_server *server, struct tinywl_toplevel *t,
		struct wlr_seat_client *sc, uint32_t keycode, bool pressed) {
	struct wlr_surface *target = wlr_surface_get_root_surface(t->xdg_toplevel->base->surface);
	struct wlr_surface *focused = server->seat->keyboard_state.focused_surface;
	struct wlr_surface *focused_root = focused ? wlr_surface_get_root_surface(focused) : NULL;
	if (target == focused_root) {
		wlr_seat_keyboard_notify_key(server->seat, cua_now_ms(), keycode,
			pressed ? WL_KEYBOARD_KEY_STATE_PRESSED : WL_KEYBOARD_KEY_STATE_RELEASED);
	} else {
		cua_kbd_key(sc, keycode, pressed);
	}
}
static bool cua_type_cp(struct tinywl_server *server, struct tinywl_toplevel *t, uint32_t cp) {
	if (cp >= 128 || !g_chartab[cp].valid) return false;
	struct wlr_seat_client *sc = cua_kbd_enter(server, t);
	if (!sc) return false;
	struct cua_keyent e = g_chartab[cp];
	if (e.shift) cua_kbd_mods(sc, g_shift_mask);
	cua_kbd_key_target(server, t, sc, e.keycode, true);
	cua_kbd_key_target(server, t, sc, e.keycode, false);
	if (e.shift) cua_kbd_mods(sc, 0);
	return true;
}
/* Decode a hex-encoded ASCII string and type it focus-free into `t`. */
static bool cua_type_hex(struct tinywl_server *server, struct tinywl_toplevel *t, const char *hex) {
	if (!t) return false;
	for (const char *p = hex; p[0] && p[1]; p += 2) {
		int hi = (p[0] <= '9') ? p[0] - '0' : (p[0] | 0x20) - 'a' + 10;
		int lo = (p[1] <= '9') ? p[1] - '0' : (p[1] | 0x20) - 'a' + 10;
		if (!cua_type_cp(server, t, (uint32_t)((hi << 4) | lo))) return false;
	}
	return true;
}
static uint32_t cua_named_keycode(const char *name) {
	uint32_t kc = 0;
	if (!strcasecmp(name, "enter") || !strcasecmp(name, "return")) kc = KEY_ENTER;
	else if (!strcasecmp(name, "tab")) kc = KEY_TAB;
	else if (!strcasecmp(name, "escape") || !strcasecmp(name, "esc")) kc = KEY_ESC;
	else if (!strcasecmp(name, "backspace")) kc = KEY_BACKSPACE;
	else if (!strcasecmp(name, "space")) kc = KEY_SPACE;
	else if (!strcasecmp(name, "up")) kc = KEY_UP;
	else if (!strcasecmp(name, "down")) kc = KEY_DOWN;
	else if (!strcasecmp(name, "left")) kc = KEY_LEFT;
	else if (!strcasecmp(name, "right")) kc = KEY_RIGHT;
	else if (!strncasecmp(name, "f", 1)) {
		char *end = NULL; long fn = strtol(name + 1, &end, 10);
		if (end && !*end && fn >= 1 && fn <= 10) kc = KEY_F1 + (uint32_t)fn - 1;
		else if (end && !*end && fn == 11) kc = KEY_F11;
		else if (end && !*end && fn == 12) kc = KEY_F12;
	}
	return kc;
}
/* Returns 1 when `name` is a recognised key (delivered if the target had a
 * keyboard bound), 0 when it is outside the whitelist so the caller can NAK. */
static int cua_key_named(struct tinywl_server *server, struct tinywl_toplevel *t, const char *name) {
	if (!t) return 0;
	uint32_t kc = cua_named_keycode(name);
	if (!kc) return 0;
	struct wlr_seat_client *sc = cua_kbd_enter(server, t);
	if (!sc) return -1;
	cua_kbd_key_target(server, t, sc, kc, true);
	cua_kbd_key_target(server, t, sc, kc, false);
	return 1;
}
static int cua_hotkey(struct tinywl_server *server, struct tinywl_toplevel *t, const char *mods, const char *key) {
	if (!t) return 0;
	uint32_t kc = cua_named_keycode(key);
	if (!kc && key[0] && !key[1]) {
		unsigned char cp = (unsigned char)key[0];
		if (cp < 128 && g_chartab[cp].valid) kc = g_chartab[cp].keycode;
	}
	if (!kc) return 0;
	xkb_mod_mask_t mask = 0;
	char copy[128]; snprintf(copy, sizeof copy, "%s", mods);
	char *save = NULL;
	for (char *mod = strtok_r(copy, ",", &save); mod; mod = strtok_r(NULL, ",", &save)) {
		if (!strcasecmp(mod, "ctrl") || !strcasecmp(mod, "control")) mask |= g_ctrl_mask;
		else if (!strcasecmp(mod, "shift")) mask |= g_shift_mask;
		else if (!strcasecmp(mod, "alt") || !strcasecmp(mod, "option")) mask |= g_alt_mask;
		else if (!strcasecmp(mod, "meta") || !strcasecmp(mod, "super") || !strcasecmp(mod, "win") || !strcasecmp(mod, "cmd")) mask |= g_logo_mask;
		else return 0;
	}
	struct wlr_seat_client *sc = cua_kbd_enter(server, t);
	if (!sc) return -1;
	cua_kbd_mods(sc, mask);
	cua_kbd_key_target(server, t, sc, kc, true);
	cua_kbd_key_target(server, t, sc, kc, false);
	cua_kbd_mods(sc, 0);
	return 1;
}
/* ── control socket: one line per command, routed by app_id ───────────────── */
/* Process one command line. Returns NULL on success, else a stable error token
 * the caller sends back as `err <token>`. The command is only acknowledged
 * after it has been resolved and applied — never before. */
static const char *cua_handle_cmd(struct tinywl_server *server, char *line, struct cua_conn *c) {
	char cmd[8], app[128];
	if (sscanf(line, "%7s", cmd) != 1) return "empty";
	struct tinywl_toplevel *batch_target = c->action_target;
	if (!strcmp(cmd, "d")) {
		double x, y; unsigned count, btn;
		if (!c->desktop_batch || batch_target) return "desktop-batch-required";
		if (sscanf(line, "d %lf %lf %u %u", &x, &y, &count, &btn) != 4) return "bad-args";
		if (count < 1 || count > CUA_MAX_CLICK_COUNT) return "click-count-out-of-range";
		struct tinywl_toplevel *t = cua_desktop_motion(server, c, x, y);
		if (!t) return "no-surface-or-pointer-busy";
		for (unsigned i = 0; i < count; i++) {
			if (!cua_button(server, c, t, 0, btn, true)) return "no-pointer-resource";
			if (!cua_button(server, c, t, 0, btn, false)) return "no-pointer-resource";
		}
		return NULL;
	}
	if (c->desktop_batch || !batch_target) return "batch-required";
	if (sscanf(line, "%*7s %127s", app) != 1) return "bad-args";
	if (strcmp(app, batch_target->cua_target)) return "batch-target-mismatch";
	const char *err = NULL;
	struct tinywl_toplevel *t;
	if (!strcmp(cmd, "m")) {
		int idx; double x, y;
		if (sscanf(line, "m %127s %d %lf %lf", app, &idx, &x, &y) != 4) return "bad-args";
		if (!(t = cua_resolve_target(server, app, &err))) return err;
		if (!cua_motion(server, c, t, idx, x, y)) return "no-pointer-resource-or-busy";
		return NULL;
	} else if (!strcmp(cmd, "b")) {
		int idx; unsigned btn, pr;
		if (sscanf(line, "b %127s %d %u %u", app, &idx, &btn, &pr) != 4) return "bad-args";
		if (!(t = cua_resolve_target(server, app, &err))) return err;
		if (!cua_button(server, c, t, idx, btn, pr != 0)) return "pointer-not-owned-by-batch";
		return NULL;
	} else if (!strcmp(cmd, "t")) {
		char hex[8192];
		if (sscanf(line, "t %127s %8191s", app, hex) != 2) return "bad-args";
		if (!(t = cua_resolve_target(server, app, &err))) return err;
		if (!cua_type_hex(server, t, hex)) return "no-keyboard-resource";
		return NULL;
	} else if (!strcmp(cmd, "k")) {
		char key[32];
		if (sscanf(line, "k %127s %31s", app, key) != 2) return "bad-args";
		if (!(t = cua_resolve_target(server, app, &err))) return err;
		int result = cua_key_named(server, t, key);
		if (result < 0) return "no-keyboard-resource";
		if (!result) return "unknown-key";
		return NULL;
	} else if (!strcmp(cmd, "h")) {
		char mods[128], key[32];
		if (sscanf(line, "h %127s %127s %31s", app, mods, key) != 3) return "bad-args";
		if (!(t = cua_resolve_target(server, app, &err))) return err;
		int result = cua_hotkey(server, t, mods, key);
		if (result < 0) return "no-keyboard-resource";
		if (!result) return "unknown-hotkey";
		return NULL;
	} else if (!strcmp(cmd, "a")) {
		int idx; unsigned axis; double value;
		if (sscanf(line, "a %127s %d %u %lf", app, &idx, &axis, &value) != 4) return "bad-args";
		if (!(t = cua_resolve_target(server, app, &err))) return err;
		if (!cua_axis(server, c, t, idx, axis, value)) return "pointer-not-owned-by-batch";
		return NULL;
	}
	return "unknown-command";
}
static void cua_schedule_frames(struct tinywl_server *server) {
	struct tinywl_output *output;
	wl_list_for_each(output, &server->outputs, link) wlr_output_schedule_frame(output->wlr_output);
}
static void cua_capture_restore(struct cua_conn *c) {
	if (!c || g_capture_owner != c) return;
	struct tinywl_toplevel *t;
	wl_list_for_each(t, &c->server->toplevels, link) wlr_scene_node_set_enabled(&t->scene_tree->node, true);
	if (c->capture_timer) { wl_event_source_remove(c->capture_timer); c->capture_timer = NULL; }
	c->capture_lease = 0;
	g_capture_owner = NULL;
	g_capture_target = NULL;
	cua_schedule_frames(c->server);
}
static int cua_capture_timeout(void *data) {
	struct cua_conn *c = data;
	cua_capture_restore(c);
	return 0;
}
static const char *cua_capture_begin(struct cua_conn *c, const char *target, char *out, size_t out_len) {
	if (g_capture_owner) return "capture-busy";
	const char *err = NULL;
	struct tinywl_toplevel *wanted = cua_resolve_target(c->server, target, &err);
	if (!wanted) return err;
	struct tinywl_toplevel *t;
	wl_list_for_each(t, &c->server->toplevels, link)
		wlr_scene_node_set_enabled(&t->scene_tree->node, t == wanted);
	c->capture_lease = g_capture_lease_seq++;
	if (!c->capture_lease) c->capture_lease = g_capture_lease_seq++;
	g_capture_owner = c;
	g_capture_target = wanted;
	c->capture_timer = wl_event_loop_add_timer(wl_display_get_event_loop(c->server->wl_display), cua_capture_timeout, c);
	if (!c->capture_timer || wl_event_source_timer_update(c->capture_timer, 10000) < 0) {
		cua_capture_restore(c);
		return "capture-timer-unavailable";
	}
	cua_schedule_frames(c->server);
	snprintf(out, out_len, "capture %llu", (unsigned long long)c->capture_lease);
	return NULL;
}
/* Tear a connection down once: remove its event source (else the loop fires it
 * again on freed data -> double free), close the fd, free the state. */
static int cua_conn_drop(struct cua_conn *c, int fd) {
	cua_capture_restore(c);
	cua_ptr_release(c);
	if (c->action_target && c->action_target->cua_action_owner == c)
		c->action_target->cua_action_owner = NULL;
	if (c->src) wl_event_source_remove(c->src);
	close(fd); free(c);
	return 0;
}
static int cua_conn_readable(int fd, uint32_t mask, void *data) {
	struct cua_conn *c = data;
	if (mask & (WL_EVENT_HANGUP | WL_EVENT_ERROR)) return cua_conn_drop(c, fd);
	ssize_t n = read(fd, c->buf + c->len, sizeof(c->buf) - c->len - 1);
	if (n <= 0) return cua_conn_drop(c, fd);
	c->len += (size_t)n; c->buf[c->len] = 0;
	char *p = c->buf, *nl;
	while ((nl = memchr(p, '\n', (size_t)(c->buf + c->len - p)))) {
		*nl = 0;
		/* Tolerate CRLF clients by trimming a trailing carriage return. */
		if (nl > p && nl[-1] == '\r') nl[-1] = 0;
		if (!c->hello) {
			/* The first line must be the versioned v2 handshake. */
			if (!strcmp(p, CUA_PROTO_HELLO)) {
				c->hello = 1;
				cua_reply(fd, CUA_PROTO_HELLO);
			} else {
				cua_reply(fd, "err unsupported-version");
				return cua_conn_drop(c, fd);
			}
		} else {
			int query_pid;
			char dispatch_cmd[8];
			char geometry_target[128];
			char activate_target[128];
			char capture_target[128];
			char begin_target[128];
			unsigned long long restore_lease;
			int desktop_data = sscanf(p, "%7s", dispatch_cmd) == 1 && !strcmp(dispatch_cmd, "d");
			if (c->desktop_batch && strcmp(p, "end") && !desktop_data) {
				cua_reply(fd, "err desktop-command-required");
			} else if (sscanf(p, "q %d", &query_pid) == 1) {
				char msg[128]; cua_query_state(c->server, (pid_t)query_pid, msg, sizeof msg);
				cua_reply(fd, msg);
			} else if (sscanf(p, "g %127s", geometry_target) == 1) {
				char msg[128];
				const char *err = cua_query_geometry(c->server, geometry_target, msg, sizeof msg);
				if (err) {
					char reply[128]; snprintf(reply, sizeof reply, "err %s", err); cua_reply(fd, reply);
				} else {
					cua_reply(fd, msg);
				}
			} else if (sscanf(p, "c %127s", capture_target) == 1) {
				char msg[128];
				const char *err = cua_capture_begin(c, capture_target, msg, sizeof msg);
				wl_display_flush_clients(c->server->wl_display);
				if (err) { char reply[128]; snprintf(reply, sizeof reply, "err %s", err); cua_reply(fd, reply); }
				else cua_reply(fd, msg);
			} else if (sscanf(p, "r %llu", &restore_lease) == 1) {
				if (g_capture_owner != c || c->capture_lease != (uint64_t)restore_lease) cua_reply(fd, "err stale-capture-lease");
				else { cua_capture_restore(c); cua_reply(fd, "ok"); }
			} else if (sscanf(p, "begin %127s", begin_target) == 1) {
				if (c->action_target || c->desktop_batch) {
					cua_reply(fd, "err nested-batch");
				} else if (!strcmp(begin_target, "desktop")) {
					c->desktop_batch = 1;
					cua_reply(fd, "ok");
				} else {
					const char *resolve_err = NULL;
					struct tinywl_toplevel *batch_target = cua_resolve_target(c->server, begin_target, &resolve_err);
					if (!batch_target) { char msg[128]; snprintf(msg, sizeof msg, "err %s", resolve_err); cua_reply(fd, msg); }
					else if (batch_target->cua_action_owner && batch_target->cua_action_owner != c) cua_reply(fd, "err target-busy");
					else { c->action_target = batch_target; batch_target->cua_action_owner = c; cua_reply(fd, "ok"); }
				}
			} else if (!strcmp(p, "end")) {
				if (!c->action_target && !c->desktop_batch) {
					cua_reply(fd, "err no-active-batch");
				} else {
					cua_ptr_release(c);
					if (c->action_target) c->action_target->cua_action_owner = NULL;
					c->action_target = NULL;
					c->desktop_batch = 0;
					cua_reply(fd, "ok");
				}
			} else if (sscanf(p, "f %127s", activate_target) == 1) {
				const char *err = !c->action_target ? "batch-required" :
					(strcmp(activate_target, c->action_target->cua_target) ? "batch-target-mismatch" : cua_activate_target(c->server, activate_target));
				wl_display_flush_clients(c->server->wl_display);
				if (err) {
					char msg[128]; snprintf(msg, sizeof msg, "err %s", err); cua_reply(fd, msg);
				} else {
					cua_reply(fd, "ok");
				}
			} else {
				const char *err = cua_handle_cmd(c->server, p, c);
				/* Deliver injected events before acking so `ok` means "processed",
				 * never merely "parsed". */
				wl_display_flush_clients(c->server->wl_display);
				if (err) {
					char msg[128];
					snprintf(msg, sizeof msg, "err %s", err);
					cua_reply(fd, msg);
				} else {
					cua_reply(fd, "ok");
				}
			}
		}
		p = nl + 1;
	}
	size_t rem = (size_t)(c->buf + c->len - p);
	memmove(c->buf, p, rem); c->len = rem;
	wl_display_flush_clients(c->server->wl_display);
	return 0;
}
static int cua_listen_cb(int fd, uint32_t mask, void *data) {
	struct tinywl_server *server = data;
	int cfd = accept(fd, NULL, NULL);
	if (cfd < 0) return 0;
	struct cua_conn *c = calloc(1, sizeof *c); c->server = server;
	c->src = wl_event_loop_add_fd(wl_display_get_event_loop(server->wl_display), cfd,
		WL_EVENT_READABLE, cua_conn_readable, c);
	return 0;
}
static void cua_setup_control_socket(struct tinywl_server *server) {
	cua_init_keymap(server);
	const char *path = getenv("CUA_INJECT_SOCKET");
	if (!path) { wlr_log(WLR_INFO, "[cua] no CUA_INJECT_SOCKET; injection disabled"); return; }
	int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	if (fd < 0) { wlr_log(WLR_ERROR, "[cua] socket(): %s", strerror(errno)); return; }
	struct sockaddr_un addr = {0}; addr.sun_family = AF_UNIX;
	if (strlen(path) >= sizeof addr.sun_path) { wlr_log(WLR_ERROR, "[cua] injection socket path too long"); close(fd); return; }
	memcpy(addr.sun_path, path, strlen(path) + 1);
	unlink(path);
	if (bind(fd, (struct sockaddr *)&addr, sizeof addr) < 0) { wlr_log(WLR_ERROR, "[cua] bind %s: %s", path, strerror(errno)); close(fd); return; }
	if (chmod(path, 0600) < 0 || listen(fd, 16) < 0) { wlr_log(WLR_ERROR, "[cua] secure/listen %s: %s", path, strerror(errno)); close(fd); unlink(path); return; }
	cua_init_epoch();
	wl_event_loop_add_fd(wl_display_get_event_loop(server->wl_display), fd,
		WL_EVENT_READABLE, cua_listen_cb, server);
	wlr_log(WLR_INFO, "[cua] injection control socket on %s", path);
}

"""

def repl(s, old, new, label, count=1):
    if s.count(old) != count:
        sys.stderr.write("ANCHOR FAIL [%s]: found %d (want %d)\n" % (label, s.count(old), count))
        sys.exit(1)
    return s.replace(old, new, count)

# 1) includes + globals + a foreign-toplevel handle field on the toplevel.
src = repl(src, "struct tinywl_server {", INCLUDES + "struct tinywl_server {", "includes")
# tinywl's desktop-oriented focus helper toggles xdg_toplevel activation on
# every focus transition. Chromium needs one coherent transition (deactivate
# the old surface, activate the new one) to finish renderer startup, but can
# stop scheduling frames after later toggles in this minimal headless
# compositor. Send that startup transition once per toplevel; seat focus plus
# scene stacking are authoritative after that.
src = repl(src,
    "\tif (prev_surface) {\n"
    "\t\t/*\n"
    "\t\t * Deactivate the previously focused surface. This lets the client know\n"
    "\t\t * it no longer has focus and the client will repaint accordingly, e.g.\n"
    "\t\t * stop displaying a caret.\n"
    "\t\t */\n"
    "\t\tstruct wlr_xdg_toplevel *prev_toplevel =\n"
    "\t\t\twlr_xdg_toplevel_try_from_wlr_surface(prev_surface);\n"
    "\t\tif (prev_toplevel != NULL) {\n"
    "\t\t\twlr_xdg_toplevel_set_activated(prev_toplevel, false);\n"
    "\t\t}\n"
    "\t}\n",
    "\tif (!toplevel->cua_initial_activation_sent && prev_surface) {\n"
    "\t\tstruct wlr_xdg_toplevel *prev_toplevel =\n"
    "\t\t\twlr_xdg_toplevel_try_from_wlr_surface(prev_surface);\n"
    "\t\tif (prev_toplevel != NULL) {\n"
    "\t\t\twlr_xdg_toplevel_set_activated(prev_toplevel, false);\n"
    "\t\t}\n"
    "\t}\n",
    "headless-startup-deactivation")
src = repl(src,
    "\t/* Activate the new surface */\n"
    "\twlr_xdg_toplevel_set_activated(toplevel->xdg_toplevel, true);\n",
    "\t/* Chromium needs one activated configure to complete renderer startup.\n"
    "\t * Later focus changes use the seat and scene only, avoiding activation\n"
    "\t * toggles that can stall it in this private headless compositor. */\n"
    "\tif (!toplevel->cua_initial_activation_sent) {\n"
    "\t\twlr_xdg_toplevel_set_activated(toplevel->xdg_toplevel, true);\n"
    "\t\ttoplevel->cua_initial_activation_sent = true;\n"
    "\t}\n",
    "headless-initial-activation")
src = repl(src,
    "\tstruct wlr_xdg_toplevel *xdg_toplevel;\n",
    STRUCT_FIELD, "ftl-field")

# 2) injection helpers + control socket, just before main().
src = repl(src, "int main(int argc, char *argv[]) {", FUNCS + "int main(int argc, char *argv[]) {", "funcs")

# 3) On map: register a foreign-toplevel handle (title/app_id) for list_windows.
src = repl(src,
    "\twl_list_insert(&toplevel->server->toplevels, &toplevel->link);\n\n\tfocus_toplevel(toplevel);",
    "\twl_list_insert(&toplevel->server->toplevels, &toplevel->link);\n"
    "\tcua_assign_target(toplevel);\n"
    "\tif (g_ftl_mgr) {\n"
    "\t\ttoplevel->ftl = wlr_foreign_toplevel_handle_v1_create(g_ftl_mgr);\n"
    "\t\tif (toplevel->xdg_toplevel->title)\n"
    "\t\t\twlr_foreign_toplevel_handle_v1_set_title(toplevel->ftl, toplevel->xdg_toplevel->title);\n"
    "\t\twlr_foreign_toplevel_handle_v1_set_app_id(toplevel->ftl, toplevel->cua_target);\n"
    "\t\ttoplevel->ftl_request_activate.notify = cua_ftl_request_activate;\n"
    "\t\twl_signal_add(&toplevel->ftl->events.request_activate, &toplevel->ftl_request_activate);\n"
    "\t}\n"
    "\t/* A lease is exact for its full lifetime, including toplevels mapped\n"
    "\t * after it begins. New non-target scene trees never become capturable. */\n"
    "\tif (g_capture_owner && toplevel != g_capture_target)\n"
    "\t\twlr_scene_node_set_enabled(&toplevel->scene_tree->node, false);\n"
    "\tif (!g_capture_owner || toplevel == g_capture_target)\n"
    "\t\tcua_maybe_focus_new_toplevel(toplevel);",
    "ftl-on-map")

# 4) On unmap: drop the foreign-toplevel handle.
src = repl(src,
    "\twl_list_remove(&toplevel->link);\n}",
    "\tif (g_capture_target == toplevel) cua_capture_restore(g_capture_owner);\n"
    "\tfor (int i = 0; i < CUA_MAXDEV; i++) {\n"
    "\t\tif (cua_ptr_target[i] == toplevel) {\n"
    "\t\t\tcua_ptr_release_index(toplevel->server, i); cua_ptr_set_surface(toplevel->server, i, NULL, NULL);\n"
    "\t\t} else if (cua_ptr[i].entered_target == toplevel) {\n"
    "\t\t\tcua_ptr_release_index(toplevel->server, i); cua_ptr_set_surface(toplevel->server, i, NULL, NULL);\n"
    "\t\t}\n"
    "\t\tif (cua_kbd_state[i].entered_target == toplevel) cua_devstate_set_surface(&cua_kbd_state[i], NULL);\n"
    "\t}\n"
    "\tif (toplevel->cua_action_owner) { toplevel->cua_action_owner->action_target = NULL; toplevel->cua_action_owner = NULL; }\n"
    "\tif (toplevel->ftl) { wl_list_remove(&toplevel->ftl_request_activate.link); wlr_foreign_toplevel_handle_v1_destroy(toplevel->ftl); toplevel->ftl = NULL; }\n"
    "\twl_list_remove(&toplevel->link);\n}",
    "ftl-on-unmap")

# 5) On commit: keep title/app_id fresh (they often arrive after the map).
src = repl(src,
    "\t\twlr_xdg_toplevel_set_size(toplevel->xdg_toplevel, 0, 0);\n\t}",
    "\t\twlr_xdg_toplevel_set_size(toplevel->xdg_toplevel, 0, 0);\n\t}\n"
    "\tif (toplevel->ftl) {\n"
    "\t\tif (toplevel->xdg_toplevel->title)\n"
    "\t\t\twlr_foreign_toplevel_handle_v1_set_title(toplevel->ftl, toplevel->xdg_toplevel->title);\n"
    "\t\twlr_foreign_toplevel_handle_v1_set_app_id(toplevel->ftl, toplevel->cua_target);\n"
    "\t}",
    "ftl-on-commit")

# 6) Headless backend has no preferred mode -> set a real custom mode so the
#    scene has a non-zero output for screencopy/grim.
src = repl(src,
    "\tstruct wlr_output_mode *mode = wlr_output_preferred_mode(wlr_output);\n\tif (mode != NULL) {\n\t\twlr_output_state_set_mode(&state, mode);\n\t}",
    "\tstruct wlr_output_mode *mode = wlr_output_preferred_mode(wlr_output);\n\tif (mode != NULL) {\n\t\twlr_output_state_set_mode(&state, mode);\n\t} else {\n"
    "\t\tint ow = getenv(\"CUA_OUTW\") ? atoi(getenv(\"CUA_OUTW\")) : 1280;\n"
    "\t\tint oh = getenv(\"CUA_OUTH\") ? atoi(getenv(\"CUA_OUTH\")) : 1024;\n"
    "\t\twlr_output_state_set_custom_mode(&state, ow, oh, 0);\n\t}",
    "headless-custom-mode")

# 7) Create the extra managers + the seat caps + the control socket in main.
src = repl(src,
    "\tserver.output_layout = wlr_output_layout_create(server.wl_display);",
    "\tserver.output_layout = wlr_output_layout_create(server.wl_display);\n"
    "\twlr_xdg_output_manager_v1_create(server.wl_display, server.output_layout);\n"
    "\twlr_screencopy_manager_v1_create(server.wl_display);\n"
    "\tg_ftl_mgr = wlr_foreign_toplevel_manager_v1_create(server.wl_display);",
    "managers")
src = repl(src,
    'server.seat = wlr_seat_create(server.wl_display, "seat0");',
    'server.seat = wlr_seat_create(server.wl_display, "seat0");\n'
    "\twlr_seat_set_capabilities(server.seat, WL_SEAT_CAPABILITY_POINTER | WL_SEAT_CAPABILITY_KEYBOARD);",
    "seat-caps")
src = repl(src,
    "\twl_display_run(server.wl_display);",
    "\tcua_setup_control_socket(&server);\n\twl_display_run(server.wl_display);",
    "control-socket-setup")
src = repl(src,
    "\twlr_backend_destroy(server.backend);\n"
    "\twl_display_destroy(server.wl_display);\n"
    "\treturn 0;",
    "\twlr_backend_destroy(server.backend);\n"
    "\twlr_keyboard_finish(&g_keyboard);\n"
    "\twl_display_destroy(server.wl_display);\n"
    "\treturn 0;",
    "virtual-keyboard-cleanup")

io.open(out, "w", encoding="utf-8").write(src)
sys.stderr.write("cua-compositor.c written (%d bytes)\n" % len(src))
