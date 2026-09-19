//! Pure identity matching shared by native actuation and headless tests.
use super::AtspiIdentity;
use anyhow::{anyhow, Result};

pub(super) fn exact_window_element_index(
    indexed_elements: &[(usize, u64)],
    scoped_frame: usize,
    element_key: u64,
    window_id: u64,
) -> Result<usize> {
    let matches = indexed_elements
        .iter()
        .enumerate()
        .filter(|(_, (_, key))| *key == element_key)
        .map(|(index, (frame, _))| (index, *frame))
        .collect::<Vec<_>>();
    let (index, frame) = match matches.as_slice() {
        [(index, frame)] => (*index, *frame),
        [] => {
            return Err(anyhow!(
                "AT-SPI element key {element_key:#x} is stale for exact window {window_id}"
            ))
        }
        _ => {
            anyhow::bail!("AT-SPI element key {element_key:#x} is ambiguous across pid toplevels")
        }
    };
    if frame != scoped_frame {
        anyhow::bail!(
            "AT-SPI element key {element_key:#x} belongs to a same-process sibling, not exact window {window_id}"
        );
    }
    Ok(index)
}

#[cfg(test)]
mod exact_window_element_tests {
    use super::exact_window_element_index;

    #[test]
    fn stable_key_survives_process_wide_ordinal_reordering() {
        let reordered = [(0, 0x99), (1, 0x41), (1, 0x42)];
        assert_eq!(
            exact_window_element_index(&reordered, 1, 0x41, 7).unwrap(),
            1
        );
    }

    #[test]
    fn same_key_in_another_window_cannot_retarget() {
        let records = [(0, 0x41), (1, 0x42)];
        assert!(exact_window_element_index(&records, 1, 0x41, 7).is_err());
    }

    #[test]
    fn duplicate_key_in_exact_window_fails_closed() {
        let records = [(1, 0x41), (1, 0x41)];
        assert!(exact_window_element_index(&records, 1, 0x41, 7).is_err());
    }

    #[test]
    fn duplicate_key_in_same_pid_sibling_fails_before_frame_filter() {
        let records = [(0, 0x41), (1, 0x41), (1, 0x42)];
        assert!(exact_window_element_index(&records, 1, 0x41, 7).is_err());
    }
}

/// One live-walk occurrence, with native address independent of frame proof.
pub(super) struct ObservedIdentityCandidate<'a> {
    pub position: usize,
    // The live proxy address is available even when frame identity is unproven.
    pub bus_name: &'a str,
    pub path: &'a str,
    pub identity: Option<&'a AtspiIdentity>,
    pub frame_ordinal: usize,
    pub indexable: bool,
}

/// Select a retained identity from a live walk. The returned position is only
/// the position in that live walk, never the snapshot's public index.
pub(super) fn unique_observed_identity_position<'a>(
    nodes: impl Iterator<Item = ObservedIdentityCandidate<'a>>,
    identity: &AtspiIdentity,
    expected_frame: usize,
) -> Result<usize> {
    // Count the native address across the entire walk BEFORE filtering by
    // ancestry or indexability. One object reported below two frames is
    // ambiguous even when only one occurrence has the retained frame identity.
    let mut matches = nodes.filter(|candidate| {
        candidate.bus_name == identity.bus_name && candidate.path == identity.path
    });
    let Some(candidate) = matches.next() else {
        anyhow::bail!("stale_element_token: observed AT-SPI object is no longer present");
    };
    if matches.next().is_some()
        || candidate.identity != Some(identity)
        || candidate.frame_ordinal != expected_frame
        || !candidate.indexable
    {
        anyhow::bail!(
            "stale_element_token: observed object is ambiguous, disabled or outside the target window"
        );
    }
    Ok(candidate.position)
}

/// Exact native address, not a hash or an application-wide ordinal.
pub(super) type ObjectAddress = (String, String);

/// Bound both memory and traversal work even for a broken/cyclic Parent graph.
/// Native callers additionally bound each D-Bus read and the entire operation.
const MAX_ANCESTRY_NODES: usize = 128;

/// Read live Parent edges from the retained object to its retained frame. The
/// lookup must return unique-owner addresses (never an unresolved well-known
/// name), and must report lookup failures as errors rather than missing edges.
/// This deliberately does not re-resolve an element index or choose a new frame.
pub(super) async fn verify_observed_frame_ancestry<F, Fut>(
    identity: &AtspiIdentity,
    mut parent: F,
) -> Result<()>
where
    F: FnMut(ObjectAddress) -> Fut,
    Fut: std::future::Future<Output = Result<Option<ObjectAddress>>>,
{
    let expected = (identity.frame_bus_name.clone(), identity.frame_path.clone());
    let mut current = (identity.bus_name.clone(), identity.path.clone());
    let mut seen = std::collections::HashSet::new();
    for _ in 0..MAX_ANCESTRY_NODES {
        if !current.0.starts_with(':')
            || !current.1.starts_with('/')
            || current.1 == "/org/a11y/atspi/null"
        {
            anyhow::bail!("stale_element_token: unproven native ancestry address");
        }
        if !seen.insert(current.clone()) {
            anyhow::bail!("stale_element_token: cycle in observed object ancestry");
        }
        if current == expected {
            return Ok(());
        }
        current = parent(current).await?.ok_or_else(|| {
            anyhow!("stale_element_token: observed object left the retained window frame")
        })?;
    }
    anyhow::bail!("stale_element_token: observed object ancestry exceeds the traversal bound")
}

#[cfg(test)]
mod observed_identity_tests {
    use super::*;

    fn match_nodes<'a>(
        nodes: impl Iterator<Item = (usize, Option<&'a AtspiIdentity>, usize, bool)>,
        identity: &AtspiIdentity,
        expected_frame: usize,
    ) -> Result<usize> {
        unique_observed_identity_position(
            nodes.map(
                |(position, identity, frame_ordinal, indexable)| ObservedIdentityCandidate {
                    position,
                    bus_name: identity.map_or("", |identity| identity.bus_name.as_str()),
                    path: identity.map_or("", |identity| identity.path.as_str()),
                    identity,
                    frame_ordinal,
                    indexable,
                },
            ),
            identity,
            expected_frame,
        )
    }

    fn identity(path: &str) -> AtspiIdentity {
        AtspiIdentity {
            bus_name: ":1.1".into(),
            path: path.into(),
            frame_bus_name: ":1.1".into(),
            frame_path: "/frame".into(),
        }
    }

    #[test]
    fn live_reorder_matches_retained_identity_not_the_old_index() {
        let observed = identity("/ok");
        let replacement = identity("/cancel");
        let live = vec![replacement, observed.clone()];
        let target = match_nodes(
            live.iter()
                .enumerate()
                .map(|(position, item)| (position, Some(item), 0, true)),
            &observed,
            0,
        )
        .unwrap();
        assert_eq!(target, 1);
    }

    #[test]
    fn changed_bus_or_frame_identity_cannot_match_a_reused_path() {
        let observed = identity("/ok");
        for field in 0..3 {
            let mut replacement = observed.clone();
            match field {
                0 => replacement.bus_name = ":1.2".into(),
                1 => replacement.frame_bus_name = ":1.2".into(),
                _ => replacement.frame_path = "/other_frame".into(),
            }
            assert!(match_nodes(
                std::iter::once((0, Some(&replacement), 0, true)),
                &observed,
                0,
            )
            .is_err());
        }
    }

    #[test]
    fn duplicate_disabled_missing_or_sibling_objects_refuse() {
        let observed = identity("/ok");
        for nodes in [
            vec![(0, Some(&observed), 0, true), (1, Some(&observed), 0, true)],
            vec![(0, Some(&observed), 0, false)],
            vec![(0, Some(&observed), 1, true)],
            vec![(0, None, 0, true)],
        ] {
            assert!(match_nodes(nodes.into_iter(), &observed, 0).is_err());
        }
    }

    #[test]
    fn duplicate_native_address_in_other_frame_refuses_before_ancestry_filter() {
        let observed = identity("/ok");
        for changed_owner in [false, true] {
            let mut sibling = observed.clone();
            sibling.frame_path = "/sibling".into();
            if changed_owner {
                sibling.frame_bus_name = ":1.9".into();
            }
            // Neither iteration order nor a disabled sibling can hide a duplicate.
            for sibling_enabled in [false, true] {
                let mut nodes = vec![
                    (0, Some(&observed), 0, true),
                    (1, Some(&sibling), 1, sibling_enabled),
                ];
                for _ in 0..2 {
                    assert!(match_nodes(nodes.iter().copied(), &observed, 0).is_err());
                    nodes.reverse();
                }
            }
        }
    }

    #[test]
    fn duplicate_native_address_with_unproven_frame_still_refuses() {
        let observed = identity("/ok");
        let candidates = [
            ObservedIdentityCandidate {
                position: 0,
                bus_name: &observed.bus_name,
                path: &observed.path,
                identity: Some(&observed),
                frame_ordinal: 0,
                indexable: true,
            },
            ObservedIdentityCandidate {
                position: 1,
                bus_name: &observed.bus_name,
                path: &observed.path,
                identity: None,
                frame_ordinal: 1,
                indexable: false,
            },
        ];
        assert!(unique_observed_identity_position(candidates.into_iter(), &observed, 0).is_err());
    }

    #[test]
    fn same_path_on_distinct_native_owner_is_not_a_duplicate() {
        let observed = identity("/ok");
        let mut other = observed.clone();
        other.bus_name = ":1.2".into();
        let nodes = [(4, Some(&other), 1, true), (9, Some(&observed), 0, true)];
        assert_eq!(match_nodes(nodes.into_iter(), &observed, 0).unwrap(), 9);
    }

    fn address(path: &str) -> ObjectAddress {
        (":1.1".into(), path.into())
    }

    async fn check_parents(
        observed: &AtspiIdentity,
        parents: &std::collections::HashMap<ObjectAddress, ObjectAddress>,
    ) -> Result<()> {
        verify_observed_frame_ancestry(observed, |current| {
            std::future::ready(Ok(parents.get(&current).cloned()))
        })
        .await
    }

    #[tokio::test]
    async fn live_ancestry_accepts_only_retained_frame_owner_and_path() {
        let observed = identity("/ok");
        let mut parents = std::collections::HashMap::from([
            (address("/ok"), address("/panel")),
            (address("/panel"), address("/frame")),
        ]);
        check_parents(&observed, &parents).await.unwrap();
        // Reparenting inside the same retained frame remains safe.
        parents.insert(address("/ok"), address("/frame"));
        check_parents(&observed, &parents).await.unwrap();
        for wrong_frame in [address("/sibling"), (":1.2".into(), "/frame".into())] {
            parents.insert(address("/ok"), wrong_frame);
            assert!(check_parents(&observed, &parents).await.is_err());
        }
    }

    #[tokio::test]
    async fn retained_frame_itself_needs_no_parent_lookup() {
        let observed = identity("/frame");
        verify_observed_frame_ancestry(&observed, |_| {
            panic!("do not walk beyond the retained frame");
            #[allow(unreachable_code)]
            std::future::ready(Ok(None))
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn live_ancestry_errors_and_missing_edges_never_allow_pointer_fallback() {
        let observed = identity("/ok");
        for message in [
            "Parent timed out",
            "object vanished",
            "malformed Parent",
            "no unique owner",
        ] {
            let error = verify_observed_frame_ancestry(&observed, |_| {
                std::future::ready(Err(anyhow!(message)))
            })
            .await
            .unwrap_err();
            assert!(!super::super::types::click_error_allows_pointer_fallback(
                &error
            ));
        }
        let error = check_parents(&observed, &std::collections::HashMap::new())
            .await
            .unwrap_err();
        assert!(!super::super::types::click_error_allows_pointer_fallback(
            &error
        ));
    }

    #[tokio::test]
    async fn live_ancestry_refuses_cycles_unresolved_names_and_null_objects() {
        let observed = identity("/ok");
        for parents in [
            std::collections::HashMap::from([(address("/ok"), address("/ok"))]),
            std::collections::HashMap::from([
                (address("/ok"), address("/panel")),
                (address("/panel"), address("/ok")),
            ]),
            std::collections::HashMap::from([(
                address("/ok"),
                ("org.webkit.Renderer".into(), "/frame".into()),
            )]),
            std::collections::HashMap::from([(address("/ok"), address("/org/a11y/atspi/null"))]),
        ] {
            assert!(check_parents(&observed, &parents).await.is_err());
        }
    }

    #[tokio::test]
    async fn live_ancestry_has_a_hard_traversal_bound() {
        let observed = identity("/ok");
        let mut reads = 0;
        let error = verify_observed_frame_ancestry(&observed, |_| {
            reads += 1;
            std::future::ready(Ok(Some(address(&format!("/ancestor{reads}")))))
        })
        .await
        .unwrap_err();
        assert_eq!(reads, MAX_ANCESTRY_NODES);
        assert!(error.to_string().contains("traversal bound"));
    }

    // Fault injection at each asynchronous gap. The production caller ordering
    // is guarded separately by the source-wiring tests; these exercise the real
    // verifier with a live, mutable Parent graph, never a copied implementation.
    async fn assert_reparenting_refuses_dispatch(gap: &str) {
        let observed = identity("/ok");
        let mut parents = std::collections::HashMap::from([(address("/ok"), address("/frame"))]);
        check_parents(&observed, &parents).await.unwrap();
        // XID/PID, enabled state and native object address remain unchanged.
        parents.insert(address("/ok"), address("/sibling"));
        let mut dispatches = 0;
        let result = async {
            check_parents(&observed, &parents).await?;
            dispatches += 1;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        assert!(result.is_err(), "reparenting during {gap} must refuse");
        assert_eq!(dispatches, 0, "no semantic or pointer replay after {gap}");
    }

    #[tokio::test]
    async fn reparenting_during_overlay_wait_refuses_mutation() {
        assert_reparenting_refuses_dispatch("overlay wait").await;
    }

    #[tokio::test]
    async fn reparenting_during_action_name_lookup_refuses_mutation() {
        assert_reparenting_refuses_dispatch("action-name lookup").await;
    }

    #[tokio::test]
    async fn reparenting_during_foreground_activation_refuses_pointer_delivery() {
        assert_reparenting_refuses_dispatch("foreground activation").await;
    }

    #[test]
    fn removed_observed_identity_refuses_before_click() {
        let observed = identity("/ok");
        let replacement = identity("/cancel");
        assert!(match_nodes(
            std::iter::once((0, Some(&replacement), 0, true)),
            &observed,
            0,
        )
        .is_err());
    }
}
