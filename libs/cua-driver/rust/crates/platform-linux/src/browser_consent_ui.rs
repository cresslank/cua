//! Exact Linux AT-SPI handling for Chromium's browser-owned debugging consent.

use std::time::{Duration, Instant};

use cua_driver_core::browser::{
    BrowserConsentOutcome, BrowserConsentRequest, BrowserRefusal, BrowserRefusalCode,
};

use crate::atspi::AtspiNode;

fn refusal(code: BrowserRefusalCode, message: impl Into<String>) -> BrowserRefusal {
    BrowserRefusal::new(code, message)
}

fn normalized_text(node: &AtspiNode) -> String {
    [
        node.name.as_deref(),
        node.value.as_deref(),
        node.description.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .trim()
    .to_ascii_lowercase()
}

fn role_is(node: &AtspiNode, accepted: &[&str]) -> bool {
    let role = node.role.trim().to_ascii_lowercase();
    accepted.iter().any(|candidate| role == *candidate)
}

fn trusted_semantic_action(node: &AtspiNode) -> Option<&str> {
    let actions = node
        .actions
        .iter()
        .filter(|action| {
            matches!(
                action.trim().to_ascii_lowercase().as_str(),
                "activate" | "click" | "press"
            )
        })
        .collect::<Vec<_>>();
    (actions.len() == 1).then(|| actions[0].as_str())
}

fn is_in_web_content(nodes: &[AtspiNode], node: &AtspiNode) -> bool {
    let mut parent = node.parent_element_index;
    for _ in 0..nodes.len() {
        let Some(parent_index) = parent else {
            return false;
        };
        let Some(parent_node) = nodes
            .iter()
            .find(|candidate| candidate.element_index == Some(parent_index))
        else {
            return false;
        };
        if role_is(parent_node, &["document web", "document frame", "document"]) {
            return true;
        }
        parent = parent_node.parent_element_index;
    }
    true
}

fn trusted_prompt_nodes(nodes: &[AtspiNode]) -> impl Iterator<Item = &AtspiNode> {
    nodes.iter().filter(|node| {
        !node.in_web_content
            && !role_is(node, &["document web", "document frame", "document"])
            && !is_in_web_content(nodes, node)
    })
}

fn remote_debugging_prompt_present(nodes: &[AtspiNode]) -> bool {
    let has_title =
        trusted_prompt_nodes(nodes).any(|node| normalized_text(node) == "allow remote debugging?");
    let body = trusted_prompt_nodes(nodes)
        .map(normalized_text)
        .collect::<Vec<_>>()
        .join(" ");
    has_title
        && body.contains("external app wants full control")
        && body.contains("saved data, cookies and site data")
        && body.contains("navigate to any url")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExactAllowAction {
    element_index: usize,
    element_key: u64,
    role: String,
    name: String,
    checked: Option<bool>,
    actions: Vec<String>,
    action: String,
}

fn allow_action(node: &AtspiNode) -> ExactAllowAction {
    ExactAllowAction {
        element_index: node.element_index.expect("matched actionable index"),
        element_key: node.element_key,
        role: node.role.clone(),
        name: node.name.clone().unwrap_or_default(),
        checked: node.checked,
        actions: node.actions.clone(),
        action: trusted_semantic_action(node)
            .expect("matched semantic action")
            .to_owned(),
    }
}

fn exact_allow_button(
    nodes: &[AtspiNode],
    bounds: &[(usize, i32, i32, u32, u32)],
) -> Result<Option<ExactAllowAction>, BrowserRefusal> {
    if !remote_debugging_prompt_present(nodes) {
        return Ok(None);
    }
    let matches = trusted_prompt_nodes(nodes)
        .filter(|node| {
            role_is(node, &["push button", "button"])
                && normalized_text(node) == "allow"
                && trusted_semantic_action(node).is_some()
                && node.element_index.is_some()
        })
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        let candidate_bounds = |element_index: usize| {
            bounds
                .iter()
                .find(|(index, _, _, width, height)| {
                    *index == element_index && *width > 0 && *height > 0
                })
                .map(|(_, x, y, width, height)| (*x, *y, *width, *height))
        };
        let first_bounds = candidate_bounds(matches[0].element_index.unwrap());
        let same_physical_control = first_bounds.is_some()
            && matches.iter().all(|candidate| {
                candidate.role == matches[0].role
                    && candidate.actions == matches[0].actions
                    && candidate_bounds(candidate.element_index.unwrap()) == first_bounds
            });
        if same_physical_control {
            return Ok(matches
                .iter()
                .max_by_key(|candidate| candidate.depth)
                .map(|node| allow_action(node)));
        }
    }
    match matches.as_slice() {
        [] => Ok(None),
        [node] => Ok(Some(allow_action(node))),
        _ => Err(refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "multiple exact Allow actions matched the browser consent prompt",
        )
        .with_detail(serde_json::json!({
            "candidates": matches.iter().map(|node| serde_json::json!({
                "element_index": node.element_index,
                "element_key": node.element_key,
                "depth": node.depth,
                "role": node.role,
                "actions": node.actions,
                "parent_element_index": node.parent_element_index,
            })).collect::<Vec<_>>()
        }))),
    }
}

pub async fn handle(
    request: BrowserConsentRequest,
) -> Result<BrowserConsentOutcome, BrowserRefusal> {
    let pid = u32::try_from(request.pid).map_err(|_| {
        refusal(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "the approved browser pid is outside the Linux process-id range",
        )
    })?;
    let target =
        crate::wayland::establish_exact_target(pid, request.window_id).map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserBindingStale,
                format!("the approved browser window is not exact: {error}"),
            )
        })?;
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut saw_prompt = false;
    loop {
        crate::wayland::validate_exact_target(&target).map_err(|error| {
            refusal(
                BrowserRefusalCode::BrowserBindingStale,
                format!("the approved browser window changed before consent: {error}"),
            )
        })?;
        let window_id = request.window_id;
        let tree =
            cua_driver_core::blocking::spawn(move || crate::atspi::walk_tree(pid, window_id, None))
                .await
                .map_err(|error| {
                    refusal(
                        BrowserRefusalCode::BrowserRouteUnavailable,
                        format!("could not inspect the browser consent UI: {error}"),
                    )
                })?;
        let prompt_present = remote_debugging_prompt_present(&tree.nodes);
        saw_prompt |= prompt_present;
        match exact_allow_button(&tree.nodes, &tree.bounds)? {
            Some(allow) => {
                let expected_action = allow.action.clone();
                let action_target = target.clone();
                let result = cua_driver_core::blocking::spawn(move || {
                    // Revalidate the immutable compositor identity in the same
                    // blocking action transaction as the stable semantic key.
                    crate::atspi::perform_verified_action_by_key(
                        &action_target,
                        allow.element_key,
                        &allow.role,
                        &allow.name,
                        allow.checked,
                        &allow.actions,
                        &allow.action,
                    )
                })
                .await
                .map_err(|error| {
                    refusal(
                        BrowserRefusalCode::BrowserRouteUnavailable,
                        format!("could not dispatch the exact browser consent action: {error}"),
                    )
                })?
                .map_err(|error| {
                    refusal(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        format!("the exact browser consent action failed: {error}"),
                    )
                })?;
                if result.0 != expected_action || result.1 {
                    return Err(refusal(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        "the exact browser consent action was not explicitly acknowledged",
                    ));
                }
                // The verified helper re-walks at action time, matches the
                // stable D-Bus object key and complete semantic identity,
                // selects this named action, and rejects do_action(false).
                return Ok(BrowserConsentOutcome::Accepted);
            }
            None if saw_prompt && !prompt_present => {
                return Err(refusal(
                    BrowserRefusalCode::BrowserConsentRevoked,
                    "the browser consent prompt disappeared without an explicitly acknowledged Allow action",
                ));
            }
            None if Instant::now() >= deadline => {
                return Err(refusal(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    format!(
                        "no exact Chromium remote-debugging consent prompt appeared for reconnect attempt {}",
                        request.attempt
                    ),
                ));
            }
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(role: &str, name: &str, actions: &[&str]) -> AtspiNode {
        AtspiNode {
            element_index: (!actions.is_empty()).then_some(7),
            role: role.to_owned(),
            name: Some(name.to_owned()),
            value: None,
            checked: None,
            enabled: None,
            selected: None,
            description: None,
            actions: actions.iter().map(|value| (*value).to_owned()).collect(),
            element_key: 0x77,
            depth: 0,
            parent_element_index: None,
            in_web_content: false,
        }
    }

    fn prompt() -> Vec<AtspiNode> {
        vec![
            node("heading", "Allow remote debugging?", &[]),
            node(
                "static",
                "An external app wants full control. This includes access to your saved data, cookies and site data, and the ability to navigate to any URL.",
                &[],
            ),
            node("push button", "Cancel", &["click"]),
            node("push button", "Allow", &["click"]),
        ]
    }

    #[test]
    fn matcher_returns_stable_key_and_named_action() {
        let allow = exact_allow_button(&prompt(), &[]).unwrap().unwrap();
        assert_eq!(allow.element_index, 7);
        assert_eq!(allow.element_key, 0x77);
        assert_eq!(allow.action, "click");
        assert!(
            exact_allow_button(&[node("push button", "Allow", &["click"])], &[])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn matcher_refuses_ambiguous_allow_actions() {
        let mut nodes = prompt();
        let mut duplicate = node("push button", "Allow", &["press"]);
        duplicate.element_index = Some(8);
        duplicate.element_key = 0x88;
        nodes.push(duplicate);
        assert_eq!(
            exact_allow_button(&nodes, &[]).unwrap_err().code,
            BrowserRefusalCode::BrowserWrongTargetRefused
        );
    }

    #[test]
    fn matcher_refuses_ambiguous_semantic_action_on_one_button() {
        let mut nodes = prompt();
        nodes.last_mut().unwrap().actions = vec!["click".into(), "press".into()];
        assert!(exact_allow_button(&nodes, &[]).unwrap().is_none());
    }

    #[test]
    fn matcher_collapses_duplicate_atspi_paths_for_one_physical_button() {
        let mut nodes = prompt();
        let mut duplicate = node("push button", "Allow", &["click"]);
        duplicate.element_index = Some(8);
        duplicate.element_key = 0x88;
        duplicate.depth = 2;
        nodes.last_mut().unwrap().depth = 1;
        nodes.push(duplicate);
        let bounds = vec![(7, 10, 20, 80, 30), (8, 10, 20, 80, 30)];
        assert_eq!(
            exact_allow_button(&nodes, &bounds)
                .unwrap()
                .unwrap()
                .element_key,
            0x88
        );
    }

    #[test]
    fn matcher_ignores_a_spoofed_prompt_inside_web_content() {
        let mut nodes = prompt();
        let mut document = node("document web", "Example page", &[]);
        document.element_index = Some(42);
        nodes.insert(0, document);
        for child in &mut nodes[1..] {
            child.parent_element_index = Some(42);
            child.in_web_content = true;
        }
        assert!(exact_allow_button(&nodes, &[]).unwrap().is_none());
    }
}
