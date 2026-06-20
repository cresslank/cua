# Cua Driver Window Control GNOME Shell extension

Local-only GNOME Shell extension for Cua Driver's opt-in GNOME backend.

It exports a narrow session-bus API so `cua-driver` can enumerate and safely control GNOME/Mutter windows that are invisible to the X11/XWayland `_NET_CLIENT_LIST` path.

## Security posture

This extension intentionally exposes only allowlisted window/workspace methods:

- `ListWindowsJson() -> s`
- `GetFocusedWindowJson() -> s`
- `ActivateWindow(u id) -> b`
- `MoveResizeWindow(u id, i x, i y, i width, i height) -> b`
- `MoveWindowToWorkspace(u id, i workspace_index) -> b`
- `SwitchWorkspace(i workspace_index) -> b`

It does **not** expose generic GNOME Shell JavaScript evaluation, filesystem access, network access, clipboard access, screenshot capture, or synthetic input.

Window IDs are per-extension-session integers. They are stable while the extension is loaded, but callers should refresh with `ListWindowsJson()` after windows close/reopen or after restarting GNOME Shell.

## Install for local testing

From the Cua repo root:

```bash
mkdir -p ~/.local/share/gnome-shell/extensions
rm -rf ~/.local/share/gnome-shell/extensions/cua-driver-window-control@local
cp -R libs/cua-driver/gnome-shell-extension/cua-driver-window-control@local \
  ~/.local/share/gnome-shell/extensions/
gnome-extensions enable cua-driver-window-control@local
```

On Wayland sessions, log out/in if GNOME Shell does not load the extension immediately. On Xorg sessions, `Alt+F2`, `r`, Enter may be enough.

Enable the Cua Rust backend separately:

```bash
CUA_DRIVER_GNOME_SHELL=1 cua-driver call list_windows '{}'
```

Do not install or enable this extension automatically from `cua-driver serve`; keep opt-in service wiring outside the app repo.

## Manual D-Bus probes

```bash
gdbus call --session \
  --dest org.trycua.Driver.GnomeShell \
  --object-path /org/trycua/Driver/GnomeShell \
  --method org.trycua.Driver.GnomeShell.ListWindowsJson
```

The result is a JSON string containing records with fields like `id`, `title`, `app_id`, `wm_class`, `pid`, `workspace`, `focused`, `minimized`, `visible`, `x`, `y`, `width`, and `height`.
