use super::*;
use std::sync::mpsc;

// A real NativeTask is dropped while the first native mutation is inside the
// sink. No wall-clock deadline race, X connection, device or desktop is needed.
async fn cancelled_route(route: u8) {
    let (first, started) = mpsc::channel();
    let (resume, wait) = mpsc::channel();
    let (done, finished) = mpsc::channel();
    let task = cua_driver_core::blocking::spawn(move || {
        let mut events = Vec::new();
        let mut first = Some(first);
        let mut record = |release: bool, code: u16| {
            events.push((release, code));
            if !release {
                if let Some(first) = first.take() {
                    first.send(()).unwrap();
                    wait.recv().unwrap();
                }
            }
        };
        let result = match route {
            0 => send_xsend_key_sequence(
                &[(50, KeyButMask::SHIFT), (37, KeyButMask::CONTROL)],
                38,
                |kind, code, _, _| {
                    record(kind == KEY_RELEASE_EVENT, u16::from(code));
                    Ok(())
                },
            ),
            1 => send_xtest_key_sequence(&[50, 37, 38], |release, code| {
                record(release, u16::from(code));
                Ok(())
            }),
            2 => {
                use mpx_keyboard::{deliver_steps, KeyStep};
                deliver_steps(
                    &[
                        KeyStep {
                            evdev_code: 50,
                            press: true,
                        },
                        KeyStep {
                            evdev_code: 37,
                            press: true,
                        },
                        KeyStep {
                            evdev_code: 37,
                            press: false,
                        },
                        KeyStep {
                            evdev_code: 50,
                            press: false,
                        },
                    ],
                    |step| {
                        record(!step.press, step.evdev_code);
                        Ok(())
                    },
                )
                .map(|_| ())
            }
            _ => {
                let mut emit = |frames: &[InputEvent]| {
                    for event in frames {
                        record(
                            event.event_type() == EventType::KEY && event.value() == 0,
                            event.code(),
                        );
                    }
                    Ok(())
                };
                let gesture = (|| {
                    emit_button_with(1, true, &mut emit)?;
                    match route {
                        3 => emit_button_with(1, true, &mut emit)?,
                        4 => emit_relative_motion_with(5, 6, &mut emit)?,
                        _ => emit_scroll_with(false, 1, &mut emit)?,
                    }
                    Ok(())
                })();
                let release = emit_button_with(1, false, &mut emit);
                gesture.and(release)
            }
        };
        done.send((result.map_err(|error| error.to_string()), events))
            .unwrap();
    });
    tokio::task::yield_now().await;
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(task);
    resume.send(()).unwrap();
    let (result, events) = finished.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        result.unwrap_err(),
        "native worker deadline expired or its waiter was cancelled"
    );
    let code = if route < 3 {
        50
    } else {
        button_code(1).unwrap().0
    };
    assert_eq!(events, [(false, code), (true, code)]);
}

#[tokio::test]
async fn cancellation_xsend_chord_unwinds_only_held_modifier() {
    cancelled_route(0).await;
}
#[tokio::test]
async fn cancellation_xtest_chord_unwinds_only_held_modifier() {
    cancelled_route(1).await;
}
#[tokio::test]
async fn cancellation_mpx_keyboard_unwinds_held_modifier() {
    cancelled_route(2).await;
}
#[tokio::test]
async fn cancellation_mpx_button_stops_next_press() {
    cancelled_route(3).await;
}
#[tokio::test]
async fn cancellation_mpx_motion_retains_button_release() {
    cancelled_route(4).await;
}
#[tokio::test]
async fn cancellation_mpx_wheel_retains_button_release() {
    cancelled_route(5).await;
}

#[tokio::test]
async fn text_preflight_cancellation_stops_remapping_and_emission() {
    for during_remap in [true, false] {
        let (started, ready) = mpsc::channel();
        let (resume, wait) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let task = cua_driver_core::blocking::spawn(move || {
            let mut mapping = GetKeyboardMappingReply {
                keysyms_per_keycode: 1,
                sequence: 0,
                keysyms: vec![0; 248],
            };
            if !during_remap {
                mapping.keysyms[0] = 'a' as u32;
                mapping.keysyms[1] = 'b' as u32;
            }
            let mut remaps = Vec::new();
            let mut emitted = Vec::new();
            let result = type_text_with_preflight::<()>(
                "ab",
                &mapping,
                |ch, _| {
                    remaps.push(ch);
                    started.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok((8, None))
                },
                |key, _| {
                    emitted.push(key);
                    started.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                },
            );
            done.send((result.unwrap_err().to_string(), remaps, emitted))
                .unwrap();
        });
        tokio::task::yield_now().await;
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(task);
        resume.send(()).unwrap();
        let (error, remaps, emitted) = finished.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            error,
            "native worker deadline expired or its waiter was cancelled"
        );
        if during_remap {
            assert_eq!(remaps, ['a']);
            assert!(emitted.is_empty());
        } else {
            assert!(remaps.is_empty());
            assert_eq!(emitted, [8]);
        }
    }
}
