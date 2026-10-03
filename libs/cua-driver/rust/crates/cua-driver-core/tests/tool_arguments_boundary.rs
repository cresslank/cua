//! Regression coverage for the canonical dispatch boundary's handling of
//! malformed `arguments` values.
//!
//! The MCP contract types `arguments` as an object and `Request::tool_call`
//! substitutes `{}` when it is absent, but a client can still send any JSON
//! value. Found by the `registry_invoke` fuzz target: a non-object value for a
//! session-selecting tool reached an index assignment inside
//! `ToolRegistry::invoke_authorized` and panicked the dispatcher.

use cua_driver_core::authorization::PermissionMode;
use cua_driver_core::protocol::ToolResult;
use cua_driver_core::session_authorization::{
    EffectiveAuthorizationContext, SessionAuthorizationRegistry, SessionModeCeiling,
};
use cua_driver_core::tool::{Tool, ToolDef, ToolRegistry};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

struct Stub(ToolDef);

#[async_trait::async_trait]
impl Tool for Stub {
    fn def(&self) -> &ToolDef {
        &self.0
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        ToolResult::text("stub")
    }
}

fn unrestricted_context() -> Arc<EffectiveAuthorizationContext> {
    let ceiling = SessionModeCeiling::for_trusted_sessions(
        [PermissionMode::Unrestricted],
        true,
        Duration::from_secs(60),
        Duration::from_secs(30),
    )
    .unwrap();
    SessionAuthorizationRegistry::with_ceiling(ceiling)
        .compatibility_context(PermissionMode::Unrestricted, None)
        .unwrap()
}

fn registry_with(name: &str) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(Stub(ToolDef {
        name: name.to_owned(),
        description: "stub".to_owned(),
        input_schema: json!({"type": "object", "properties": {}}),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })));
    let registry = Arc::new(registry);
    registry.init_self_weak();
    registry
}

#[tokio::test]
async fn non_object_arguments_for_a_session_selecting_tool_are_rejected_not_panicked() {
    let registry = registry_with("get_session");
    for arguments in [
        json!(true),
        json!(null),
        json!(7),
        json!("s"),
        json!([1, 2]),
    ] {
        let result = registry
            .invoke_with_context("get_session", arguments.clone(), unrestricted_context())
            .await;
        assert_eq!(
            result.is_error,
            Some(true),
            "arguments {arguments} were accepted"
        );
        let structured = result.structured_content.expect("structured refusal");
        assert_eq!(
            structured["code"], "invalid_arguments",
            "arguments {arguments}"
        );
        assert_eq!(structured["tool"], "get_session");
    }
}

#[tokio::test]
async fn non_object_arguments_for_an_ordinary_tool_are_rejected_before_dispatch() {
    let registry = registry_with("get_screen_size");
    let result = registry
        .invoke_with_context("get_screen_size", json!("nope"), unrestricted_context())
        .await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(
        result.structured_content.unwrap()["code"],
        "invalid_arguments"
    );
}

#[tokio::test]
async fn unknown_tool_still_wins_over_malformed_arguments() {
    let registry = registry_with("get_screen_size");
    let result = registry
        .invoke_with_context("no_such_tool", json!(true), unrestricted_context())
        .await;
    assert_eq!(result.is_error, Some(true));
    let text = serde_json::to_string(&result.content).unwrap();
    assert!(text.contains("Unknown tool"), "{text}");
}

// Exercise the fork compatibility shim through real dispatch and token
// resolution, without native providers or host input.
struct ElementPayload;
impl cua_driver_core::snapshot_store::SnapshotPayload for ElementPayload {
    type Element = usize;
    fn len(&self) -> usize {
        2
    }
    fn retain(&self, index: usize) -> Option<usize> {
        (index < 2).then_some(index)
    }
}

struct ElementAction {
    def: ToolDef,
    snapshots: cua_driver_core::snapshot_store::SnapshotStore<ElementPayload>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for ElementAction {
    fn def(&self) -> &ToolDef {
        &self.def
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        match self.snapshots.resolve(std::process::id() as i32, &args) {
            Ok(cua_driver_core::element_token::ResolvedElement::Element {
                element_index, ..
            }) => {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ToolResult::text("acted").with_structured(json!({"index": element_index}))
            }
            Ok(_) => ToolResult::error("expected element"),
            Err(refusal) => refusal,
        }
    }
}

#[tokio::test]
async fn legacy_element_index_only_cross_checks_a_current_token() {
    let context = unrestricted_context();
    let (snapshots, token) =
        cua_driver_core::tool::with_runtime_scope(context.runtime_scope_key(), || {
            let snapshots = cua_driver_core::snapshot_store::SnapshotStore::new();
            let id = snapshots.publish(std::process::id() as i32, 41, ElementPayload);
            (snapshots, cua_driver_core::element_token::token_for(id, 1))
        });
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ElementAction {
        def: ToolDef {
            name: "click".into(),
            description: "test".into(),
            input_schema: json!({"type":"object", "properties": {
                "element_token": {"type":"string"}
            }, "additionalProperties":false}),
            read_only: false,
            destructive: false,
            idempotent: true,
            open_world: false,
        },
        snapshots,
        calls: calls.clone(),
    }));
    let registry = Arc::new(registry);
    registry.init_self_weak();
    for args in [
        json!({"element_token":token}),
        json!({"element_token":token,"element_index":1}),
    ] {
        let result = registry
            .invoke_with_context("click", args, context.clone())
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
    }
    for bad_index in [json!(0), json!(-1), json!("1"), json!(null), json!(1.5)] {
        let result = registry
            .invoke_with_context(
                "click",
                json!({
                    "element_token":token, "element_index":bad_index
                }),
                context.clone(),
            )
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["refusal"]["code"],
            "stale_element_token"
        );
    }
    for args in [
        json!({"element_index":1}),
        json!({"element_index":1,"element_token":null}),
        json!({"element_token":token,"element_index":1,"snapshot_id":"s1"}),
        json!({"element_token":token,"element_index":1,"unexpected":true}),
    ] {
        let result = registry
            .invoke_with_context("click", args.clone(), context.clone())
            .await;
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["refusal"]["code"], "invalid_arguments");
        if args.get("element_token").and_then(Value::as_str).is_none() {
            assert!(structured
                .to_string()
                .contains("element_token from get_window_state"));
        }
    }
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "refusals must not act"
    );
}

struct ConfigCall {
    def: ToolDef,
    seen: Arc<std::sync::Mutex<Value>>,
}
#[async_trait::async_trait]
impl Tool for ConfigCall {
    fn def(&self) -> &ToolDef {
        &self.def
    }
    async fn invoke(&self, args: Value) -> ToolResult {
        *self.seen.lock().unwrap() = args;
        ToolResult::text("configured")
    }
}

#[tokio::test]
async fn anonymous_config_scope_survives_lifecycle_stamping_but_cannot_be_forged() {
    use cua_driver_core::tool::TrustedInvocationEvidence;
    let seen = Arc::new(std::sync::Mutex::new(json!({})));
    let mut registry = ToolRegistry::new();
    for name in ["get_config", "set_config"] {
        registry.register(Box::new(ConfigCall {
            def: ToolDef {
                name: name.into(), description: "test".into(),
                input_schema: json!({"type":"object", "properties":{}, "additionalProperties":false}),
                read_only: name == "get_config", destructive: false, idempotent: true, open_world: false,
            }, seen: seen.clone(),
        }));
    }
    let registry = Arc::new(registry);
    registry.init_self_weak();
    let context = unrestricted_context();
    let refusal = registry
        .invoke_with_context(
            "get_config",
            json!({
                "element_token": "s00000001:e1", "element_index": 1
            }),
            context.clone(),
        )
        .await;
    assert_eq!(
        refusal.structured_content.unwrap()["refusal"],
        json!({
            "code":"invalid_arguments", "message":"get_config: unknown argument element_index"
        })
    );
    for name in ["get_config", "set_config"] {
        for (mut args, transport, global) in [
            (json!({}), false, true),
            (
                json!({"session":"named", "_global_config":true}),
                false,
                false,
            ),
            (
                json!({"_session_id":"transport", "_global_config":true}),
                true,
                false,
            ),
        ] {
            let evidence = if transport {
                TrustedInvocationEvidence::extract_from_adapter_args(&mut args)
            } else {
                TrustedInvocationEvidence::default()
            };
            let result = registry
                .invoke_with_context_and_evidence(name, args, context.clone(), evidence)
                .await;
            assert_ne!(result.is_error, Some(true), "{result:?}");
            let seen = seen.lock().unwrap();
            assert_eq!(seen["_global_config"] == true, global);
            assert!(
                seen["_session_id"].is_string(),
                "lifecycle isolation remains enabled"
            );
        }
    }
}
