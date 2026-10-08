use super::*;
use std::cell::RefCell;

#[test]
fn secondary_buffered_motion_completes_before_validation_and_press() {
    // Model RustConnection: merely queuing a warp does not change the pointer;
    // a checked cookie completes pending work on the injection connection.
    #[derive(Default)]
    struct Connection {
        queued_motion: bool,
        moved: bool,
        events: Vec<&'static str>,
    }
    let conn = RefCell::new(Connection::default());
    conn.borrow_mut().queued_motion = true;
    complete_motion_before_validation(
        || {
            let mut conn = conn.borrow_mut();
            assert!(conn.queued_motion);
            conn.queued_motion = false;
            conn.moved = true;
            conn.events.push("motion completed");
            Ok(())
        },
        &mut || {
            let mut conn = conn.borrow_mut();
            assert!(
                conn.moved && !conn.queued_motion,
                "validation before buffered warp completed"
            );
            conn.events.push("validated");
            Ok(())
        },
    )
    .unwrap();
    conn.borrow_mut().events.push("press");
    assert_eq!(
        conn.borrow().events,
        ["motion completed", "validated", "press"]
    );
}

#[test]
fn secondary_failed_motion_never_validates_and_failed_validation_never_presses() {
    let mut validations = 0;
    assert!(complete_motion_before_validation(
        || anyhow::bail!("motion transport failed"),
        &mut || {
            validations += 1;
            Ok(())
        }
    )
    .is_err());
    assert_eq!(validations, 0);
    let mut presses = 0;
    if complete_motion_before_validation(|| Ok(()), &mut || {
        anyhow::bail!("retained validation failed")
    })
    .is_ok()
    {
        presses += 1;
    }
    assert_eq!(presses, 0);
}
