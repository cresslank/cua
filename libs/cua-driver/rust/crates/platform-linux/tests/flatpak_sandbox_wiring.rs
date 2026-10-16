//! Flatpak accessibility wiring guards. The sandbox proof itself is tested in
//! `atspi/native/sandbox_peer.rs`; these pin where the snapshot walk applies
//! it. They do not claim D-Bus or desktop delivery.
const NATIVE: &str = include_str!("../src/atspi/native.rs");

fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    source
        .rsplit_once(start)
        .unwrap()
        .1
        .split_once(end)
        .unwrap()
        .0
}

fn ordered(source: &str, needles: &[&str]) {
    let compact = source.split_whitespace().collect::<String>();
    let mut rest = compact.as_str();
    for needle in needles {
        let needle = needle.split_whitespace().collect::<String>();
        rest = rest
            .split_once(&needle)
            .unwrap_or_else(|| panic!("missing/out of order: {needle}"))
            .1;
    }
}

fn absent(source: &str, needle: &str) {
    let compact = source.split_whitespace().collect::<String>();
    let needle = needle.split_whitespace().collect::<String>();
    assert!(!compact.contains(&needle), "unexpected: {needle}");
}

fn walk() -> &'static str {
    section(
        NATIVE,
        concat!("async fn collect_visited_", "in_gate<'a>("),
        concat!(
            "/// Roles whose Description / LABELLED_BY ",
            "are worth a round-trip"
        ),
    )
}

#[test]
fn only_the_snapshot_walk_admits_a_sandbox_proxy() {
    let focus_and_hits = section(
        NATIVE,
        concat!("async fn app_for_", "pid<'a>("),
        concat!("fn process_", "name(pid: u32)"),
    );
    ordered(
        focus_and_hits,
        &[concat!("resolve_app_for_pid(conn, pid, ", "false)")],
    );
    ordered(
        walk(),
        &[concat!("resolve_app_for_pid(conn, pid, ", "true)")],
    );
}

#[test]
fn every_object_address_passes_the_sandbox_gate_first() {
    // The three places an object reference becomes a D-Bus destination.
    for (start, end) in [
        (
            concat!("async fn accessible_via_", "bus<'a>("),
            concat!("async fn accessible_", "for<'a>("),
        ),
        (
            concat!("async fn raw_child_", "count(conn"),
            concat!(
                "/// Pin well-known names to their current ",
                "unique owner."
            ),
        ),
        (
            concat!("async fn raw_", "children(\n"),
            concat!(
                "/// Keep the first matching application as a ",
                "compatibility fallback"
            ),
        ),
    ] {
        let body = section(NATIVE, start, end);
        ordered(
            body,
            &[
                "if !sandbox_gate_allows(",
                "return Err(sandbox_refusal(&oref.name));",
                "Proxy",
            ],
        );
    }
}

#[test]
fn each_walk_has_its_own_gate_opened_only_by_a_sandbox_admission() {
    let opts = section(
        NATIVE,
        concat!("async fn collect_visited_", "bounded_opts<'a>("),
        concat!("async fn collect_visited_", "in_gate<'a>("),
    );
    ordered(
        opts,
        &[
            "SANDBOX_GATE .scope( std::cell::RefCell::new(None),",
            "collect_visited_in_gate(",
        ],
    );
    ordered(
        walk(),
        &[
            "resolve_app_for_pid(conn, pid, true)",
            "if let Some(proxy) = sandbox_proxy {",
            "*gate.borrow_mut() = Some(SandboxGate {",
            "if opened.is_err() {",
            "return Err(",
            "if sandbox_gate_allows(zconn, &seed.name).await { kept.push(seed); }",
            "seeds = kept;",
            "resolve_window_frame(conn, pid, xid, &seeds)",
            "while let Some((oref, depth, inherited_web_doc, top_level_key, frame_ordinal)) = stack.pop()",
            "if !sandbox_gate_allows(zconn, &oref.name).await {",
            "continue;",
            "identity_ref(conn, &oref, &mut identity_owners)",
        ],
    );
}

#[test]
fn a_proxy_selection_needs_a_complete_scan_and_the_shell_window_list() {
    let resolve = section(
        NATIVE,
        concat!("async fn resolve_app_for_", "pid<'a>("),
        concat!(
            "/// The pid behind a unique bus name ",
            "on the accessibility bus."
        ),
    );
    ordered(
        resolve,
        &[
            "Some(None) | None => {",
            "incomplete = Some(\"peer_pid_unknown\");",
            "continue;",
        ],
    );
    let proof = section(
        resolve,
        "if selected.sandbox_proxy.is_some()",
        "Ok(AppLookup::Found(selected))",
    );
    ordered(
        proof,
        &[
            "if let Some(reason) = incomplete",
            "return Ok(AppLookup::Incomplete(reason));",
            "bounded_blocking(|| crate::wayland::shell_helper::list_windows(None))",
            "} else { None };",
            "(Some(proof), Some(windows)) => {",
            "!proof.owns_its_sandbox_windows(windows.iter().map(|window| window.pid))",
            "_ => true,",
            "if shared {",
            "return Err(anyhow!(",
            "flatpak_sandbox_shared",
        ],
    );
    // Presentation lists drop untitled windows or synthesize ids.
    absent(proof, "x11::list_windows");
    absent(proof, "list_windows_dispatch");
}

#[test]
fn an_unproven_absence_is_retried_by_the_snapshot_walk() {
    ordered(
        walk(),
        &[
            "if let AppLookup::Incomplete(reason) = lookup",
            "REGISTRY_INCOMPLETE.try_with(|cell| cell.set(Some(reason)))",
        ],
    );
    let snapshot = section(
        NATIVE,
        concat!("pub(super) fn walk_tree_bounded_", "with_timeout("),
        concat!(
            "let walked = match before_snapshot_",
            "deadline(backstop, walk)"
        ),
    );
    ordered(
        snapshot,
        &[
            "REGISTRY_INCOMPLETE.scope(",
            "Some(reason) if walked.is_none() && !deadline_passed()",
            "atspi_registry_incomplete",
        ],
    );
}
