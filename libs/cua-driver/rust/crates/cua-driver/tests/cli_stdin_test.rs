//! Stdin parsing belongs at the real CLI/wire boundary, without a desktop runtime.

#![cfg(any(unix, target_os = "windows"))]

use cua_driver_testkit::IsolatedStateRoot;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn cli_call_strips_a_utf8_bom_from_piped_stdin_arguments() {
    let home = IsolatedStateRoot::new().expect("isolated CLI home");
    #[cfg(unix)]
    let socket = home.path().join("d.sock").display().to_string();
    #[cfg(target_os = "windows")]
    let socket = format!(r"\\.\pipe\cua-cli-bom-{}", uuid::Uuid::new_v4());

    // Bind before launching the client. Windows readiness uses WaitNamedPipe;
    // Unix readiness sends a `list` request over a separate connection.
    #[cfg(unix)]
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind fake daemon");
    #[cfg(target_os = "windows")]
    let mut listener = tokio::net::windows::named_pipe::ServerOptions::new()
        .first_pipe_instance(true)
        .create(&socket)
        .expect("bind fake daemon pipe");

    let payload = json!({"session": "synthetic-cli-bom", "capture_scope": "window"});
    let mut calls = Vec::new();
    let server = async {
        loop {
            #[cfg(unix)]
            let (stream, _) = listener.accept().await.expect("accept CLI connection");
            #[cfg(target_os = "windows")]
            let stream = {
                listener.connect().await.expect("connect CLI pipe");
                let next = tokio::net::windows::named_pipe::ServerOptions::new()
                    .create(&socket)
                    .expect("prepare next pipe instance");
                std::mem::replace(&mut listener, next)
            };
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
            let request: Value = serde_json::from_str(&line).expect("daemon request JSON");
            let result = match request["method"].as_str().expect("request method") {
                "list" => json!([]),
                "metadata" => json!(cua_driver_core::daemon::current_daemon_metadata()),
                // The compatibility SDK client closes its own disposable session.
                "session_end" => json!({}),
                "call" => {
                    calls.push(request);
                    json!({"content": [], "structuredContent": {"received": true}})
                }
                method => panic!("unexpected daemon method: {method}"),
            };
            stream
                .write_all(format!("{}\n", json!({"ok": true, "result": result})).as_bytes())
                .await
                .unwrap();
            stream.flush().await.unwrap();
            // Keep the Windows pipe alive until the client consumes the reply.
            let mut trailing = Vec::new();
            stream.read_to_end(&mut trailing).await.unwrap();
            assert!(trailing.is_empty(), "unexpected trailing request bytes");
        }
    };
    let client = async {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_cua-driver"))
            .args(["call", "start_session", "--socket", &socket])
            .envs(home.env())
            .env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "false")
            .env("CUA_DRIVER_RS_UPDATE_CHECK", "false")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn real CLI");
        // PowerShell 5.1 Set-Content -Encoding utf8 prepends this BOM.
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(format!("\u{feff}{payload}").as_bytes())
            .await
            .unwrap();
        drop(stdin);
        child.wait_with_output().await.expect("wait for CLI")
    };
    let output = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::select! {
            _ = server => panic!("fake daemon exited before CLI"),
            output = client => output,
        }
    })
    .await
    .expect("bounded CLI exchange");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        json!({"received": true})
    );
    // Exact wire equality forbids missing/default arguments, extra calls, or
    // accidental argument rewriting. Session lifecycle has its own native tests.
    assert_eq!(
        calls,
        vec![json!({
            "method": "call", "name": "start_session", "args": payload,
            "session_id": "cli-explicit", "observation_origin": "direct", "client_kind": "cli"
        })]
    );
}
