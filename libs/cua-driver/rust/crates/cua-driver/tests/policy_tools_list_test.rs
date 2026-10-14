//! Permission policy must shape the MCP tool roster as well as invocation.

#![cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]

use std::collections::HashSet;
use std::path::Path;

use cua_driver_testkit::RawDriver;
use serde_json::json;

#[test]
#[ignore = "requires an explicitly prepared private accessibility bus; metadata only"]
fn tools_list_hides_policy_denied_tools_and_calls_stay_denied() {
    let directory = tempfile::tempdir().expect("temporary policy directory");
    let policy_path = directory.path().join("policy.yaml");
    // `get_config` is unconditionally allowed via `allow.tools`.
    // `get_agent_cursor_state` is conditionally allowed via `allow.rules` with a
    // constraint.  Both must appear in `tools/list` because `tools/list`
    // should not hide tools that are *potentially* allowed.
    // `list_apps` is explicitly denied and must be absent.
    std::fs::write(
        &policy_path,
        "allow:\n  tools: [get_config]\n  rules:\n    - tool: get_agent_cursor_state\n      constraints:\n        session: {\"allowed\": [\"policy-allowed\"]}\ndeny:\n  tools: [list_apps]\n",
    )
    .expect("write permission policy");
    assert_service_policy(&policy_path);
}

#[test]
#[ignore = "requires an explicitly prepared private accessibility bus; metadata only"]
fn rego_directory_tools_list_and_calls_use_service_policy() {
    let directory = tempfile::tempdir().expect("temporary Rego policy directory");
    std::fs::write(
        directory.path().join("base.rego"),
        "package cua.policy\ndefault allow = false\nallowed_session := \"policy-allowed\"\n",
    )
    .expect("write Rego default module");
    std::fs::write(
        directory.path().join("tools.rego"),
        r#"package cua.policy
allow if {
    input.tool == "get_config"
}
allow if {
    input.tool == "get_agent_cursor_state"
    # Empty args are the documented Rego tools/list sentinel. The constant
    # is supplied by base.rego so this positive path requires both modules.
    object.get(input.arguments, "session", allowed_session) == allowed_session
}
"#,
    )
    .expect("write Rego tool rules module");
    assert_service_policy(directory.path());
}

fn assert_service_policy(policy_path: &Path) {
    // Only serve receives the override. RawDriver starts mcp --socket separately;
    // an ambient override could otherwise conceal broken service propagation.
    assert!(
        std::env::var_os("CUA_DRIVER_POLICY_FILE").is_none(),
        "run without an ambient policy override so the proxy cannot load it"
    );
    let policy = policy_path.display().to_string();
    let mut driver = RawDriver::spawn_with_env(&[("CUA_DRIVER_POLICY_FILE", &policy)])
        .expect("prepared policy service must start successfully");
    assert!(
        driver.daemon_pid().is_some(),
        "must use a real daemon-backed proxy"
    );

    driver.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    let response = driver.recv();
    assert_eq!(response["id"], 1);
    assert!(response["error"].is_null(), "initialize failed: {response}");
    assert!(
        response["result"].is_object(),
        "initialize result: {response}"
    );

    driver.send(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}));
    let response = driver.recv();
    assert_eq!(response["id"], 2);
    assert!(response["error"].is_null(), "tools/list failed: {response}");
    let names: HashSet<&str> = response["result"]["tools"]
        .as_array()
        .expect("tools/list tools array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(
        names.contains("get_config"),
        "unconditionally allowed tool must remain listed"
    );
    assert!(
        names.contains("get_agent_cursor_state"),
        "rule-conditionally allowed metadata tool must remain listed"
    );
    assert!(
        !names.contains("list_apps"),
        "policy-denied tool must not be advertised"
    );

    driver.send(&json!({
        "jsonrpc":"2.0",
        "id":3,
        "method":"tools/call",
        "params":{"name":"list_apps","arguments":{}}
    }));
    let response = driver.recv();
    assert_denied(&response, 3);
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|message| message.contains("Permission denied")),
        "policy-denied invocation must remain rejected: {response}"
    );

    driver.send(&json!({
        "jsonrpc":"2.0", "id":4, "method":"tools/call",
        "params":{"name":"get_config","arguments":{}}
    }));
    let response = driver.recv();
    assert_eq!(response["id"], 4);
    assert!(
        response["error"].is_null(),
        "allowed metadata call: {response}"
    );
    assert!(
        response["result"].is_object(),
        "missing allowed result: {response}"
    );
    assert_ne!(
        response["result"]["isError"], true,
        "allowed metadata call: {response}"
    );
    assert!(
        response["result"]["content"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "allowed get_config must return metadata: {response}"
    );

    // Prove arguments cross the same service boundary without capture or input.
    driver.send(&json!({
        "jsonrpc":"2.0", "id":5, "method":"tools/call",
        "params":{"name":"get_agent_cursor_state","arguments":{"session":"policy-denied"}}
    }));
    assert_denied(&driver.recv(), 5);
    driver.send(&json!({
        "jsonrpc":"2.0", "id":6, "method":"tools/call",
        "params":{"name":"get_agent_cursor_state","arguments":{"session":"policy-allowed"}}
    }));
    let response = driver.recv();
    assert_eq!(response["id"], 6);
    assert!(
        response["error"].is_null(),
        "allowed constrained metadata: {response}"
    );
    assert!(
        response["result"].is_object(),
        "missing constrained result: {response}"
    );
    assert_ne!(
        response["result"]["isError"], true,
        "allowed constrained metadata: {response}"
    );
}

fn assert_denied(response: &serde_json::Value, id: u64) {
    assert_eq!(response["id"], id);
    assert!(
        response["error"].is_null(),
        "unexpected transport error: {response}"
    );
    assert_eq!(response["result"]["isError"], true, "{response}");
    // The daemon rejects at its service policy boundary; the MCP proxy maps
    // that service error to its stable invocation envelope, not a ToolResult
    // produced by an invoked native handler.
    assert_eq!(
        response["result"]["structuredContent"]["code"], "tool_invocation_failed",
        "exact service refusal: {response}"
    );
    assert_eq!(response["result"]["structuredContent"]["exit_code"], 1);
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("Permission denied: user policy:")),
        "must be a policy denial, not an arbitrary native failure: {response}"
    );
}
