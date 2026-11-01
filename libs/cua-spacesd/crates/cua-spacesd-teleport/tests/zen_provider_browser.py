#!/usr/bin/env python3
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.

"""Guest browser oracle for actual ZenProvider bundles, not synthetic payloads."""
import base64
import hashlib
import http.server
import json
import pathlib
import subprocess
import threading
import time
import traceback
import urllib.parse

import zen_guest_browser as b

BUNDLES = pathlib.Path("/tmp/provider-bundles")


def imported(home, name):
    home.mkdir(mode=0o700, exist_ok=True)
    p = subprocess.run(["/tmp/zen-receiver", "import", str(home)],
                       input=(BUNDLES / (name + ".tar")).read_bytes(), capture_output=True, timeout=60)
    if p.returncode:
        raise RuntimeError("receiver refused real provider bundle: " + p.stderr.decode())
    return json.loads(p.stdout)


def main():
    config = json.loads((BUNDLES / "fixture.json").read_text())
    origins = config["origins"]
    parsed = [urllib.parse.urlsplit(o) for o in origins]
    assert [u.hostname for u in parsed] == ["localhost", "127.0.0.1"]
    assert parsed[0].port == parsed[1].port
    b.NONCE, b.PHASE = config["nonce"], "guest"
    server = http.server.ThreadingHTTPServer(("127.0.0.1", parsed[0].port), b.Site)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    result = {"boundary": "actual retained Zen -> public ZenProvider.capture_selected -> bundle -> public receiver -> Firefox; NO Keyvault/polkit approval", "checks": {}, "source_capabilities": config["source_capabilities"]}
    try:
        for case in ("full", "selected-first", "tabs-only", "defaults"):
            b.EVENTS.clear()
            home = b.ROOT / case
            outcome = imported(home, case)
            b.save(case + "-import.json", outcome)
            assert outcome["provider"] == "zen" and outcome["program"] == "firefox"
            assert outcome["tabs"] == [o + "/account" for o in origins]
            assert not (home / ".mozilla/firefox/profiles.ini").exists()
            assert not outcome["notices"], outcome
            child, m, log = b.browser(pathlib.Path(outcome["profile"]), case)
            states = []
            for i, url in enumerate(outcome["tabs"]):
                if i:
                    handle = m.call("WebDriver:NewWindow", {"type": "tab"})["handle"]
                    m.call("WebDriver:SwitchToWindow", {"handle": handle})
                m.call("WebDriver:Navigate", {"url": url})
                state = m.js("return {url:location.href,text:document.body.innerText,storage:localStorage.getItem('synthetic-session'),scriptCookie:document.cookie}")
                expected = case == "full" or (case == "selected-first" and i == 0)
                assert state["url"] == url
                assert ("Signed in" in state["text"]) == expected, state
                assert state["storage"] == (b.NONCE if expected else None), state
                cookies = m.call("WebDriver:GetCookies")
                assert len(cookies) == int(expected), cookies
                if expected:
                    cookie = cookies[0]
                    assert cookie["name"] == "session" and cookie["value"] == b.NONCE
                    assert cookie["httpOnly"] and cookie["sameSite"] == "Lax" and cookie["path"] == "/"
                    assert cookie["expiry"] > time.time(), "retained persistent cookie expired; regenerate the real Zen fixture"
                    assert not state["scriptCookie"], "HttpOnly cookie exposed to JS"
                states.append({**state, "cookies": cookies})
                png = base64.b64decode(m.call("WebDriver:TakeScreenshot", {"full": False, "scroll": False})["value"], validate=True)
                assert png.startswith(b"\x89PNG\r\n\x1a\n") and len(png) > 1000
                (b.OUT / (case + "-site" + str(i + 1) + ".png")).write_bytes(png)
            # Inspect the browser, not just the requested launch URL list.
            handles = m.call("WebDriver:GetWindowHandles")
            assert len(handles) == 2, handles
            urls = []
            for handle in handles:
                m.call("WebDriver:SwitchToWindow", {"handle": handle})
                urls.append(m.js("return location.href"))
            assert sorted(urls) == sorted(outcome["tabs"])
            b.save(case + "-states.json", states)
            m.call("Marionette:SetContext", {"value": "chrome"})
            logins = m.js("const done=arguments[arguments.length-1];Services.logins.getAllLogins().then(x=>done(x.map(y=>({origin:y.hostname,username:y.username})))).catch(e=>done({error:String(e)}));", asynchronous=True)
            assert logins == [], logins
            if case == "full" and (BUNDLES / "logins.tar").exists():
                created = m.js("const done=arguments[arguments.length-1];(async()=>{const l=Cc['@mozilla.org/login-manager/loginInfo;1'].createInstance(Ci.nsILoginInfo);l.init(arguments[0],arguments[0],null,'destination-key-only','Discard-only','username','password');await Services.logins.addLoginAsync(l);done(true);})().catch(e=>done({error:String(e)}));", [origins[0]], asynchronous=True)
                assert created is True, created
            b.close_browser(child, m, log)
            if case == "full" and (BUNDLES / "logins.tar").exists():
                key = pathlib.Path(outcome["profile"]) / "key4.db"
                before = hashlib.sha256(key.read_bytes()).hexdigest()
                login_outcome = imported(home, "logins")
                assert not login_outcome["notices"], login_outcome
                assert hashlib.sha256(key.read_bytes()).hexdigest() == before
                child, m, log = b.browser(pathlib.Path(outcome["profile"]), "saved-login")
                m.call("Marionette:SetContext", {"value": "chrome"})
                verified = m.js("const done=arguments[arguments.length-1];Services.logins.getAllLogins().then(x=>done(x.some(y=>y.hostname===arguments[0]&&y.username==='synthetic-user'&&y.password===arguments[1]))).catch(e=>done({error:String(e)}));", [origins[0], "Synthetic-only-" + b.NONCE], asynchronous=True)
                assert verified is True, verified
                b.close_browser(child, m, log)
                result["checks"]["real_source_to_independent_destination_saved_login"] = True
            expected_hosts = {urllib.parse.urlsplit(origins[i]).netloc for i in range(2 if case == "full" else 1 if case == "selected-first" else 0)}
            deadline = time.monotonic() + 5
            while len(b.EVENTS) < 2 and time.monotonic() < deadline:
                time.sleep(.05)
            callbacks = [e for e in b.EVENTS if e.get("phase") == "guest"]
            assert {e["host"] for e in callbacks} == {u.netloc for u in parsed}, callbacks
            assert {e["host"] for e in callbacks if e["authenticated"]} == expected_hosts, callbacks
            assert {e["host"] for e in callbacks if e["query"].get("storage") == [b.NONCE]} == expected_hosts, callbacks
            b.save(case + "-callbacks.json", callbacks)
            subprocess.run(["/tmp/zen-receiver", "wipe", str(home)], check=True, capture_output=True, timeout=30)
            if case in ("full", "selected-first"):
                assert not (pathlib.Path(outcome["profile"]) / "cookies.sqlite").exists()
            result["checks"][case + "_cookies_storage_tabs_callbacks_screenshots"] = True
        result["saved_login_capture"] = config["provider"]["saved_logins"]
        if config.get("require_saved_login"):
            assert result["checks"].get("real_source_to_independent_destination_saved_login"), result["saved_login_capture"]
    except BaseException as e:
        result["failure"], result["traceback"] = str(e), traceback.format_exc()
        raise
    finally:
        for child in b.CHILDREN:
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
        result["cleanup"] = {"children_exited": all(c.poll() is not None for c in b.CHILDREN), "server_closed": server.socket.fileno() == -1, "thread_stopped": not thread.is_alive()}
        b.save("result.json", result)
    assert all(result["cleanup"].values()), result["cleanup"]
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
