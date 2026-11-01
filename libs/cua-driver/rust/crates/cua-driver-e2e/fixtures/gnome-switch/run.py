#!/usr/bin/env python3
"""Opt-in adapter for run_shell_lab.sh. All fixture changes stay in owned scratch.

Usage: python3 run.py <run_shell_lab.sh> <source winrects@cua> <driver binary> <scratch>
The supplied runner must be the reviewed private-bus/AT-SPI/Mutter EIS lab; this
copies its fixtures, not the helper implementation, and runs its private-bus outer
runner with a Cua-scoped process-admission guard. A Rust manual E2E entry point exposes the same opt-in command.
"""
from pathlib import Path
import json
import os
import shutil
import subprocess
import sys
import tempfile

runner, helper, driver, scratch = map(lambda p: Path(p).resolve(strict=True), sys.argv[1:])
assert runner.name == 'run_shell_lab.sh'
assert scratch.is_dir() and not scratch.is_relative_to('/tmp')
here = Path(__file__).resolve().parent
root = Path(tempfile.mkdtemp(prefix='switch-lab-', dir=scratch))
lab = root/'harness'
shutil.copytree(runner.parent, lab)
label = root.name
out = root/('out-' + label)
# Preserve the canonical private-bus runner. Its global Cargo-name guard is
# narrowed in this owned copy to actual Cua desktop-lock contenders: unrelated
# Rust projects cannot acquire the driver's per-UID locks. All Cua gates/lanes
# and other private Shells still refuse, with their actual cwd recorded.
assert (lab/runner.name).read_bytes() == runner.read_bytes()
shutil.copyfile(here/'cua_jobs.py', lab/'cua_jobs.py')
outer = (lab/runner.name).read_text()
guard = "pgrep -f '[s]lice_chain.sh|[e]2e_lane.sh|[e]2e_contained.sh|[c]argo (test|build|check)' >/dev/null"
assert outer.count(guard) == 1
(lab/runner.name).write_text(outer.replace(guard, '/usr/bin/python3 "$L/cua_jobs.py"'))
(lab/'lab_mcp.py').write_bytes((here/'mcp.py').read_bytes())
for filename in ('lab_probe.py', 'lab_cover.py'):
    path = lab/filename
    code = path.read_text()
    code = code.replace('from gi.repository import GLib, Gtk', 'from gi.repository import GLib, Gtk, Gdk')
    # Connect to every test-owned Gtk.Window; this records even keys consumed by
    # an entry before the window's ordinary key-press handler.
    code += '\n'
    old = 'Gtk.main()'
    assert code.count(old) == 1
    code = code.replace(old, '''
for window in Gtk.Window.list_toplevels():
    window.connect('key-press-event', lambda _w, event: print('CLIENT-KEY', Gdk.keyval_name(event.keyval), flush=True) or False)
Gtk.main()''')
    if filename == 'lab_probe.py':
        code = code.replace('class Lab(Gtk.Window):', "GLib.set_prgname('org.cua.SwitchProbe')\nGLib.set_application_name('Cua Switch Probe')\n\nclass Lab(Gtk.Window):")
    path.write_text(code)
# The normal cover is optional; keep it mapped for all nine switch scenarios.
inner = lab/'lab_shell_inner.sh'
text = inner.read_text()
text = text.replace("['winrects@cua', 'noov@lab']", "['winrects@cua', 'noov@lab', 'switch@lab']")
anchor = 'gsettings set org.gnome.shell enabled-extensions'
assert text.count(anchor) == 1
setup = '''mkdir -p "$XDG_DATA_HOME/gnome-shell/extensions/switch@lab" "$XDG_DATA_HOME/applications"
cp "$L/switch-extension.js" "$XDG_DATA_HOME/gnome-shell/extensions/switch@lab/extension.js"
cp "$L/switch-metadata.json" "$XDG_DATA_HOME/gnome-shell/extensions/switch@lab/metadata.json"
cp "$L/switch-probe.desktop" "$XDG_DATA_HOME/applications/org.cua.SwitchProbe.desktop"
'''
text = text.replace(anchor, setup + anchor)
# Launch cover before MCP; unlike LAB_COVER's capture gate the controller owns
# every subsequent state transition and needs no screenshot or gate file.
anchor = 'gate="$XDG_RUNTIME_DIR/lab-gate"'
text = text.replace(anchor, '''WAYLAND_DISPLAY=wl-cuaq GDK_BACKEND=wayland /usr/bin/python3 "$L/lab_cover.py" > "$out/cover.log" 2>&1 &
fixed_cover=$!
sleep 1
''' + anchor)
text = text.replace('env CUA_DRIVER_RS_ENABLE_WAYLAND=1', 'env CUA_LOG=platform_linux::wayland::libei=debug SWITCH_LAB_OUT="$out" CUA_DRIVER_RS_ENABLE_WAYLAND=1')
text = text.replace('kill $rpid $ppid $cpid', 'kill $rpid $ppid $cpid $fixed_cover').replace('wait $rpid $ppid $cpid', 'wait $rpid $ppid $cpid $fixed_cover')
inner.write_text(text)
(lab/'switch-extension.js').write_bytes((here/'extension.js').read_bytes())
version = subprocess.check_output(['gnome-shell', '--version'], text=True).split()[2].split('.')[0]
(lab/'switch-metadata.json').write_text(json.dumps({'uuid':'switch@lab','name':'Private switch fixture','description':'Test only','shell-version':[version]}))
(lab/'switch-probe.desktop').write_text('[Desktop Entry]\nType=Application\nName=Cua Switch Probe\nExec=/usr/bin/python3 ' + str(lab/'lab_probe.py') + '\nStartupWMClass=org.cua.SwitchProbe\nTerminal=false\n')
packaged = root/'packaged-helper'
subprocess.run([sys.executable, str(helper.parent/'package.py'), str(packaged)], check=True)
env = dict(os.environ, LAB_OUT=str(root))
env.pop('LAB_COVER', None)
completed = subprocess.run(['bash', str(lab/runner.name), label, str(packaged), str(driver)], env=env, timeout=240)
print('SWITCH_LAB_EVIDENCE=' + str(out), flush=True)
log = (out/'mcp.log').read_text()
print(log, flush=True)
assert completed.returncode == 0 and 'SWITCH-LAB PASS 9 cases; zero client text/Return/Escape keys' in log
