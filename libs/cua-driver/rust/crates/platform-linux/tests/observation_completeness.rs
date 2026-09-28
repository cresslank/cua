//! The Linux walker has no exhaustion proof yet. Budget/trust flags alone must
//! never turn an omitted AT-SPI subtree into authoritative absence.
use cua_driver_contract::{StatePredicate, UnknownReason, VerificationStatus};
use cua_driver_core::expectation::{evaluate_predicates, ObservationSnapshot};
use serde_json::json;

#[test]
fn linux_partial_observations_keep_missing_elements_unknown() {
    let tools = include_str!("../src/tools/impl_.rs");
    let assignments: Vec<_> = tools
        .lines()
        .filter(|line| line.contains("structured[\"elements_complete\"] ="))
        .map(str::trim)
        .collect();
    assert_eq!(
        assignments,
        ["structured[\"elements_complete\"] = json!(false);"]
    );

    let predicate: StatePredicate = serde_json::from_value(json!({
        "element": {"selector": {"role": "button", "label_contains": "Omitted"}, "exists": true}
    }))
    .unwrap();
    let mut snapshot = ObservationSnapshot {
        window: Some(json!({"pid": 42, "window_id": 7})),
        elements: Some(vec![json!({"role": "button", "label": "Visible sibling"})]),
        element_source_trusted: true,
        elements_complete: false,
    };
    let outcomes = evaluate_predicates(std::slice::from_ref(&predicate), &snapshot);
    assert_eq!(outcomes[0].status, VerificationStatus::Unknown);
    assert_eq!(
        outcomes[0].unknown_reason,
        Some(UnknownReason::ObservationUnavailable)
    );
    // Show the consequence of the erroneous completeness claim, not merely
    // that the observation parser accepts a false flag.
    snapshot.elements_complete = true;
    let outcomes = evaluate_predicates(&[predicate], &snapshot);
    assert_eq!(outcomes[0].status, VerificationStatus::Unsatisfied);
}
