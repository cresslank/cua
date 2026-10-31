//! Exact Linux AT-SPI handling for Chromium's browser-owned debugging consent.

use std::time::{Duration, Instant};

use cua_driver_core::browser::platform::BrowserConsentAction;
use cua_driver_core::browser::{
    BrowserConsentOutcome, BrowserConsentRequest, BrowserRefusal, BrowserRefusalCode,
};

use crate::atspi::AtspiNode;

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

fn exact_prompt_button(
    nodes: &[AtspiNode],
    bounds: &[(usize, i32, i32, u32, u32)],
    label: &str,
) -> Result<Option<ExactAllowAction>, BrowserRefusal> {
    if !remote_debugging_prompt_present(nodes) {
        return Ok(None);
    }
    let matches = trusted_prompt_nodes(nodes)
        .filter(|node| {
            role_is(node, &["push button", "button"])
                && normalized_text(node) == label
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
        _ => Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            format!("multiple exact {label} actions matched the browser consent prompt"),
        )
        .with_detail(serde_json::json!({
            "candidates": matches.iter().map(|node| {
                let parent = node.parent_element_index.and_then(|parent_index| {
                    nodes.iter().find(|candidate| candidate.element_index == Some(parent_index))
                });
                let frame = bounds.iter().find(|(index, ..)| node.element_index == Some(*index));
                serde_json::json!({
                    "element_index": node.element_index,
                    "element_key": node.element_key,
                    "depth": node.depth,
                    "role": node.role,
                    "actions": node.actions,
                    "parent_element_index": node.parent_element_index,
                    "parent_role": parent.map(|parent| parent.role.as_str()),
                    "parent_name": parent.and_then(|parent| parent.name.as_deref()),
                    "bounds": frame.map(|(_, x, y, width, height)| serde_json::json!({
                        "x": x, "y": y, "width": width, "height": height,
                    })),
                })
            }).collect::<Vec<_>>()
        }))),
    }
}

fn exact_allow_button(
    nodes: &[AtspiNode],
    bounds: &[(usize, i32, i32, u32, u32)],
) -> Result<Option<ExactAllowAction>, BrowserRefusal> {
    exact_prompt_button(nodes, bounds, "allow")
}

fn exact_cancel_button(
    nodes: &[AtspiNode],
    bounds: &[(usize, i32, i32, u32, u32)],
) -> Result<Option<ExactAllowAction>, BrowserRefusal> {
    exact_prompt_button(nodes, bounds, "cancel")
}

fn exact_button_center(
    bounds: &[(usize, i32, i32, u32, u32)],
    element_index: usize,
) -> anyhow::Result<(i32, i32)> {
    let (_, x, y, width, height) = bounds
        .iter()
        .find(|(index, _, _, width, height)| *index == element_index && *width > 1 && *height > 1)
        .ok_or_else(|| anyhow::anyhow!("the exact Allow action had empty screen bounds"))?;
    let center_x = x
        .checked_add(i32::try_from(width / 2)?)
        .ok_or_else(|| anyhow::anyhow!("Allow button center x overflowed"))?;
    let center_y = y
        .checked_add(i32::try_from(height / 2)?)
        .ok_or_else(|| anyhow::anyhow!("Allow button center y overflowed"))?;
    Ok((center_x, center_y))
}

/// Pointer fallback after an acknowledged accessibility action left the prompt
/// up. Re-walk, match the semantic Allow control, and inject through the
/// carried exact-target proof. Bounds are this walk's frame, not another
/// snapshot, and there is no index-only action path beside the rewalk.
fn trusted_allow_click(target: &crate::wayland::ExactTargetProof) -> anyhow::Result<()> {
    crate::wayland::validate_exact_target(target)?;
    let pid = target.pid();
    let window_id = target.window_id();
    let tree = crate::atspi::walk_tree(pid, window_id, None);
    let allow = exact_allow_button(&tree.nodes, &tree.bounds)
        .map_err(|error| anyhow::anyhow!(error.message))?
        .ok_or_else(|| {
            anyhow::anyhow!("the exact Chromium remote-debugging consent action became stale")
        })?;
    let (center_x, center_y) = exact_button_center(&tree.bounds, allow.element_index)?;
    crate::wayland::validate_exact_target(target)?;
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        // AT-SPI screen bounds are layout coordinates, not desktop-frame
        // points. Keep the carried exact target; click's native pointer space
        // applies the layout origin.
        crate::wayland::click(target.clone(), center_x, center_y, 1, 1)
    } else {
        crate::input::with_x11_foreground(window_id, 80, || {
            crate::wayland::validate_exact_target(target)?;
            crate::input::send_click_xtest_desktop(center_x, center_y, 1, 1)
        })
    }
}

pub fn dismiss(pid: u32, window_id: u64) -> Result<bool, BrowserRefusal> {
    let target = crate::wayland::establish_exact_target(pid, window_id).map_err(|error| {
        BrowserRefusal::new(
            BrowserRefusalCode::BrowserBindingStale,
            format!("the approved browser window is not exact: {error}"),
        )
    })?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut dismissed = false;
    loop {
        crate::wayland::validate_exact_target(&target).map_err(|error| {
            BrowserRefusal::new(
                BrowserRefusalCode::BrowserBindingStale,
                format!("the approved browser window changed before cleanup: {error}"),
            )
        })?;
        let tree = crate::atspi::walk_tree(pid, window_id, None);
        let prompt_present = remote_debugging_prompt_present(&tree.nodes);
        if !prompt_present {
            return Ok(dismissed);
        }
        let cancel = exact_cancel_button(&tree.nodes, &tree.bounds)?.ok_or_else(|| {
            BrowserRefusal::new(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "the exact remote-debugging consent prompt exposed no semantic cancel action",
            )
        })?;
        let expected_action = cancel.action.clone();
        let result = crate::atspi::perform_verified_action_by_key(
            &target,
            cancel.element_key,
            &cancel.role,
            &cancel.name,
            cancel.checked,
            &cancel.actions,
            &cancel.action,
        )
        .map_err(|error| {
            BrowserRefusal::new(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                format!("the exact browser consent cancel action failed: {error}"),
            )
        })?;
        if result.0 != expected_action || result.1 {
            return Err(BrowserRefusal::new(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "the exact browser consent cancel action was not explicitly acknowledged",
            ));
        }
        dismissed = true;
        if Instant::now() >= deadline {
            return Err(BrowserRefusal::new(
                BrowserRefusalCode::BrowserWrongTargetRefused,
                "the remote-debugging consent prompt remained after its exact cancel action",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn check_deadline(deadline: Instant) -> Result<(), BrowserRefusal> {
    if Instant::now() >= deadline {
        return Err(BrowserRefusal::new(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "Chromium remote-debugging consent did not settle within its bounded attempt",
        ));
    }
    Ok(())
}

async fn consent_action<T: Send + 'static>(
    action: &BrowserConsentAction,
    deadline: Instant,
    operation: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, BrowserRefusal> {
    check_deadline(deadline)?;
    // Commit before scheduling: dropping a running blocking task cannot cancel it.
    let worker = action.perform(|| {
        cua_driver_core::blocking::spawn(move || {
            check_deadline(deadline)?;
            operation().map_err(|error| {
                BrowserRefusal::new(
                    BrowserRefusalCode::BrowserWrongTargetRefused,
                    format!("the exact browser consent action failed: {error}"),
                )
            })
        })
    });
    let result = worker.await.map_err(|error| {
        BrowserRefusal::new(
            BrowserRefusalCode::BrowserRouteUnavailable,
            format!("could not dispatch the exact browser consent action: {error}"),
        )
    })??;
    // The native helper may outlast our deadline. Join it, then refuse without fallback.
    check_deadline(deadline)?;
    Ok(result)
}

pub async fn handle(
    request: BrowserConsentRequest,
) -> Result<BrowserConsentOutcome, BrowserRefusal> {
    let pid = u32::try_from(request.pid).map_err(|_| {
        BrowserRefusal::new(
            BrowserRefusalCode::BrowserWrongTargetRefused,
            "the approved browser pid is outside the Linux process-id range",
        )
    })?;
    let target =
        crate::wayland::establish_exact_target(pid, request.window_id).map_err(|error| {
            BrowserRefusal::new(
                BrowserRefusalCode::BrowserBindingStale,
                format!("the approved browser window is not exact: {error}"),
            )
        })?;
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut saw_prompt = false;
    let mut accessibility_action_at = None;
    let mut trusted_click_attempted = false;
    loop {
        check_deadline(deadline)?;
        crate::wayland::validate_exact_target(&target).map_err(|error| {
            BrowserRefusal::new(
                BrowserRefusalCode::BrowserBindingStale,
                format!("the approved browser window changed before consent: {error}"),
            )
        })?;
        let window_id = request.window_id;
        let tree =
            cua_driver_core::blocking::spawn(move || crate::atspi::walk_tree(pid, window_id, None))
                .await
                .map_err(|error| {
                    BrowserRefusal::new(
                        BrowserRefusalCode::BrowserRouteUnavailable,
                        format!("could not inspect the browser consent UI: {error}"),
                    )
                })?;
        check_deadline(deadline)?;
        let prompt_present = remote_debugging_prompt_present(&tree.nodes);
        saw_prompt |= prompt_present;
        match exact_allow_button(&tree.nodes, &tree.bounds)? {
            Some(allow) if accessibility_action_at.is_none() => {
                let expected_action = allow.action.clone();
                let action_target = target.clone();
                let result = consent_action(&request.action, deadline, move || {
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
                .await?;
                if result.0 != expected_action || result.1 {
                    return Err(BrowserRefusal::new(
                        BrowserRefusalCode::BrowserWrongTargetRefused,
                        "the exact browser consent action was not explicitly acknowledged",
                    ));
                }
                // The verified helper re-walks at action time, matches the
                // stable D-Bus object key and complete semantic identity,
                // selects this named action, and rejects do_action(false).
                // Acceptance waits until that prompt actually leaves; a dialog
                // that stays is not a second, weaker do_action.
                accessibility_action_at = Some(Instant::now());
            }
            Some(_)
                if !trusted_click_attempted
                    && accessibility_action_at.is_some_and(|attempted| {
                        attempted.elapsed() >= Duration::from_millis(150)
                    }) =>
            {
                let action_target = target.clone();
                consent_action(&request.action, deadline, move || {
                    trusted_allow_click(&action_target)
                })
                .await?;
                trusted_click_attempted = true;
            }
            None if saw_prompt && !prompt_present => {
                if accessibility_action_at.is_some() {
                    return Ok(BrowserConsentOutcome::Accepted);
                }
                return Err(BrowserRefusal::new(
                    BrowserRefusalCode::BrowserConsentRevoked,
                    "the browser consent prompt disappeared without an explicitly acknowledged Allow action",
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

    #[tokio::test]
    async fn expired_action_never_enters_native_helper() {
        let result = consent_action(&BrowserConsentAction::default(), Instant::now(), || {
            panic!("expired action entered the native helper");
            #[allow(unreachable_code)]
            Ok(())
        })
        .await;
        assert_eq!(
            result.unwrap_err().code,
            BrowserRefusalCode::BrowserWrongTargetRefused
        );
    }

    #[test]
    fn queued_action_checks_deadline_before_native_helper() {
        use std::future::Future;
        use std::task::Poll;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        started_rx.recv().unwrap();
        runtime.block_on(async {
            let action = BrowserConsentAction::default();
            let deadline = Instant::now() + Duration::from_millis(100);
            let mut consent = Box::pin(consent_action(&action, deadline, || {
                panic!("expired queued action entered the native helper");
                #[allow(unreachable_code)]
                Ok(())
            }));
            std::future::poll_fn(|cx| {
                assert!(consent.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            tokio::time::sleep(Duration::from_millis(120)).await;
            release_tx.send(()).unwrap();
            assert_eq!(
                consent.await.unwrap_err().code,
                BrowserRefusalCode::BrowserWrongTargetRefused
            );
            blocker.await.unwrap();
        });
    }

    #[tokio::test]
    async fn running_action_is_joined_and_late_success_refused() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = completed.clone();
        let deadline = Instant::now() + Duration::from_millis(100);
        let result = consent_action(&BrowserConsentAction::default(), deadline, move || {
            std::thread::sleep(Duration::from_millis(120));
            worker_completed.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(
            completed.load(Ordering::SeqCst),
            "native worker was detached"
        );
        assert_eq!(
            result.unwrap_err().code,
            BrowserRefusalCode::BrowserWrongTargetRefused
        );
    }

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
            identity: None,
            depth: 0,
            parent_element_index: None,
            in_web_content: false,
            object_ref: None,
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
        let cancel = exact_cancel_button(&prompt(), &[]).unwrap().unwrap();
        assert_eq!(cancel.element_index, 7);
        assert_eq!(cancel.element_key, 0x77);
        assert_eq!(cancel.action, "click");
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
