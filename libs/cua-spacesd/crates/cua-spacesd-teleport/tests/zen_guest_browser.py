#!/usr/bin/env python3
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.

"""Runs only inside contained_zen.py's disposable networkless container."""
import base64
import hashlib
import http.server
import io
import json
import os
import pathlib
import signal
import socket
import subprocess
import tarfile
import threading
import time
import traceback
import urllib.parse

ROOT = pathlib.Path("/home/fixture")
OUT = ROOT / "evidence"
OUT.mkdir(mode=0o700)
NONCE = "synthetic-" + os.urandom(12).hex()
EVENTS = []
PHASE = "source"
CHILDREN = []


def save(name, value):
    (OUT / name).write_text(json.dumps(value, indent=2) + "\n")


class Site(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        auth = ("session=" + NONCE) in self.headers.get("Cookie", "")
        if self.path.startswith("/evidence?"):
            EVENTS.append({"phase": PHASE, "host": self.headers["Host"], "authenticated": auth,
                           "query": urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)})
            self.send_response(204)
            self.end_headers()
            return
        if self.path == "/login":
            page = '<form action="/login" method="POST"><input name="username"><input name="password" type="password"><button>Log in</button></form>'
        else:
            seed = "localStorage.setItem('synthetic-session', " + json.dumps(NONCE) + ");" if PHASE == "source" and auth else ""
            page = '<title>Synthetic Account</title><h1>' + ("Signed in" if auth else "Signed out") + '</h1><p>' + NONCE + '</p><pre id="storage"></pre><script>' + seed + "const value=localStorage.getItem('synthetic-session'); document.querySelector('#storage').textContent=value; fetch('/evidence?storage='+encodeURIComponent(value));</script>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.end_headers()
        self.wfile.write(page.encode())

    def do_POST(self):
        data = urllib.parse.parse_qs(self.rfile.read(int(self.headers.get("Content-Length", 0))).decode())
        ok = data == {"username": ["synthetic-user"], "password": ["Synthetic-only-" + NONCE]}
        EVENTS.append({"phase": PHASE, "kind": "login-post", "host": self.headers["Host"], "credentials_correct": ok})
        self.send_response(303 if ok else 403)
        if ok:
            self.send_header("Set-Cookie", "session=" + NONCE + "; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400")
            self.send_header("Location", "/account")
        self.end_headers()


class Marionette:
    def __init__(self, sock):
        self.s = sock
        self.n = 0
        self.recv()

    def recv(self):
        length = b""
        while True:
            c = self.s.recv(1)
            if not c:
                raise EOFError("Marionette closed")
            if c == b":":
                break
            length += c
        count = int(length)
        data = b""
        while len(data) < count:
            chunk = self.s.recv(count - len(data))
            if not chunk:
                raise EOFError("Marionette truncated")
            data += chunk
        return json.loads(data)

    def call(self, name, params=None):
        self.n += 1
        data = json.dumps([0, self.n, name, params or {}]).encode()
        self.s.sendall(str(len(data)).encode() + b":" + data)
        response = self.recv()
        if response[2]:
            raise RuntimeError(name + ": " + json.dumps(response[2]))
        return response[3]

    def js(self, script, args=None, asynchronous=False):
        return self.call("WebDriver:ExecuteAsyncScript" if asynchronous else "WebDriver:ExecuteScript",
                         {"script": script, "args": args or [], "newSandbox": True, "sandbox": "default", "line": 1, "filename": "synthetic-zen-receiver"})["value"]


def browser(profile, name):
    profile.mkdir(mode=0o700, parents=True, exist_ok=True)
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    prefs = {"marionette.port": port, "browser.shell.checkDefaultBrowser": False,
             "browser.aboutwelcome.enabled": False, "browser.startup.page": 0,
             "browser.startup.homepage": "about:blank", "browser.newtabpage.enabled": False,
             "datareporting.policy.dataSubmissionEnabled": False, "toolkit.telemetry.enabled": False,
             "network.proxy.type": 0, "network.trr.mode": 5,
             "network.captive-portal-service.enabled": False, "network.connectivity-service.enabled": False}
    # Test-only browser preferences, never product environment switches.
    with (profile / "user.js").open("a") as f:
        for k, v in prefs.items():
            f.write("user_pref(" + json.dumps(k) + ", " + json.dumps(v) + ");\n")
    env = {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": str(ROOT), "TMPDIR": "/tmp",
           "XDG_RUNTIME_DIR": str(ROOT), "XDG_CACHE_HOME": str(ROOT / "cache"),
           "MOZ_HEADLESS": "1", "MOZ_NO_REMOTE": "1", "MOZ_CRASHREPORTER_DISABLE": "1",
           "MOZ_REMOTE_ALLOW_SYSTEM_ACCESS": "1", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/no-session-bus"}
    log = (OUT / (name + "-browser.log")).open("wb")
    child = subprocess.Popen(["firefox", "--headless", "--no-remote", "--profile", str(profile), "--marionette", "--remote-allow-system-access", "about:blank"], env=env, stdout=log, stderr=subprocess.STDOUT)
    CHILDREN.append(child)
    deadline = time.monotonic() + 60
    while True:
        if child.poll() is not None:
            raise RuntimeError("browser exited " + str(child.returncode))
        try:
            sock = socket.create_connection(("127.0.0.1", port), timeout=1)
            break
        except OSError:
            if time.monotonic() > deadline:
                raise TimeoutError("Marionette startup")
            time.sleep(.2)
    sock.settimeout(35)
    m = Marionette(sock)
    caps = m.call("WebDriver:NewSession", {"capabilities": {"alwaysMatch": {"acceptInsecureCerts": False}}})
    m.call("WebDriver:SetTimeouts", {"script": 25000, "pageLoad": 25000, "implicit": 0})
    save(name + "-capabilities.json", caps)
    return child, m, log


def close_browser(child, m, log):
    try:
        m.call("Marionette:SetContext", {"value": "chrome"})
        m.call("Marionette:Quit", {"flags": ["eAttemptQuit"]})
    except (EOFError, ConnectionError):
        pass
    finally:
        m.s.close()
        child.wait(timeout=25)
        log.close()
    assert child.returncode == 0


def pack(entries, scope="full_profile"):
    # Exact public SessionBundle representation; each value came from the real
    # source browser below. This is deliberately NOT a source-provider test.
    encoded = {name: json.dumps(value).encode() for name, value in entries.items()}
    manifest = {"provider_id": "zen", "app_display_name": "Zen", "scope": scope,
                "entries": [{"rel_path": name, "mode": 384, "byte_len": len(value), "sha256": hashlib.sha256(value).hexdigest()} for name, value in encoded.items()]}
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=tarfile.GNU_FORMAT) as tar:
        for name, value in {"manifest.json": json.dumps(manifest).encode(), **encoded}.items():
            info = tarfile.TarInfo(name)
            info.size, info.mode = len(value), 384
            tar.addfile(info, io.BytesIO(value))
    return stream.getvalue()


def import_bundle(home, entries, scope="full_profile"):
    home.mkdir(mode=0o700, exist_ok=True)
    p = subprocess.run(["/tmp/zen-receiver", "import", str(home)], input=pack(entries, scope), capture_output=True, timeout=60)
    if p.returncode:
        raise RuntimeError("receiver refused: " + p.stderr.decode())
    return json.loads(p.stdout)


def main():
    global PHASE
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Site)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    origins = ["http://localhost:" + str(server.server_port), "http://127.0.0.1:" + str(server.server_port)]
    result = {"boundary": "normalized receiver only; real disposable Gecko source; no Zen-provider or Keyvault claim", "checks": {}}
    try:
        source = ROOT / "source-profile"
        child, m, log = browser(source, "source")
        cookie_rows, storage, tabs = [], [], []
        for i, origin in enumerate(origins):
            if i:
                handle = m.call("WebDriver:NewWindow", {"type": "tab"})["handle"]
                m.call("WebDriver:SwitchToWindow", {"handle": handle})
            m.call("WebDriver:Navigate", {"url": origin + "/login"})
            m.js("document.querySelector('[name=username]').value=arguments[0];document.querySelector('[name=password]').value=arguments[1];document.querySelector('form').submit();return true;", ["synthetic-user", "Synthetic-only-" + NONCE])
            deadline = time.monotonic() + 15
            while m.js("return location.pathname") != "/account":
                if time.monotonic() > deadline:
                    raise TimeoutError("source login")
                time.sleep(.1)
            assert m.js("return localStorage.getItem('synthetic-session')") == NONCE
            storage.append({"origin": origin, "key": "synthetic-session", "value": NONCE})
            tabs.append(origin + "/account")
            for c in m.call("WebDriver:GetCookies"):
                cookie_rows.append({"host_key": c["domain"], "name": c["name"], "value": base64.b64encode(c["value"].encode()).decode(), "path": c["path"], "expires_utc": 11644473600000000 + int(c["expiry"]) * 1000000, "is_secure": c["secure"], "is_httponly": c["httpOnly"], "samesite": {"None": 0, "Lax": 1, "Strict": 2}[c["sameSite"]]})
        m.call("Marionette:SetContext", {"value": "chrome"})
        source_logins = m.js("const done=arguments[arguments.length-1];(async()=>{const l=Cc['@mozilla.org/login-manager/loginInfo;1'].createInstance(Ci.nsILoginInfo);l.init(arguments[0],arguments[0],null,'synthetic-user',arguments[1],'username','password');await Services.logins.addLoginAsync(l);const all=await Services.logins.getAllLogins();done(all.filter(x=>x.username==='synthetic-user').map(x=>({origin:x.hostname,username:x.username,password:btoa(x.password),signon_realm:x.hostname+'/'})));})().catch(e=>done({error:String(e)}));", [origins[0], "Synthetic-only-" + NONCE], asynchronous=True)
        assert isinstance(source_logins, list) and len(source_logins) == 1
        close_browser(child, m, log)
        assert len(cookie_rows) == 2 and len(tabs) == 2
        result["checks"]["real_source_http_logins"] = True
        PHASE = "guest"
        for case in ("full", "selected-first", "tabs-only"):
            chosen = 1 if case == "selected-first" else 2
            entries = {"zen/tabs.json": tabs}
            if case != "tabs-only":
                entries.update({"cookies.json": cookie_rows[:chosen], "localstorage.json": storage[:chosen]})
            home = ROOT / case
            imported = import_bundle(home, entries, "tabs_only" if case == "tabs-only" else "full_profile")
            save(case + "-import.json", imported)
            assert imported["provider"] == "zen" and imported["program"] == "firefox"
            assert imported["tabs"] == tabs
            assert not (home / ".mozilla/firefox/profiles.ini").exists()
            child, m, log = browser(pathlib.Path(imported["profile"]), case)
            states = []
            for i, url in enumerate(imported["tabs"]):
                if i:
                    handle = m.call("WebDriver:NewWindow", {"type": "tab"})["handle"]
                    m.call("WebDriver:SwitchToWindow", {"handle": handle})
                m.call("WebDriver:Navigate", {"url": url})
                state = m.js("return {url:location.href,text:document.body.innerText,storage:localStorage.getItem('synthetic-session')}")
                expected = case != "tabs-only" and i < chosen
                assert ("Signed in" in state["text"]) == expected, state
                assert state["storage"] == (NONCE if expected else None), state
                states.append(state)
                screenshot = m.call("WebDriver:TakeScreenshot", {"full": False, "scroll": False})["value"]
                png = base64.b64decode(screenshot)
                assert png.startswith(b"\x89PNG\r\n\x1a\n")
                (OUT / (case + "-site" + str(i + 1) + ".png")).write_bytes(png)
            save(case + "-states.json", states)
            save(case + "-callbacks.json", [e for e in EVENTS if e.get("phase") == "guest"])
            if case == "full":
                # Generate the DESTINATION's own NSS key with its real login
                # service using a separate disposable synthetic login. No key
                # from the source is copied or accepted by the receiver.
                m.call("Marionette:SetContext", {"value": "chrome"})
                created = m.js("const done=arguments[arguments.length-1];(async()=>{const l=Cc['@mozilla.org/login-manager/loginInfo;1'].createInstance(Ci.nsILoginInfo);l.init(arguments[0],arguments[0],null,'destination-key-only','Discard-only','username','password');await Services.logins.addLoginAsync(l);done(true);})().catch(e=>done({error:String(e)}));", [origins[0]], asynchronous=True)
                save("destination-key-initialization.json", created)
                assert created is True, created
            close_browser(child, m, log)
            if case == "full":
                key = pathlib.Path(imported["profile"]) / "key4.db"
                before = hashlib.sha256(key.read_bytes()).hexdigest()
                logins_import = import_bundle(home, {"logins.json": source_logins})
                assert not logins_import["notices"], logins_import
                assert hashlib.sha256(key.read_bytes()).hexdigest() == before
                child, m, log = browser(pathlib.Path(imported["profile"]), "saved-login")
                m.call("Marionette:SetContext", {"value": "chrome"})
                verified = m.js("const done=arguments[arguments.length-1];Services.logins.getAllLogins().then(all=>done(all.some(x=>x.username==='synthetic-user'&&x.password===arguments[0]))).catch(e=>done({error:String(e)}));", ["Synthetic-only-" + NONCE], asynchronous=True)
                assert verified is True, verified
                close_browser(child, m, log)
                result["checks"]["real_destination_nss_saved_login"] = True
            expected_hosts = {urllib.parse.urlsplit(origins[i]).netloc for i in range(chosen if case != "tabs-only" else 0)}
            callbacks = [e for e in EVENTS if e.get("phase") == "guest" and e.get("authenticated") and e.get("query", {}).get("storage") == [NONCE]]
            assert expected_hosts == {e["host"] for e in callbacks}
            subprocess.run(["/tmp/zen-receiver", "wipe", str(home)], check=True, capture_output=True, timeout=30)
            if case != "tabs-only":
                assert not (pathlib.Path(imported["profile"]) / "cookies.sqlite").exists()
            # A tabs-only destination's cookies.sqlite was created by Firefox,
            # not imported: the ledger must NOT claim/delete that neighbour.
            result["checks"][case + "_browser_and_callbacks"] = True
            EVENTS.clear()
    except BaseException as e:
        result["failure"] = str(e)
        result["traceback"] = traceback.format_exc()
        raise
    finally:
        for child in CHILDREN:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=8)
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
        result["events"] = EVENTS
        result["cleanup"] = {"children_exited": all(p.poll() is not None for p in CHILDREN), "server_closed": server.socket.fileno() == -1, "thread_stopped": not thread.is_alive()}
        save("result.json", result)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
