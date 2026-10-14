//! `run_script` against fake driver tools: results, errors, limits and
//! sandbox-escape attempts.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{
    protocol::{Content, ToolResult},
    tool::{Tool, ToolDef, ToolRegistry},
};

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// Fake app: pid 42, window 7, a "Name" field and a "Save" button.
struct Fake {
    def: ToolDef,
    calls: Calls,
}

#[async_trait]
impl Tool for Fake {
    fn def(&self) -> &ToolDef {
        &self.def
    }

    async fn invoke(&self, args: Value) -> ToolResult {
        let name = self.def.name.clone();
        self.calls
            .lock()
            .unwrap()
            .push((name.clone(), args.clone()));
        match name.as_str() {
            "list_windows" => ToolResult::text("windows").with_structured(json!({"windows": [{
                "pid": 42, "window_id": 7, "app_name": "Notes", "title": "Draft",
                "z_index": 1, "is_on_screen": true,
                "bounds": {"x": 0.0, "y": 0.0, "width": 800.0, "height": 600.0}
            }]})),
            "list_apps" => ToolResult::text("apps").with_structured(json!({"apps": [
                {"pid": 42, "name": "Notes", "bundle_id": "com.example.notes", "running": true, "active": true}
            ]})),
            "get_window_state" => ToolResult::text("- [0] AXWindow \"Draft\"").with_structured(json!({
                "pid": 42, "window_id": 7, "snapshot_id": "s00000009",
                "tree_markdown": "- [0] AXWindow \"Draft\"\n  - [1] AXTextField \"Name\"\n  - [2] AXButton \"Save\"\n  - AXStaticText \"Saved\"\n",
                "elements": [
                    {"element_index": 1, "element_token": "s00000009:1", "role": "AXTextField", "label": "Name", "value": "Ada", "depth": 1},
                    {"element_index": 2, "element_token": "s00000009:2", "role": "AXButton", "label": "Save", "depth": 1}
                ]
            })),
            "launch_app" => ToolResult::text("launched").with_structured(json!({"pid": 42})),
            _ if args["fail"] == true => ToolResult::error(format!("{name} refused: asked to fail")),
            _ => ToolResult::text(format!("{name} done")),
        }
    }
}

struct Harness {
    registry: Arc<ToolRegistry>,
    calls: Calls,
}

impl Harness {
    fn new() -> Self {
        let calls: Calls = Arc::default();
        let mut registry = ToolRegistry::new();
        for name in crate::batch_tools::BATCHABLE_TOOLS
            .iter()
            .copied()
            .chain(super::READ_TOOLS.iter().copied())
            .chain(["kill_app"])
        {
            registry.register(Box::new(Fake {
                def: ToolDef {
                    name: name.into(),
                    description: "fake".into(),
                    input_schema: json!({"type": "object"}),
                    read_only: matches!(name, "get_window_state" | "list_windows" | "list_apps"),
                    destructive: false,
                    idempotent: false,
                    open_world: false,
                },
                calls: calls.clone(),
            }));
        }
        registry.register_session_tools();
        registry.register_script_tool();
        let registry = Arc::new(registry);
        registry.init_self_weak();
        Self { registry, calls }
    }

    async fn run(&self, script: &str) -> ToolResult {
        self.run_with(json!({ "script": script })).await
    }

    async fn run_with(&self, args: Value) -> ToolResult {
        self.registry.invoke(super::RUN_SCRIPT_TOOL, args).await
    }

    fn calls(&self, tool: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == tool)
            .map(|(_, args)| args.clone())
            .collect()
    }
}

fn text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn structured(result: &ToolResult) -> Value {
    result.structured_content.clone().unwrap_or(Value::Null)
}

#[tokio::test]
async fn a_script_acts_by_name_and_returns_value_console_and_call_log() {
    let harness = Harness::new();
    let result = harness
        .run(
            r#"const app = await cua.getApp("Notes");
console.log("found", app.name, {window: app.windowId});
await app.setValue({role: "textfield", name: "Name"}, "Grace");
await app.click({role: "button", name: "Save"});
const [field] = await app.query({role: "textbox", name: "name"});
await app.waitFor({text: "Saved"});
return {value: field.value, platform: cua.computer.target};"#,
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let out = structured(&result);
    assert_eq!(out["value"]["value"], "Ada");
    assert_eq!(out["console"][0], "found Notes {\"window\":7}");
    let click = &harness.calls("click")[0];
    assert_eq!(click["element_token"], "s00000009:2");
    assert_eq!(click["pid"], 42);
    assert_eq!(click["window_id"], 7);
    assert!(
        click.get("role").is_none(),
        "selector keys never reach the tool"
    );
    assert_eq!(harness.calls("set_value")[0]["value"], "Grace");
    let log = out["calls"].as_array().unwrap();
    assert!(
        log.iter()
            .any(|entry| entry["op"] == "click" && entry["line"] == 4),
        "{log:?}"
    );
    assert!(
        text(&result).contains("3. line 4 click ok"),
        "{}",
        text(&result)
    );
}

#[tokio::test]
async fn an_uncaught_driver_error_names_the_line_the_call_and_the_reason() {
    let harness = Harness::new();
    let result = harness
        .run("const app = await cua.getApp('Notes');\n\nawait app.click({role: 'button', name: 'Publish'}, {timeout_ms: 0});\nreturn 1;")
        .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.starts_with("run_script: ERROR (exception) at line 3, call 2 (click): click failed: no button \"Publish\""),
        "{message}"
    );
    assert!(
        message.contains("nearest: [2] AXButton \"Save\""),
        "{message}"
    );
    let error = &structured(&result)["error"];
    assert_eq!(error["line"], 3);
    assert_eq!(error["code"], "not_found");
    assert!(harness.calls("click").is_empty());
}

#[tokio::test]
async fn a_script_can_catch_a_failed_call_and_carry_on() {
    let harness = Harness::new();
    let result = harness
        .run("const app = await cua.getApp('Notes');\ntry { await app.click('s00000009:2', {fail: true}); } catch (e) { console.log(e.code, e.op); }\nawait app.pressKey('cmd+s');\nreturn 'ok';")
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(structured(&result)["console"][0], "tool_error click");
    assert_eq!(harness.calls("hotkey")[0]["keys"], json!(["cmd", "s"]));
}

#[tokio::test]
async fn the_sandbox_has_no_io_and_no_way_around_the_api() {
    let harness = Harness::new();
    let result = harness
        .run(
            r#"const probes = {};
for (const name of ["require", "process", "fetch", "std", "os", "XMLHttpRequest", "WebSocket", "setTimeout", "Deno", "Bun", "__host", "__done", "__log", "__settle"]) {
  probes[name] = typeof globalThis[name];
}
probes.viaFunction = typeof Function("return this")().__host;
probes.viaConstructor = (() => {}).constructor.constructor("return typeof process")();
try { await import("fs"); probes.import = "loaded"; } catch (e) { probes.import = String(e); }
try { cua.call = null; probes.frozen = cua.call === null ? "mutated" : "frozen"; } catch (e) { probes.frozen = "frozen"; }
return probes;"#,
        )
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let probes = &structured(&result)["value"];
    for name in [
        "require",
        "process",
        "fetch",
        "std",
        "os",
        "XMLHttpRequest",
        "WebSocket",
        "setTimeout",
        "Deno",
        "Bun",
        "__host",
        "__done",
        "__log",
        "__settle",
        "viaFunction",
    ] {
        assert_eq!(probes[name], "undefined", "{name}: {probes}");
    }
    assert_eq!(probes["viaConstructor"], "undefined");
    assert!(
        probes["import"]
            .as_str()
            .unwrap()
            .contains("could not load module"),
        "{probes}"
    );
    assert_eq!(probes["frozen"], "frozen");
}

#[tokio::test]
async fn only_driver_tools_on_the_allowlist_are_reachable() {
    let harness = Harness::new();
    for (script, needle) in [
        (
            "await cua.call('kill_app', {pid: 42})",
            "not available to scripts",
        ),
        (
            "await cua.call('run_steps', {steps: []})",
            "not available to scripts",
        ),
        (
            "await cua.call('run_actions', {steps: []})",
            "not available to scripts",
        ),
        (
            "await cua.call('run_script', {script: '1'})",
            "not available to scripts",
        ),
        (
            "await cua.call('set_config', {key: 'experimental_script', value: true})",
            "not available to scripts",
        ),
        (
            "await cua.call('click', {pid: 42, _observation_only: true})",
            "reserved",
        ),
        (
            "await cua.call('click', {pid: 42, session: 'someone-else'})",
            "session",
        ),
    ] {
        let result = harness.run(script).await;
        assert_eq!(result.is_error, Some(true), "{script}");
        assert!(
            text(&result).contains(needle),
            "{script}: {}",
            text(&result)
        );
    }
    assert!(harness.calls("kill_app").is_empty());
    assert!(harness.calls("click").is_empty());
}

#[tokio::test]
async fn an_endless_loop_stops_at_the_wall_time_limit() {
    let harness = Harness::new();
    let started = Instant::now();
    let result = harness
        .run_with(json!({"script": "while (true) {}", "timeout_ms": 300}))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured(&result)["error"]["kind"], "timeout");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );

    // A catch block cannot swallow the interrupt.
    let result = harness
        .run_with(json!({"script": "try { while (true) {} } catch (e) {} return 'escaped';", "timeout_ms": 300}))
        .await;
    assert_eq!(structured(&result)["error"]["kind"], "timeout");

    // Nor can a long sleep outlast the limit.
    let started = Instant::now();
    let result = harness
        .run_with(json!({"script": "await cua.sleep(60000); return 'late';", "timeout_ms": 300}))
        .await;
    assert_eq!(
        structured(&result)["error"]["kind"],
        "timeout",
        "{}",
        text(&result)
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn memory_and_stack_are_bounded() {
    let harness = Harness::new();
    let result = harness
        .run("const chunks = []; while (true) chunks.push('x'.repeat(1 << 16));")
        .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        structured(&result)["error"]["kind"],
        "memory",
        "{}",
        text(&result)
    );

    let result = harness
        .run("function down() { return down(); }\ndown();")
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("Maximum call stack size exceeded"),
        "{}",
        text(&result)
    );
    assert_eq!(structured(&result)["error"]["line"], 1);
}

#[tokio::test]
async fn the_driver_call_limit_holds() {
    let harness = Harness::new();
    let result = harness
        .run_with(json!({
            "script": "for (let i = 0; i < 10; i++) await cua.call('press_key', {pid: 42, key: 'a'});",
            "max_calls": 3
        }))
        .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("driver call limit reached (max_calls=3)"),
        "{}",
        text(&result)
    );
    assert_eq!(structured(&result)["error"]["code"], "call_limit");
    assert_eq!(harness.calls("press_key").len(), 3);
}

#[tokio::test]
async fn syntax_errors_and_stalled_promises_are_named() {
    let harness = Harness::new();
    let result = harness.run("const a = 1;\nconst b = ;").await;
    assert_eq!(
        structured(&result)["error"]["kind"],
        "syntax",
        "{}",
        text(&result)
    );
    assert_eq!(structured(&result)["error"]["line"], 2);

    let result = harness.run("await new Promise(() => {});").await;
    assert_eq!(
        structured(&result)["error"]["kind"],
        "stalled",
        "{}",
        text(&result)
    );

    let result = harness.run("throw new TypeError('nope');").await;
    assert!(
        text(&result).contains("ERROR (exception) at line 1: TypeError: nope"),
        "{}",
        text(&result)
    );
}

#[tokio::test]
async fn dropping_the_request_cancels_the_script() {
    let harness = Harness::new();
    let script = "while (true) { await cua.call('list_apps', {}); await cua.sleep(20); }";
    let call = harness.run_with(json!({"script": script, "max_calls": 500, "timeout_ms": 60000}));
    let _ = tokio::time::timeout(Duration::from_millis(300), call).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after_cancel = harness.calls("list_apps").len();
    assert!(after_cancel > 0, "the script ran");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        harness.calls("list_apps").len(),
        after_cancel,
        "no driver call after the request was dropped"
    );
}

#[tokio::test]
async fn bad_requests_are_rejected_before_running() {
    let harness = Harness::new();
    for args in [
        json!({}),
        json!({"script": ""}),
        json!({"script": "1", "timeout_ms": 999999}),
        json!({"script": "1", "max_calls": 0}),
        json!({"script": "1", "extra": true}),
    ] {
        let result = harness.run_with(args.clone()).await;
        assert_eq!(result.is_error, Some(true), "{args}");
    }
}

#[test]
fn the_tool_is_on_unless_the_operator_turns_it_off() {
    use super::disabled_from as off;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    assert_eq!(off(None, None, None), None, "on by default");
    assert_eq!(off(None, None, Some(&config)), None, "no config file");
    std::fs::write(&config, "not json").unwrap();
    assert_eq!(off(None, None, Some(&config)), None, "unreadable config");
    std::fs::write(&config, r#"{"experimental_pip": true}"#).unwrap();
    assert_eq!(off(None, None, Some(&config)), None, "other keys");

    // The opt-outs.
    assert!(off(Some("1"), None, None)
        .unwrap()
        .contains("CUA_DRIVER_DISABLE_RUN_SCRIPT"));
    assert!(off(Some(" TRUE "), None, None).is_some());
    assert_eq!(off(Some("0"), None, None), None);
    assert_eq!(off(Some("maybe"), None, None), None);
    std::fs::write(&config, r#"{"disable_run_script": true}"#).unwrap();
    assert!(off(None, None, Some(&config))
        .unwrap()
        .contains("disable_run_script"));
    std::fs::write(&config, r#"{"disable_run_script": false}"#).unwrap();
    assert_eq!(off(None, None, Some(&config)), None);
    std::fs::write(&config, r#"{"experimental_script": false}"#).unwrap();
    assert!(off(None, None, Some(&config))
        .unwrap()
        .contains("experimental_script"));
}

#[test]
fn the_legacy_opt_in_flag_can_only_turn_the_tool_off() {
    use super::disabled_from as off;
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.json");
    // `=1` and `true` were the old opt-in: now they change nothing.
    assert_eq!(off(None, Some("1"), None), None);
    std::fs::write(&config, r#"{"experimental_script": true}"#).unwrap();
    assert_eq!(off(None, None, Some(&config)), None);
    // ...and they do not override an opt-out.
    assert!(off(Some("1"), Some("1"), Some(&config)).is_some());
    std::fs::write(
        &config,
        r#"{"experimental_script": true, "disable_run_script": true}"#,
    )
    .unwrap();
    assert!(off(None, Some("1"), Some(&config)).is_some());
    std::fs::write(&config, r#"{"experimental_script": false}"#).unwrap();
    assert!(off(None, Some("1"), Some(&config)).is_some());
    // `=0` turns it off, even over a config opt-in.
    std::fs::write(&config, r#"{"experimental_script": true}"#).unwrap();
    assert!(off(None, Some("0"), Some(&config))
        .unwrap()
        .contains("CUA_DRIVER_EXPERIMENTAL_SCRIPT=0"));
    assert!(off(None, Some("false"), None).is_some());
}

#[test]
fn a_default_registry_offers_the_tool() {
    let mut registry = ToolRegistry::new();
    registry.register_session_tools();
    // On unless whoever runs the tests turned it off.
    if super::enabled() {
        assert!(registry.get_def(super::RUN_SCRIPT_TOOL).is_some());
        let listed = registry.tools_list();
        assert!(listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == super::RUN_SCRIPT_TOOL));
    } else {
        assert!(registry.get_def(super::RUN_SCRIPT_TOOL).is_none());
    }
    assert!(registry
        .get_def(crate::batch_tools::RUN_STEPS_TOOL)
        .is_some());
}

#[tokio::test]
async fn query_returns_static_text_rows_with_their_value() {
    let harness = Harness::new();
    let result = harness
        .run("const app = await cua.getApp('Notes');\nconst rows = await app.query({role: 'statictext', name: 'Saved'});\nconst both = await app.query({name: 'Save'});\nreturn {rows, kinds: both.map(r => r.display_only ? 'text' : r.role)};")
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let value = &structured(&result)["value"];
    assert_eq!(value["rows"][0]["role"], "AXStaticText");
    assert_eq!(value["rows"][0]["label"], "Saved");
    assert_eq!(value["rows"][0]["window"]["window_id"], 7);
    assert_eq!(value["kinds"], json!(["AXButton", "text"]));
}

/// A real registry dispatch and owned native worker, without any host GUI.
#[tokio::test]
async fn cancelled_script_retains_native_authority_and_stops_later_actions() {
    use crate::action_lease::{ExactWindow, LeaseRequest};
    use crate::authorization::PermissionMode;
    use crate::element_token::{format_token, TokenRegistry};
    use crate::session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Native {
        def: ToolDef,
        permit: Mutex<Option<crate::element_token::MutationPermit>>,
        release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        started: tokio::sync::Notify,
        calls: AtomicUsize,
        mutations: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Tool for Native {
        fn def(&self) -> &ToolDef {
            &self.def
        }
        async fn invoke(&self, _: Value) -> ToolResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let permit = self.permit.lock().unwrap().take().unwrap();
            let release = self.release.lock().unwrap().take().unwrap();
            let mutations = self.mutations.clone();
            let (entered, running) = tokio::sync::oneshot::channel();
            let worker = crate::blocking::spawn(move || {
                let _permit = permit;
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(10)).unwrap();
                if crate::blocking::check_cancelled().is_ok() {
                    mutations.fetch_add(1, Ordering::SeqCst);
                }
            });
            running.await.unwrap();
            self.started.notify_one();
            worker.await.unwrap();
            ToolResult::text("done")
        }
    }
    let pid = 424_242i32;
    let window_id = 606_000_007u64;
    let tokens = TokenRegistry::default();
    let snapshot = tokens
        .register_snapshot(std::process::id() as i32, window_id, 1)
        .unwrap();
    let (generation, _) = tokens
        .resolve_generation(std::process::id() as i32, &format_token(snapshot, 0))
        .unwrap();
    let permit = tokens.try_acquire_mutation(generation.identity()).unwrap();
    let (release, resume) = std::sync::mpsc::channel();
    let mutations = Arc::new(AtomicUsize::new(0));
    let native = Arc::new(Native {
        def: ToolDef {
            name: "set_value".into(),
            description: "owned fake native mutation".into(),
            input_schema: json!({"type":"object"}),
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: false,
        },
        permit: Mutex::new(Some(permit)),
        release: Mutex::new(Some(resume)),
        started: tokio::sync::Notify::new(),
        calls: AtomicUsize::new(0),
        mutations: mutations.clone(),
    });
    struct Shared(Arc<Native>);
    #[async_trait]
    impl Tool for Shared {
        fn def(&self) -> &ToolDef {
            self.0.def()
        }
        async fn invoke(&self, args: Value) -> ToolResult {
            self.0.invoke(args).await
        }
    }
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(Shared(native.clone())));
    registry.register_script_tool();
    let registry = Arc::new(registry);
    registry.init_self_weak();
    let ceiling = SessionModeCeiling::for_trusted_sessions(
        [PermissionMode::Unrestricted],
        true,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
    .unwrap();
    let context = SessionAuthorizationRegistry::with_ceiling(ceiling)
        .compatibility_context(PermissionMode::Unrestricted, None)
        .unwrap();
    let script = format!("await cua.call('set_value', {{pid:{pid}, window_id:{window_id}, value:'first'}}); await cua.call('set_value', {{pid:{pid}, window_id:{window_id}, value:'later'}});");
    let mut task = tokio::spawn({
        let registry = registry.clone();
        async move {
            registry
                .invoke_with_context("run_script", json!({"script":script}), context)
                .await
        }
    });
    tokio::select! {
        _ = native.started.notified() => {},
        result = &mut task => panic!("script ended before native worker: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(3)) => panic!("native worker did not start"),
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let exact = ExactWindow {
        pid: i64::from(pid),
        window_id,
    };
    let refused = crate::action_lease::global()
        .acquire(LeaseRequest::window_semantic(exact, Duration::ZERO))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), "target_busy");
    // The worker owns the actual MutationPermit, not a copied native pointer.
    assert!(tokens.contains_generation(generation.identity()));
    release.send(()).unwrap();
    let _lease = crate::action_lease::global()
        .acquire(LeaseRequest::window_semantic(exact, Duration::from_secs(3)))
        .await
        .unwrap();
    assert_eq!(native.calls.load(Ordering::SeqCst), 1);
    assert_eq!(mutations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn host_calls_reenter_original_manifest_and_trusted_evidence() {
    use crate::authorization::PermissionMode;
    use crate::session_authorization::{SessionAuthorizationRegistry, SessionModeCeiling};
    use crate::tool::TrustedInvocationEvidence;
    use std::io::Write;

    let harness = Harness::new();
    let mut manifest_file = tempfile::NamedTempFile::new().unwrap();
    manifest_file.write_all(b"version: 2\nmode: bounded\nexpires_after: 1h\nidle_timeout: 30m\nallow:\n  tools: [run_script, list_apps]\n").unwrap();
    let manifest = Arc::new(crate::session_manifest::load_manifest(manifest_file.path()).unwrap());
    let ceiling = SessionModeCeiling::for_trusted_sessions(
        [PermissionMode::Bounded],
        false,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
    .unwrap();
    let context = SessionAuthorizationRegistry::with_ceiling(ceiling)
        .compatibility_context(PermissionMode::Bounded, Some(manifest))
        .unwrap();
    let mut args = json!({
        "script": "try { await cua.call('list_windows', {}); } catch (e) { return e.code; } return 'unexpected';",
        "_transport_session_id": "script-transport",
        "_cua_browser_download_mcp_host_approved": true
    });
    let evidence = TrustedInvocationEvidence::extract_from_adapter_args(&mut args);
    let result = harness
        .registry
        .invoke_with_context_and_evidence("run_script", args, context, evidence)
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    assert_eq!(structured(&result)["value"], "permission_denied");
    assert!(harness.calls("list_windows").is_empty());
    let ceiling = SessionModeCeiling::for_trusted_sessions(
        [PermissionMode::Unrestricted],
        true,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
    .unwrap();
    let context = SessionAuthorizationRegistry::with_ceiling(ceiling)
        .compatibility_context(PermissionMode::Unrestricted, None)
        .unwrap();
    let mut args = json!({
        "script": "await cua.call('list_apps', {});",
        "_transport_session_id": "script-transport",
        "_cua_browser_download_mcp_host_approved": true
    });
    let evidence = TrustedInvocationEvidence::extract_from_adapter_args(&mut args);
    let result = harness
        .registry
        .invoke_with_context_and_evidence("run_script", args, context, evidence)
        .await;
    assert_ne!(result.is_error, Some(true), "{}", text(&result));
    let calls = harness.calls("list_apps");
    assert_eq!(calls.len(), 1);
    assert!(calls[0]["_transport_session_id"]
        .as_str()
        .unwrap()
        .ends_with(":script-transport"));
    assert_eq!(calls[0]["_cua_browser_download_mcp_host_approved"], true);
}
