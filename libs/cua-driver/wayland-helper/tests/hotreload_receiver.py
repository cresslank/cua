#!/usr/bin/env python3
"""Owned GTK receiver; never starts outside the private hotreload environment."""
import json
import os
from pathlib import Path

import gi

gi.require_version('Gtk', '3.0')
gi.require_version('Gdk', '3.0')
from gi.repository import Gdk, GLib, Gtk


def main():
    root = Path(os.environ['CUA_HOTRELOAD_ROOT']).resolve()
    assert os.environ['HOME'] == str(root / 'home')
    assert os.environ['XDG_RUNTIME_DIR'] == str(root / 'run')
    assert os.environ['DBUS_SESSION_BUS_ADDRESS'] == f'unix:path={root}/run/bus'
    assert os.environ.get('GDK_BACKEND') == 'wayland'
    state = {'pid': os.getpid(), 'clicks': 0, 'keys': 0, 'focus': [], 'windows': []}
    output = root / 'out/receiver.json'

    def publish():
        next_path = output.with_suffix('.next')
        next_path.write_text(json.dumps(state, sort_keys=True))
        next_path.replace(output)
        return GLib.SOURCE_CONTINUE

    def pressed(_widget, event):
        state['clicks' if event.type == Gdk.EventType.BUTTON_PRESS else 'keys'] += 1
        publish()
        return False

    windows = []
    for title in ['Hotreload A', 'Hotreload B']:
        window = Gtk.Window(title=title)
        window.set_default_size(420, 300)
        area = Gtk.Button(label=title + '\nReal private GTK receiver')
        def clicked(_button):
            state['clicks'] += 1
            publish()
        area.connect('clicked', clicked)
        window.add(area)
        window.add_events(Gdk.EventMask.BUTTON_PRESS_MASK | Gdk.EventMask.KEY_PRESS_MASK)
        window.connect('button-press-event', pressed)
        window.connect('key-press-event', pressed)
        window.connect('focus-in-event', lambda _w, _e, name=title: state['focus'].append(name))
        window.show_all()
        windows.append(window)
        state['windows'].append(title)
    GLib.timeout_add(100, publish)
    publish()
    Gtk.main()


if __name__ == '__main__':
    main()
