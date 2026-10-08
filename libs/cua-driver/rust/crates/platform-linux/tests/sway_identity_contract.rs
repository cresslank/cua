#[test]
fn sway_activation_precedes_handle_matching_and_returns_exact_lease() {
    let source = include_str!("../src/wayland/mod.rs");
    let activation = source
        .split_once(concat!("pub fn ", "activate_window_for_input_target("))
        .unwrap()
        .1
        .split_once(concat!("pub fn ", "with_target_foreground<"))
        .unwrap()
        .0;
    let shell = activation
        .find(concat!("if shell_helper::", "available() {"))
        .unwrap();
    let sway = activation
        .find(concat!(
            "if let Some(window) = sway_ipc::",
            "window_for_id(window_id) {"
        ))
        .expect("Sway-known ids need an exact IPC branch");
    let generic = activation.find(concat!("matching_", "handle(")).unwrap();
    assert!(shell < sway && sway < generic);
    let branch = &activation[sway..];
    let branch = branch
        .split_once(concat!("let conn = Connection::", "connect_to_env()?"))
        .unwrap()
        .0;
    let pid_check = branch
        .find(concat!("if window.pid != ", "target.pid"))
        .unwrap();
    let focus = branch
        .find(concat!(
            "sway_ipc::",
            "focus_exact(target.pid, window_id)?;"
        ))
        .unwrap();
    let returned = branch
        .find(concat!("return Ok(", "ForegroundInputGuard {"))
        .unwrap();
    assert!(pid_check < focus && focus < returned);
    assert!(branch[..focus].contains("stale_target:"));
    assert!(branch[returned..].contains(concat!("_transaction: ", "None")));
    assert!(branch[returned..].contains(concat!("_lease: ", "Some(lease)")));
    assert!(branch[returned..].contains(concat!("_inject_target: ", "None")));
}
