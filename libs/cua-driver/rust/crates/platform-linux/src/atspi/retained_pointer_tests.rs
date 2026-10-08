use super::*;
use std::cell::Cell;

#[derive(Clone, Copy, Debug)]
pub(crate) enum MenuFault {
    Absent,
    Present,
    Proxy,
    ProxyTimeout,
    Count,
    CountTimeout,
    Name,
    NameTimeout,
    Reject,
    Dispatch,
    DispatchTimeout,
}
pub(crate) const MENU_FAILURES: [MenuFault; 9] = [
    MenuFault::Proxy,
    MenuFault::ProxyTimeout,
    MenuFault::Count,
    MenuFault::CountTimeout,
    MenuFault::Name,
    MenuFault::NameTimeout,
    MenuFault::Reject,
    MenuFault::Dispatch,
    MenuFault::DispatchTimeout,
];
struct MenuProbe<'a> {
    fault: MenuFault,
    verified: &'a Cell<bool>,
}
impl MenuAction for MenuProbe<'_> {
    async fn count(&self) -> Result<i32> {
        if matches!(self.fault, MenuFault::CountTimeout) {
            return std::future::pending().await;
        }
        anyhow::ensure!(!matches!(self.fault, MenuFault::Count), "NActions error");
        Ok(2)
    }
    async fn name(&self, index: i32) -> Result<String> {
        // The second name fails even when the first was a recognized menu.
        if index == 1 {
            if matches!(self.fault, MenuFault::NameTimeout) {
                return std::future::pending().await;
            }
            anyhow::ensure!(!matches!(self.fault, MenuFault::Name), "GetName error");
        }
        Ok(if matches!(self.fault, MenuFault::Absent) || index == 1 {
            "click"
        } else {
            "context-menu"
        }
        .into())
    }
    async fn dispatch(&self, index: i32) -> Result<bool> {
        assert_eq!(index, 0);
        assert!(self.verified.get(), "must revalidate after ALL live names");
        if matches!(self.fault, MenuFault::DispatchTimeout) {
            return std::future::pending().await;
        }
        anyhow::ensure!(!matches!(self.fault, MenuFault::Dispatch), "DoAction error");
        Ok(!matches!(self.fault, MenuFault::Reject))
    }
}
pub(crate) fn menu_probe(fault: MenuFault) -> Result<bool> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let verified = Cell::new(false);
            OP_DEADLINE
                .scope(
                    tokio::time::Instant::now()
                        + if matches!(
                            fault,
                            MenuFault::ProxyTimeout
                                | MenuFault::CountTimeout
                                | MenuFault::NameTimeout
                                | MenuFault::DispatchTimeout
                        ) {
                            Duration::from_millis(25)
                        } else {
                            OP_TIMEOUT
                        },
                    strict_context_menu(
                        async {
                            if matches!(fault, MenuFault::ProxyTimeout) {
                                return std::future::pending().await;
                            }
                            anyhow::ensure!(
                                !matches!(fault, MenuFault::Proxy),
                                "Action proxy error"
                            );
                            Ok(MenuProbe {
                                fault,
                                verified: &verified,
                            })
                        },
                        || async {
                            verified.set(true);
                            Ok(())
                        },
                    ),
                )
                .await
        })
}
#[test]
fn secondary_strict_native_menu_discovery_and_dispatch_fail_closed() {
    for fault in MENU_FAILURES {
        assert!(menu_probe(fault).is_err(), "{fault:?}");
    }
    assert!(!menu_probe(MenuFault::Absent).unwrap());
    assert!(menu_probe(MenuFault::Present).unwrap());
}
#[test]
fn secondary_pointer_owner_rejects_unique_cross_process_with_embedded_offset() {
    let mut id = AtspiIdentity {
        bus_name: ":1.77".into(),
        path: "/embedded/button".into(),
        frame_bus_name: ":1.41".into(),
        frame_path: "/frame".into(),
    };
    // These otherwise usable bounds omit a nonzero embedding offset. They must
    // not be projected through the outer frame when the bus owner differs.
    let project = |id: &AtspiIdentity| -> Result<_> {
        require_pointer_frame_owner(Some(id))?;
        project_retained_pointer_bounds(
            (30, 40, 50, 50),
            (0, 0, 600, 400),
            (10, 20),
            (80, 90),
            true,
        )
    };
    assert!(project(&id)
        .unwrap_err()
        .to_string()
        .contains("separate accessibility process"));
    id.bus_name = id.frame_bus_name.clone();
    assert_eq!(project(&id).unwrap(), (120, 150, 50, 50));
    id.bus_name = "org.example.Accessibility".into();
    id.frame_bus_name = id.bus_name.clone();
    assert!(
        project(&id).is_err(),
        "unresolved aliases are not unique identities"
    );
    assert!(require_pointer_frame_owner(None).is_err());
}
#[test]
fn secondary_owner_identity_provenance_pins_object_and_frame_before_proxy() {
    let source = include_str!("native.rs");
    let collect = source
        .split_once(concat!(
            "let object_identity = ",
            "identity_ref(conn, &oref"
        ))
        .unwrap()
        .1;
    assert!(
        collect
            .find("identity_ref(conn, &seeds[frame_ordinal]")
            .unwrap()
            < collect
                .find("object_identity.as_ref().unwrap_or(&oref)")
                .unwrap()
    );
    let route = source
        .split_once(concat!("pub(crate) fn ", "pointer_bounds("))
        .unwrap()
        .1
        .split_once(concat!("pub fn ", "perform_action("))
        .unwrap()
        .0;
    assert!(
        route
            .find("require_pointer_frame_owner(target.identity.as_ref())?")
            .unwrap()
            < route.find("get_extents").unwrap()
    );
}

#[test]
fn secondary_context_menu_names_are_normalized_not_activation_guesses() {
    for name in [
        "showContextMenu",
        "context-menu",
        "menu",
        "popup",
        "popup_menu",
    ] {
        assert!(context_menu_action(name));
    }
    for name in ["click", "press", "activate", "open", "show-menu-button", ""] {
        assert!(!context_menu_action(name));
    }
}
#[test]
fn secondary_projection_uses_supplied_geometry_without_saturation_or_fallback() {
    assert_eq!(
        project_retained_pointer_bounds(
            (100, 100, 50, 50),
            (0, -20, 600, 400),
            (10, 20),
            (0, 10),
            true
        )
        .unwrap(),
        (110, 150, 50, 50)
    );
    assert_eq!(
        project_retained_pointer_bounds(
            (100, 100, 50, 50),
            (0, 0, 600, 400),
            (10, 20),
            (0, 0),
            false
        )
        .unwrap(),
        (110, 120, 50, 50)
    );
    assert!(project_retained_pointer_bounds(
        (0, 0, 50, 50),
        (0, 0, 600, 400),
        (10, 20),
        (0, 0),
        false
    )
    .is_err());
    assert!(project_retained_pointer_bounds(
        (i32::MAX, 100, 50, 50),
        (0, 0, 600, 400),
        (10, 20),
        (0, 0),
        true
    )
    .is_err());
    assert!(project_retained_pointer_bounds(
        (100, 100, 0, 50),
        (0, 0, 600, 400),
        (10, 20),
        (0, 0),
        true
    )
    .is_err());
    let source = include_str!("native.rs");
    let route = source
        .split_once(concat!("pub(crate) fn ", "pointer_bounds("))
        .unwrap()
        .1
        .split_once(concat!("pub fn ", "perform_action(&self"))
        .unwrap()
        .0;
    for forbidden in [
        "window_geometry(",
        "window_local_to_output(",
        "screen_bounds(",
        "unwrap_or(",
        "get_element_bounds(",
        ".nth(",
    ] {
        assert!(!route.contains(forbidden));
    }
}
