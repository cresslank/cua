// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Real registry and composite dispatch through the production SDK/exporter,
//! decoded at a disposable HTTP/protobuf collector. Environment cases run in
//! child test processes, never by mutating the parent test runner's environment.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::common::v1::{any_value::Value as WireValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::LogRecord;
use prost::Message;
use serde_json::{json, Value};

use super::{Tool, ToolDef, ToolRegistry};
use crate::protocol::ToolResult;
use crate::session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling};

const PRIVATE: &str = "OTLP-PRIVATE-TEXT-URL-SESSION-CREDENTIAL";
const SUSPENDED_DETAIL: &str =
    "authorization for this Cua runtime has been suspended by revoke-all";

struct SyntheticTool(ToolDef);

#[async_trait]
impl Tool for SyntheticTool {
    fn def(&self) -> &ToolDef {
        &self.0
    }
    async fn invoke(&self, _args: Value) -> ToolResult {
        match self.0.name.as_str() {
            "get_guide" => ToolResult::error(PRIVATE).with_structured(json!({"code": PRIVATE})),
            "list_apps" => ToolResult::text(PRIVATE).with_structured(json!({"apps": []})),
            _ => {
                tokio::time::sleep(Duration::from_millis(2)).await;
                ToolResult::text(PRIVATE)
            }
        }
    }
}

#[tokio::test]
async fn wire_child() {
    let Ok(mode) = std::env::var("CUA_OTLP_TEST_MODE") else {
        return;
    };
    let mut registry = ToolRegistry::new();
    assert_eq!(
        registry.tool_events.is_some(),
        matches!(mode.as_str(), "on" | "outage" | "suspended")
    );
    for name in ["probe", "get_guide", "list_apps", "press_key"] {
        registry.register(Box::new(SyntheticTool(ToolDef {
            name: name.into(),
            description: "synthetic tool".into(),
            input_schema: json!({"type": "object"}),
            read_only: name != "press_key",
            destructive: false,
            idempotent: false,
            open_world: false,
        })));
    }
    registry.register_batch_tools();
    let registry = Arc::new(registry);
    registry.init_self_weak();
    if mode == "suspended" {
        let ceiling = SessionModeCeiling::for_trusted_sessions(
            [crate::authorization::PermissionMode::Standard],
            false,
            Duration::from_secs(60),
            Duration::from_secs(30),
        )
        .unwrap();
        let context = SessionAuthorizationRegistry::with_ceiling(ceiling)
            .compatibility_context(crate::authorization::PermissionMode::Standard, None)
            .unwrap();
        let scope = context.runtime_scope_key();
        assert!(crate::session::suspend_runtime_scope(&scope));
        let result = registry
            .invoke_with_context("probe", json!({"session": PRIVATE}), context)
            .await;
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.as_ref().unwrap();
        assert!(structured.get("code").is_none());
        assert_eq!(structured["status"], "refused");
        assert_eq!(structured["refusal"]["code"], "authorization_suspended");
        assert_eq!(structured["refusal"]["message"], SUSPENDED_DETAIL);
        assert!(crate::session::forget_suspended_runtime_scope(&scope));
        registry.shutdown_tool_event_logs();
        return;
    }
    let result = registry.invoke("probe", json!({"session": PRIVATE})).await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(
        registry.invoke("get_guide", json!({})).await.is_error,
        Some(true)
    );
    assert_eq!(
        registry
            .invoke(PRIVATE, json!({"text": PRIVATE}))
            .await
            .is_error,
        Some(true)
    );
    assert_eq!(
        registry.invoke("probe", Value::Null).await.is_error,
        Some(true)
    );
    #[cfg(feature = "script")]
    {
        let result = registry
            .invoke(
                "run_script",
                json!({
                    "script": format!("console.log('{PRIVATE}'); return await cua.listApps();")
                }),
            )
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
    }
    let result = registry
        .invoke(
            "run_steps",
            json!({
                "steps": [{"press_key": {"pid": 42, "window_id": 7, "key": "return"}}],
                "observe": false
            }),
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{result:?}");
    if mode == "outage" {
        for _ in 0..2048 {
            assert_eq!(
                registry.invoke(PRIVATE, json!({})).await.is_error,
                Some(true)
            );
        }
    }
    let started = Instant::now();
    registry.shutdown_tool_event_logs();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "shutdown exceeded its bounded grace"
    );
}

struct Collector {
    endpoint: String,
    received: mpsc::Receiver<(String, Vec<u8>)>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Collector {
    fn start(response: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1/logs", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let (send, received) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut header = Vec::new();
                        let mut byte = [0];
                        while !header.ends_with(b"\r\n\r\n") {
                            stream.read_exact(&mut byte).unwrap();
                            header.push(byte[0]);
                            assert!(header.len() < 16384);
                        }
                        let header = String::from_utf8(header).unwrap();
                        let len: usize = header
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().unwrap())
                            })
                            .unwrap();
                        assert!(len < 1024 * 1024);
                        let mut body = vec![0; len];
                        stream.read_exact(&mut body).unwrap();
                        send.send((header, body)).unwrap();
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("collector accept: {error}"),
                }
            }
        });
        Self {
            endpoint,
            received,
            stop,
            worker: Some(worker),
        }
    }

    fn requests(&self) -> Vec<(String, Vec<u8>)> {
        self.received.try_iter().collect()
    }
}

impl Drop for Collector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn child(mode: &str, endpoint: Option<&str>, env: &[(&str, &str)]) {
    let home = tempfile::tempdir().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "tool::tool_events_tests::wire_child", "--nocapture"])
        .env_clear().env("HOME", home.path()).env("USERPROFILE", home.path())
        .env("CUA_OTLP_TEST_MODE", mode)
        .env("DO_NOT_TRACK", "1").env("CUA_DRIVER_RS_TELEMETRY_ENABLED", "false")
        .env("OTEL_LOGS_EXPORTER", "otlp")
        .env("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "http/protobuf")
        .env("OTEL_SERVICE_NAME", "cua-wire-test")
        .env("OTEL_RESOURCE_ATTRIBUTES", format!(
            "host.name=fixture,loghouse.project=cua,loghouse.repo=owner/cua,loghouse.source_unit=cua-test.service,service.instance.id=cua-test.service,private={PRIVATE}"))
        .env("OTEL_EXPORTER_OTLP_LOGS_HEADERS", format!("authorization={PRIVATE}"));
    if let Some(endpoint) = endpoint {
        command.env("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", endpoint);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "child {mode}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn attribute<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a WireValue> {
    attrs
        .iter()
        .find(|kv| kv.key == key)?
        .value
        .as_ref()?
        .value
        .as_ref()
}

fn string<'a>(attrs: &'a [KeyValue], key: &str) -> &'a str {
    match attribute(attrs, key).unwrap() {
        WireValue::StringValue(value) => value,
        value => panic!("expected string for {key}: {value:?}"),
    }
}

fn records(requests: &[(String, Vec<u8>)]) -> Vec<LogRecord> {
    let mut records = Vec::new();
    for (header, body) in requests {
        assert!(header.starts_with("POST /v1/logs HTTP/1.1\r\n"));
        assert!(header
            .to_ascii_lowercase()
            .contains("content-type: application/x-protobuf"));
        assert!(header
            .to_ascii_lowercase()
            .contains("user-agent: cua-driver/"));
        assert!(!header.contains(PRIVATE));
        assert!(!body
            .windows(PRIVATE.len())
            .any(|window| window == PRIVATE.as_bytes()));
        let request = ExportLogsServiceRequest::decode(body.as_slice()).unwrap();
        for logs in request.resource_logs {
            let resource = logs.resource.unwrap();
            assert_eq!(
                string(&resource.attributes, "service.name"),
                "cua-wire-test"
            );
            assert_eq!(
                string(&resource.attributes, "service.version"),
                env!("CARGO_PKG_VERSION")
            );
            assert_eq!(string(&resource.attributes, "host.name"), "fixture");
            assert_eq!(
                string(&resource.attributes, "service.instance.id"),
                "cua-test.service"
            );
            assert_eq!(string(&resource.attributes, "loghouse.project"), "cua");
            assert_eq!(
                string(&resource.attributes, "loghouse.source_unit"),
                "cua-test.service"
            );
            assert!(attribute(&resource.attributes, "private").is_none());
            for scope in logs.scope_logs {
                assert_eq!(scope.scope.unwrap().name, "cua-driver.tool-events");
                records.extend(scope.log_records);
            }
        }
    }
    records
}

#[test]
fn http_protobuf_dispatch_privacy_timing_and_nested_parentage() {
    let collector = Collector::start(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    );
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    proxy.set_nonblocking(true).unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    child(
        "on",
        Some(&collector.endpoint),
        &[
            ("HTTP_PROXY", &proxy_url),
            ("http_proxy", &proxy_url),
            ("ALL_PROXY", &proxy_url),
        ],
    );
    assert_eq!(
        proxy.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let logs = records(&collector.requests());
    assert!(logs.len() >= 6, "expected outer and child dispatch records");
    let named = |name: &str| {
        logs.iter()
            .filter(|r| string(&r.attributes, "cua.tool.name") == name)
            .collect::<Vec<_>>()
    };
    assert_eq!(named("unknown").len(), 1);
    assert_eq!(
        string(&named("unknown")[0].attributes, "error.type"),
        "unknown_tool"
    );
    assert_eq!(
        string(&named("get_guide")[0].attributes, "error.type"),
        "tool_error"
    );
    for name in ["unknown", "get_guide"] {
        assert_eq!(
            string(&named(name)[0].attributes, "cua.tool.outcome"),
            "error"
        );
    }
    let probe = named("probe");
    assert_eq!(probe.len(), 2);
    assert!(probe
        .iter()
        .any(|r| string(&r.attributes, "cua.tool.outcome") == "success"));
    assert!(probe.iter().any(|r| {
        string(&r.attributes, "error.type") == "invalid_arguments"
            && string(&r.attributes, "cua.tool.outcome") == "error"
    }));
    let batch = named("run_steps");
    let action = named("press_key");
    assert_eq!(batch.len(), 1);
    assert_eq!(action.len(), 1);
    assert_eq!(batch[0].trace_id, action[0].trace_id);
    assert_eq!(
        string(&action[0].attributes, "cua.tool.parent_span_id"),
        hex(&batch[0].span_id)
    );
    #[cfg(feature = "script")]
    {
        let script = named("run_script");
        let read = named("list_apps");
        assert_eq!(script.len(), 1);
        assert_eq!(read.len(), 1);
        assert_eq!(script[0].trace_id, read[0].trace_id);
        assert_eq!(
            string(&read[0].attributes, "cua.tool.parent_span_id"),
            hex(&script[0].span_id)
        );
    }
    let mut spans = std::collections::HashSet::new();
    for record in logs {
        assert_eq!(record.event_name, "cua.tool.completed");
        assert!(record.body.is_none());
        assert!(record.time_unix_nano > 0);
        assert_eq!(record.trace_id.len(), 16);
        assert!(record.trace_id.iter().any(|b| *b != 0));
        assert_eq!(record.span_id.len(), 8);
        assert!(spans.insert(record.span_id));
        assert!(
            matches!(attribute(&record.attributes, "cua.tool.duration_ns"), Some(WireValue::IntValue(ns)) if *ns > 0)
        );
        assert!(record.attributes.iter().all(|kv| matches!(
            kv.key.as_str(),
            "cua.tool.name"
                | "cua.tool.outcome"
                | "error.type"
                | "cua.tool.duration_ns"
                | "cua.tool.parent_span_id"
        )));
    }
}

#[test]
fn http_protobuf_suspended_runtime_refusal_is_permission_denied_without_detail() {
    let collector = Collector::start(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    );
    child("suspended", Some(&collector.endpoint), &[]);
    let requests = collector.requests();
    let logs = records(&requests);
    assert_eq!(logs.len(), 1);
    let record = &logs[0];
    assert_eq!(string(&record.attributes, "cua.tool.name"), "probe");
    assert_eq!(string(&record.attributes, "cua.tool.outcome"), "refused");
    assert_eq!(
        string(&record.attributes, "error.type"),
        "permission_denied"
    );
    assert!(record.body.is_none());
    for (header, body) in &requests {
        for detail in [SUSPENDED_DETAIL, "authorization_suspended"] {
            assert!(!header.contains(detail));
            assert!(!body
                .windows(detail.len())
                .any(|window| window == detail.as_bytes()));
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn disabled_requires_opt_in_and_explicit_valid_endpoint() {
    let collector = Collector::start("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into());
    for vars in [
        vec![("OTEL_LOGS_EXPORTER", "none")],
        vec![("OTEL_SDK_DISABLED", "true")],
        vec![("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "grpc")],
        vec![("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "")],
        vec![(
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
            "http://user:secret@127.0.0.1/v1/logs",
        )],
    ] {
        child("off", Some(&collector.endpoint), &vars);
    }
    child("off", None, &[]);
    assert!(collector.requests().is_empty());
    child(
        "on",
        None,
        &[(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            collector.endpoint.trim_end_matches("/v1/logs"),
        )],
    );
    assert!(!records(&collector.requests()).is_empty());
}

#[test]
fn outage_and_redirect_never_break_tool_results() {
    let unavailable = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1/logs", unavailable.local_addr().unwrap());
    drop(unavailable);
    child("outage", Some(&endpoint), &[]);
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let redirect = Collector::start(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}/leak\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", target.local_addr().unwrap()));
    child("on", Some(&redirect.endpoint), &[]);
    assert!(!redirect.requests().is_empty());
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
