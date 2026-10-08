use super::*;

fn toplevel(title: &str, app_id: &str) -> Toplevel {
    Toplevel {
        title: title.into(),
        app_id: app_id.into(),
        ..Default::default()
    }
}

fn sway(id: u64, pid: u32, title: &str, app_id: &str) -> sway_ipc::Window {
    sway_ipc::Window {
        id,
        pid,
        title: title.into(),
        app_id: app_id.into(),
        x: id as i32,
        y: pid as i32,
        width: 800,
        height: 600,
        content_x: 0,
        content_y: 0,
        focused: false,
        visible: true,
        fullscreen: false,
    }
}

fn info(xid: u64, pid: Option<u32>, title: &str) -> WindowInfo {
    WindowInfo {
        xid,
        pid,
        title: title.into(),
        app_name: String::new(),
        is_on_screen: true,
        z_index: None,
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        native_window_id: None,
        target_id: None,
        helper_epoch: None,
        transient_for_window_id: None,
        transient_for_target_id: None,
        is_attached_dialog: None,
        is_modal: None,
        window_type: None,
        workspace_index: None,
        workspace_active: None,
        sticky: None,
        monitor: None,
        capture_current: None,
        identity_capabilities: None,
    }
}

fn assert_sway_only_listing(pids: [u32; 2]) {
    let tls = HashMap::from([
        (10, toplevel("Shared", "editor")),
        (20, toplevel("Shared", "editor")),
    ]);
    let windows = [
        sway(100, pids[0], "Shared", "editor"),
        sway(200, pids[1], "Shared", "editor"),
    ];
    assert!(pair_sway_toplevels(&tls, &windows).is_empty());
    let listed = correlated_toplevel_windows(&tls, &windows);
    assert_eq!(listed.len(), 2);
    for expected in &windows {
        let (actual, identity) = listed.iter().find(|(w, _)| w.xid == expected.id).unwrap();
        assert_eq!(actual.pid, Some(expected.pid));
        assert_eq!(
            actual.native_window_id, None,
            "ambiguous handles must not be attached"
        );
        assert_eq!(
            (actual.x, actual.y, actual.width, actual.height),
            (expected.x, expected.y, 800, 600)
        );
        assert_eq!(actual.title, "Shared [editor]");
        assert_eq!(actual.app_name, "editor");
        assert!(actual.is_on_screen);
        assert_eq!(actual.target_id, None);
        assert_eq!(actual.helper_epoch, None);
        assert_eq!(actual.transient_for_window_id, None);
        assert_eq!(actual.transient_for_target_id, None);
        assert_eq!(identity.title, expected.title);
        assert_eq!(identity.app_id, expected.app_id);
        assert!(!identity.closed && !identity.activated);
    }
}

#[test]
fn sway_pairing_same_title_two_processes_lists_own_ids_without_handles() {
    assert_sway_only_listing([101, 202]);
}

#[test]
fn sway_pairing_same_title_same_process_lists_both_containers() {
    assert_sway_only_listing([101, 101]);
}

#[test]
fn sway_pairing_unique_titles_preserve_protocol_handle_and_metadata() {
    let tls = HashMap::from([
        (10, toplevel("First", "editor")),
        (20, toplevel("Second", "editor")),
    ]);
    let mut windows = [
        sway(200, 202, "Second", "editor"),
        sway(100, 101, "First", "editor"),
    ];
    windows[0].visible = false;
    let listed = correlated_toplevel_windows(&tls, &windows);
    assert_eq!(listed.len(), 2);
    for (protocol, id, pid, title, visible) in [
        (10, 100, 101, "First [editor]", true),
        (20, 200, 202, "Second [editor]", false),
    ] {
        let (actual, _) = listed.iter().find(|(w, _)| w.xid == id).unwrap();
        assert_eq!(actual.native_window_id, Some(protocol));
        assert_eq!(actual.pid, Some(pid));
        assert_eq!(actual.title, title);
        assert_eq!(actual.is_on_screen, visible);
        assert_eq!(
            (actual.x, actual.y, actual.width, actual.height),
            (id as i32, pid as i32, 800, 600)
        );
    }
}

#[test]
fn sway_pairing_title_and_app_id_disambiguate_in_either_order() {
    for reverse in [false, true] {
        let mut tls = vec![
            (10, toplevel("Shared", "one")),
            (20, toplevel("Shared", "two")),
        ];
        let mut windows = vec![
            sway(200, 202, "Shared", "two"),
            sway(100, 101, "Shared", "one"),
        ];
        if reverse {
            tls.reverse();
            windows.reverse();
        }
        let pairs = pair_sway_toplevels(&tls.into_iter().collect(), &windows);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[&10].id, 100);
        assert_eq!(pairs[&20].id, 200);
    }
}

#[test]
fn sway_pairing_empty_title_uses_unique_app_id_without_exact_title() {
    let tls = HashMap::from([(10, toplevel("", "editor"))]);
    let windows = [sway(100, 101, "Document", "editor")];
    assert_eq!(pair_sway_toplevels(&tls, &windows)[&10].id, 100);
}

#[test]
fn sway_pairing_each_key_requires_uniqueness_on_both_sides() {
    let one = HashMap::from([(10, toplevel("Shared", "editor"))]);
    let two = HashMap::from([
        (10, toplevel("Shared", "editor")),
        (20, toplevel("Shared", "editor")),
    ]);
    let windows = [
        sway(100, 101, "Shared", "editor"),
        sway(200, 202, "Shared", "editor"),
    ];
    assert!(pair_sway_toplevels(&one, &windows).is_empty());
    assert!(pair_sway_toplevels(&two, &windows[..1]).is_empty());
    let empty = HashMap::from([(10, toplevel("", "editor")), (20, toplevel("", "editor"))]);
    assert!(pair_sway_toplevels(&empty, &windows[..1]).is_empty());
    let empty_one = HashMap::from([(10, toplevel("", "editor"))]);
    assert!(pair_sway_toplevels(&empty_one, &windows).is_empty());
}

#[test]
fn sway_pairing_exact_title_presence_blocks_app_only_fallback() {
    let tls = HashMap::from([(10, toplevel("Shared", "editor"))]);
    let windows = [
        sway(100, 101, "Shared", "other"),
        sway(200, 202, "Shared", "other"),
        sway(300, 303, "Different", "editor"),
    ];
    assert!(pair_sway_toplevels(&tls, &windows).is_empty());
}

#[test]
fn sway_pairing_closed_and_private_toplevels_do_not_claim_sway() {
    let private = "surface:1111111111111111:0000000000000001:303";
    let mut closed = toplevel("Title", "editor");
    closed.closed = true;
    let tls = HashMap::from([
        (10, toplevel("Title", "editor")),
        (20, closed),
        (30, toplevel("Title", private)),
    ]);
    let windows = [sway(100, 101, "Title", "editor")];
    let pairs = pair_sway_toplevels(&tls, &windows);
    assert_eq!(pairs.len(), 1);
    assert_eq!(pairs[&10].id, 100);
    let listed = correlated_toplevel_windows(&tls, &windows);
    assert_eq!(listed.len(), 2);
    let (surface, _) = listed.iter().find(|(w, _)| w.target_id.is_some()).unwrap();
    assert_eq!(surface.pid, Some(303));
    assert_eq!(surface.xid, private_target_window_id(private));
    assert_eq!(surface.title, "Title");
    assert_eq!(surface.native_window_id, Some(30));
}

#[test]
fn sway_listing_unclaimed_containers_are_complete_even_without_toplevels() {
    let mut windows = [
        sway(100, 101, "Only Sway", "editor"),
        sway(200, 202, "XWayland", ""),
    ];
    windows[1].visible = false;
    let listed = correlated_toplevel_windows(&HashMap::new(), &windows);
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].0.title, "Only Sway [editor]");
    assert_eq!(listed[1].0.title, "XWayland");
    assert!(!listed[1].0.is_on_screen);
    assert!(listed.iter().all(|(w, _)| w.native_window_id.is_none()));
}

#[test]
fn sway_listing_omits_unpaired_only_for_exact_title_or_empty_title_app() {
    let tls = HashMap::from([
        (10, toplevel("Shared", "")),
        (20, toplevel("Shared", "")),
        (30, toplevel("Other", "editor")),
        (40, toplevel("Other", "editor")),
        (50, toplevel("", "editor")),
        (60, toplevel("", "editor")),
        (70, toplevel("Sha", "")),
        (80, toplevel("", "")),
    ]);
    let windows = [sway(100, 101, "Shared", "editor")];
    let listed = correlated_toplevel_windows(&tls, &windows);
    let mut ids: Vec<_> = listed.iter().map(|(w, _)| w.xid).collect();
    ids.sort_unstable();
    assert_eq!(ids, [30, 40, 70, 80, 100]);
    for (window, _) in listed.iter().filter(|(w, _)| w.xid != 100) {
        assert_eq!(window.pid, None);
        assert_eq!(window.native_window_id, Some(window.xid));
    }
}

#[test]
fn native_identity_duplicate_atspi_titles_do_not_fill_pid() {
    let result = enrich_native_windows(
        vec![info(10, None, "Shared")],
        vec![
            info(100, Some(101), "Shared"),
            info(200, Some(202), "Shared"),
        ],
        false,
    );
    assert_eq!(result[0].pid, None);
}

#[test]
fn native_identity_duplicate_pidless_titles_do_not_fill_pid() {
    let mut first = info(10, None, "Shared [editor]");
    first.app_name = "editor".into();
    let result = enrich_native_windows(
        vec![first, info(20, None, "Shared")],
        vec![info(100, Some(101), "Shared")],
        false,
    );
    assert!(result.iter().all(|w| w.pid.is_none()));
}

#[test]
fn native_identity_unique_title_still_fills_pid() {
    let result = enrich_native_windows(
        vec![info(10, None, "Unique")],
        vec![info(100, Some(101), "Unique")],
        false,
    );
    assert_eq!(result[0].pid, Some(101));
    assert_eq!(result[0].xid, 10);
}

#[test]
fn native_identity_app_id_fallback_is_unchanged() {
    let mut native = info(10, None, "Different [editor]");
    native.app_name = "editor".into();
    let mut accessible = info(100, Some(101), "Title");
    accessible.app_name = "editor".into();
    assert_eq!(
        enrich_native_windows(vec![native.clone()], vec![accessible.clone()], false)[0].pid,
        Some(101)
    );
    assert_eq!(
        enrich_native_windows(vec![native], vec![accessible.clone(), accessible], false)[0].pid,
        None
    );
}

fn identity(title: &str, app_id: &str) -> ToplevelIdentity {
    ToplevelIdentity {
        title: title.into(),
        app_id: app_id.into(),
    }
}

#[test]
fn handle_identity_duplicate_title_refuses_even_with_unique_app_id() {
    assert_eq!(
        matching_protocol_id(
            &identity("Shared", "one"),
            [(10, "Shared", "one", false), (20, "Shared", "two", false)].into_iter()
        ),
        None
    );
}

#[test]
fn handle_identity_unique_title_returns_protocol_id() {
    assert_eq!(
        matching_protocol_id(
            &identity("One", "editor"),
            [(10, "One", "editor", false), (20, "Two", "editor", false)].into_iter()
        ),
        Some(10)
    );
}

#[test]
fn handle_identity_closed_toplevels_are_ignored() {
    assert_eq!(
        matching_protocol_id(
            &identity("Shared", "editor"),
            [
                (10, "Shared", "editor", true),
                (20, "Shared", "editor", false)
            ]
            .into_iter()
        ),
        Some(20)
    );
    assert_eq!(
        matching_protocol_id(
            &identity("Shared", "editor"),
            [(10, "Shared", "editor", true)].into_iter()
        ),
        None
    );
}

#[test]
fn handle_identity_app_fallback_requires_nonempty_unique_match() {
    let candidates = [(10, "One", "editor", false), (20, "Two", "editor", false)];
    assert_eq!(
        matching_protocol_id(&identity("Missing", "editor"), candidates.into_iter()),
        None
    );
    assert_eq!(
        matching_protocol_id(
            &identity("Missing", "editor"),
            candidates[..1].iter().copied()
        ),
        Some(10)
    );
    assert_eq!(
        matching_protocol_id(&identity("", ""), [(10, "", "", false)].into_iter()),
        None
    );
}

#[test]
fn atspi_sway_identity_known_pid_never_adopts_another_process_title() {
    let mut windows = [info(10, Some(101), "Shared")];
    let others = [sway(200, 202, "Shared", "editor")];
    reconcile_atspi_sway_windows(&mut windows, &others, |pid| {
        assert_eq!(pid, 101);
        None
    });
    assert_eq!(windows[0].xid, 10);
    assert_eq!(windows[0].pid, Some(101));
    assert_eq!(
        (
            windows[0].x,
            windows[0].y,
            windows[0].width,
            windows[0].height
        ),
        (0, 0, 0, 0)
    );
}

#[test]
fn atspi_sway_identity_known_pid_uses_only_pid_provider() {
    let mut windows = [info(10, Some(101), "Shared")];
    let others = [sway(200, 202, "Shared", "editor")];
    reconcile_atspi_sway_windows(&mut windows, &others, |_| {
        Some(sway(100, 101, "Renamed", "editor"))
    });
    assert_eq!(windows[0].xid, 100);
    assert_eq!(windows[0].pid, Some(101));
    assert_eq!((windows[0].x, windows[0].y), (100, 101));
}

#[test]
fn atspi_sway_identity_pidless_duplicate_titles_remain_unchanged() {
    let mut windows = [info(10, None, "Shared")];
    let others = [
        sway(100, 101, "Shared", "editor"),
        sway(200, 202, "Shared", "editor"),
    ];
    reconcile_atspi_sway_windows(&mut windows, &others, |_| panic!("pid-less window"));
    assert_eq!(windows[0].xid, 10);
    assert_eq!((windows[0].x, windows[0].y), (0, 0));
}

#[test]
fn atspi_sway_identity_pidless_requires_exact_title_or_unique_app() {
    let others = [sway(100, 101, "Document", "editor")];
    let mut windows = [
        info(10, None, "Doc"),
        info(20, None, "Document"),
        info(30, None, "Other"),
    ];
    windows[2].app_name = "editor".into();
    reconcile_atspi_sway_windows(&mut windows, &others, |_| panic!("pid-less window"));
    assert_eq!(windows[0].xid, 10, "prefix is not identity");
    assert_eq!(windows[1].xid, 100);
    assert_eq!(windows[2].xid, 100);
    assert!(windows.iter().all(|w| w.pid.is_none()));
    let mut ambiguous = [info(40, None, "Missing")];
    ambiguous[0].app_name = "editor".into();
    reconcile_atspi_sway_windows(
        &mut ambiguous,
        &[others[0].clone(), sway(200, 202, "Other", "editor")],
        |_| panic!("pid-less window"),
    );
    assert_eq!(ambiguous[0].xid, 40);
}
