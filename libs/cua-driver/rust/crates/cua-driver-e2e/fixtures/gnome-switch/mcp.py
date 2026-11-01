#!/usr/bin/python3
"""Private-lab MCP receiver assertions; never run against a host session."""
import ast
import json
import os
from pathlib import Path
import selectors
import subprocess
import sys
import time
from typing import Any

assert os.environ['DBUS_SYSTEM_BUS_ADDRESS'].startswith('unix:path=' + os.environ['XDG_RUNTIME_DIR'])
assert Path(os.environ['HOME']).parent == Path(os.environ['XDG_RUNTIME_DIR']).parent
assert os.environ['WAYLAND_DISPLAY'] == 'wl-cuaq'
out = Path(os.environ['SWITCH_LAB_OUT'])
proc = subprocess.Popen([sys.argv[1], 'mcp'], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=sys.stderr, text=True, bufsize=1)
assert proc.stdin is not None and proc.stdout is not None
next_id = 0

def rpc(method, params=None, notify=False):
    global next_id
    msg: dict[str, Any] = dict(jsonrpc='2.0', method=method)
    if params is not None:
        msg['params'] = params
    if not notify:
        next_id += 1
        msg['id'] = next_id
    proc.stdin.write(json.dumps(msg) + '\n')
    proc.stdin.flush()
    if notify:
        return
    while True:
        with selectors.DefaultSelector() as selector:
            selector.register(proc.stdout, selectors.EVENT_READ)
            assert selector.select(35), 'MCP reply deadline'
        line = proc.stdout.readline()
        assert line, 'driver closed stdout'
        reply = json.loads(line)
        if reply.get('id') == next_id:
            return reply

def tool(name, args):
    reply = rpc('tools/call', dict(name=name, arguments=args))
    assert reply is not None
    result = reply.get('result', reply)
    print(json.dumps(dict(event='tool', name=name, arguments=args, result=result)), flush=True)
    return result

def control(method, mode=None):
    command = ['gdbus', 'call', '--session', '--dest', 'org.cua.SwitchLab', '--object-path',
               '/org/cua/SwitchLab', '--method', 'org.cua.SwitchLab.' + method]
    if mode is not None:
        command.append(mode)
    raw = subprocess.check_output(command, text=True, timeout=5)
    value = json.loads(ast.literal_eval(raw)[0])
    print(json.dumps(dict(event='compositor', method=method, mode=mode, value=value)), flush=True)
    return value

def keys():
    return sum('CLIENT-KEY' in line and line != 'CLIENT-KEY Super_L'
               for p in [out/'probe.log', out/'cover.log'] for line in p.read_text().splitlines())

def overlays():
    return sum(p.read_text().splitlines().count('CLIENT-KEY Super_L')
               for p in [out/'probe.log', out/'cover.log'])

try:
    rpc('initialize', dict(protocolVersion='2025-06-18', capabilities={}, clientInfo=dict(name='switch-window-private-lab',version='1')))
    rpc('notifications/initialized', notify=True)
    windows = tool('list_windows', {})['structuredContent']['windows']
    target, = [w for w in windows if w.get('title') == 'cua-lab-probe']
    args = {k: target[k] for k in ('pid', 'window_id')}
    baseline = keys()
    assert baseline == 0, ('fixture received a key before switching', baseline)
    timings = []
    for mode in ('minimized', 'workspace', 'overview', 'keyboard', 'auto_after_keyboard', 'native_after_keyboard', 'keyboard_default', 'refusal', 'post_proof', 'no_monitor', 'cleanup_retry', 'showing_stuck'):
        control('Setup', 'keyboard' if mode in ('keyboard_default', 'auto_after_keyboard', 'native_after_keyboard') else mode)
        time.sleep(0.5)
        before = control('Observe')
        if mode in ('minimized', 'keyboard', 'keyboard_default', 'refusal', 'post_proof'):
            assert before['minimized'] is True, before
        if mode == 'workspace':
            assert before['workspace'] != before['active_workspace'], before
        if mode == 'overview':
            assert before['overview'] is True, before
        start_keys, start_overlays = keys(), overlays()
        switch_args = dict(**args, method='keyboard' if mode in ('keyboard', 'keyboard_default', 'refusal', 'post_proof', 'no_monitor', 'cleanup_retry', 'showing_stuck') else 'native')
        if mode != 'keyboard_default':
            switch_args['query'] = 'SHOULD NEVER REACH CLIENT' if mode in ('refusal', 'post_proof', 'cleanup_retry') else 'Cua Switch Probe'
        if mode == 'auto_after_keyboard':
            switch_args['method'] = 'auto'
        started = time.monotonic()
        result = tool('switch_window', switch_args)
        elapsed_ms = (time.monotonic() - started) * 1000
        # Measure only the public MCP operation, not fixture setup/settling.
        timing = dict(event='timing', case=mode, method=switch_args['method'], elapsed_ms=elapsed_ms)
        timings.append(timing)
        print(json.dumps(timing), flush=True)
        structured = result.get('structuredContent', {})
        time.sleep(0.5)
        after = control('Observe')
        if mode in ('refusal', 'post_proof', 'no_monitor', 'cleanup_retry', 'showing_stuck'):
            expected = 'overview_stuck' if mode == 'showing_stuck' else 'switch_unverified' if mode == 'cleanup_retry' else 'shell_not_ready'
            assert result.get('isError') is True and structured.get('code') == expected, result
            if mode == 'no_monitor':
                assert overlays() == start_overlays and not after['overview'], after
                assert elapsed_ms < 1500, timing
            if mode == 'cleanup_retry':
                assert after['hide_calls'] >= 2 and not after['overview'] and not after['animation'], after
            if mode == 'showing_stuck':
                assert after['held_show_completion'] and after['overview'] and after['animation'], after
                assert elapsed_ms < 6500, timing
                reasons = str(structured['attempts'])
                assert 'overview did not hide' in reasons and 'original switch failure' in reasons, structured
            assert not structured['focused'] and structured['effect'] == ('unverifiable' if mode == 'cleanup_retry' else 'refused'), structured
            if mode == 'post_proof':
                assert after['post_proof_hit'] is True and after['focus'] == 'cua-lab-probe', after
                assert 'CLIENT-KEY' not in (out/'probe.log').read_text(), 'post-proof delivery reached probe'

        else:
            assert not result.get('isError') and structured.get('focused') is True, result
            assert structured['route'] == ('keyboard' if mode in ('keyboard', 'keyboard_default') else 'native'), structured
            assert structured['window_id'] == args['window_id'] and structured['pid'] == args['pid'], structured
            assert structured['effect'] == 'verified', structured
            assert after['focus'] == 'cua-lab-probe' and after['focus_pid'] == args['pid'], after
            assert after['minimized'] is False and after['workspace'] == after['active_workspace'] and not after['overview'], after
        assert keys() == start_keys, ('client received a switch key', mode, start_keys, keys())
        # Mutter decides whether to open overview on Super release; GTK may
        # observe the preceding modifier press. This is the contract's one
        # explicit exception, not permission for query/Return/Escape leakage.
        assert overlays() - start_overlays <= (1 if switch_args['method'] == 'keyboard' else 0)
        print(json.dumps(dict(event='PASS', case=mode, before=before, after=after,
                              client_text_return_escape_keys=keys()-start_keys,
                              allowed_overlay_presses=overlays()-start_overlays, structured=structured)), flush=True)
    # The dedicated switch exception must not authorize ordinary global keys.
    ordinary = tool('press_key', dict(**args, key='F24', delivery_mode='background'))
    assert ordinary.get('isError') is True and 'background_unavailable' in json.dumps(ordinary), ordinary
    assert keys() == baseline, 'ordinary key bypassed native-Wayland background refusal'
    # Native-only activation and the first auto call after a keyboard switch
    # must not inherit an impossible libei readiness wait. Keep safety/focus
    # assertions above authoritative even when a timing budget is exceeded.
    for timing in timings:
        budget_ms = 2000 if timing['case'] in ('keyboard', 'keyboard_default') else 300 if timing['case'] in ('auto_after_keyboard', 'native_after_keyboard') else None
        if budget_ms is not None:
            assert timing['elapsed_ms'] < budget_ms, ('switch latency budget exceeded', budget_ms, timing)
    print('SWITCH-LAB PASS 12 cases; zero client text/Return/Escape keys', flush=True)
finally:
    proc.stdin.close()
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.terminate()
        proc.wait(timeout=5)
