//! Linux wire-argument compatibility, without any desktop input.
//! Fabricated pid/window targets are refused at the exact-window boundary.
//! This certifies typed target refusal parity, not delivery. Core batch tests
//! separately prove numeric normalization reaches its probe handler.
#![cfg(target_os = "linux")]

use cua_driver_testkit::RawDriver;
use serde_json::{json, Value};

#[test]
fn numeric_and_string_set_value_reach_the_exact_target_boundary_directly_and_in_a_batch() {
    let mut driver =
        RawDriver::spawn_explicit_direct().expect("spawn source-built direct MCP driver");
    driver.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}));
    assert!(driver.recv()["result"].is_object());
    let expected = "window_id 1 is stale or does not belong to pid 1.";
    for value in [
        json!(0),
        json!(-12),
        json!(4.5),
        json!(18446744073709551615u64),
        json!(" 004.50 "),
    ] {
        let arguments = json!({"pid":1,"window_id":1,"value":value});
        driver.send(
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"set_value","arguments":arguments
            }}),
        );
        let direct = driver.recv();
        assert_eq!(direct["result"]["isError"], true, "{direct}");
        assert_eq!(
            direct["result"]["structuredContent"]["code"], "window_target_mismatch",
            "{direct}"
        );
        assert_eq!(
            direct["result"]["structuredContent"]["effect"], "refused",
            "{direct}"
        );
        assert_eq!(direct["result"]["content"][0]["text"], expected, "{direct}");
        for batch_tool in ["run_steps", "run_actions"] {
            driver.send(
                &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                    "name":batch_tool,"arguments":{"steps":[{"tool":"set_value","args":arguments}]}
                }}),
            );
            let batch = driver.recv();
            let result = &batch["result"]["structuredContent"];
            assert_eq!(
                result["steps"][0]["code"], "window_target_mismatch",
                "{batch}"
            );
            assert_eq!(result["steps"][0]["phase"], "action", "{batch}");
            assert_eq!(result["steps"][0]["message"], expected, "{batch}");
        }
    }
    // Direct dispatch preserves exact-target refusal precedence even for bad
    // values. Batch prevalidation must still reject those shapes before any
    // action; the core owner also asserts zero probe invocations.
    for value in [Value::Null, json!(true), json!([]), json!({})] {
        let arguments = json!({"pid":1,"window_id":1,"value":value});
        driver.send(
            &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
                "name":"set_value","arguments":arguments
            }}),
        );
        let direct = driver.recv();
        assert_eq!(direct["result"]["isError"], true, "{direct}");
        assert_eq!(
            direct["result"]["structuredContent"]["code"], "window_target_mismatch",
            "{direct}"
        );
        assert_eq!(direct["result"]["content"][0]["text"], expected, "{direct}");
        for batch_tool in ["run_steps", "run_actions"] {
            driver.send(
                &json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
                    "name":batch_tool,"arguments":{"steps":[
                        {"tool":"set_value","args":{"pid":1,"window_id":1,"value":"valid"}},
                        {"tool":"set_value","args":arguments}
                    ]}
                }}),
            );
            let batch = driver.recv();
            let result = &batch["result"]["structuredContent"];
            assert_eq!(result["code"], "invalid_batch", "{batch}");
            assert_eq!(result["executed"], 0, "{batch}");
            assert_eq!(result["failed_step"], 1, "{batch}");
            assert!(
                result["detail"]
                    .as_str()
                    .unwrap()
                    .starts_with("set_value: invalid arguments at /value:"),
                "{batch}"
            );
        }
    }
}
