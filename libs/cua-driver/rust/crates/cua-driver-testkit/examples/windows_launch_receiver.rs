//! Test-only launch receiver. Explicitly built with `cargo build --example
//! windows_launch_receiver`, never installed or linked into the driver.
//! No console host or child processes:
//! the readiness PID and top-level window owner are the actual launched image.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(not(target_os = "windows"))]
fn main() {}

#[cfg(target_os = "windows")]
fn main() -> std::io::Result<()> {
    use cua_driver_testkit::windows_process::process_identity;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::{Duration, Instant};
    use windows::core::w;
    use windows::Win32::System::Threading::GetCurrentProcess;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
        GetWindowThreadProcessId, PeekMessageW, SetForegroundWindow, ShowWindow, TranslateMessage,
        MSG, PM_REMOVE, SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WS_EX_NOACTIVATE, WS_OVERLAPPEDWINDOW,
        WS_VISIBLE,
    };

    // An absolute watchdog also covers unexpected native/API stalls. EOF or
    // release normally exits immediately; a killed test runner cannot leak us.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        std::process::exit(3);
    });
    let mut args = std::env::args().skip(1);
    let address: SocketAddr = args
        .next()
        .expect("private listener")
        .parse()
        .expect("address");
    assert!(address.ip().is_loopback());
    let nonce = args.next().expect("ownership nonce");
    // Optional hostile-to-background behavior belongs ONLY to this unshipped
    // native fixture, never to a driver test hook or environment override.
    let activate = match args.next().as_deref() {
        None => false,
        Some("--activate") => true,
        Some(_) => panic!("unknown launch receiver mode"),
    };
    assert!(args.next().is_none());
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    // STATIC is a real OS window class with a default responsive window proc.
    // Show without activation first, so --activate measures an explicit native
    // SetForegroundWindow attempt, rather than incidental creation behavior.
    let hwnd = unsafe {
        CreateWindowExW(
            if activate {
                WINDOW_EX_STYLE::default()
            } else {
                WS_EX_NOACTIVATE
            },
            w!("STATIC"),
            w!("Cua owned launch receiver"),
            if activate {
                WS_OVERLAPPEDWINDOW
            } else {
                WS_OVERLAPPEDWINDOW | WS_VISIBLE
            },
            120,
            120,
            360,
            160,
            None,
            None,
            None,
            None,
        )
    }
    .map_err(std::io::Error::other)?;
    let activation = activate.then(|| {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        let accepted = unsafe { SetForegroundWindow(hwnd) }.as_bool();
        let foreground = unsafe { GetForegroundWindow() };
        let mut owner = 0;
        unsafe { GetWindowThreadProcessId(foreground, Some(&mut owner)) };
        serde_json::json!({
            "accepted": accepted,
            "foreground_hwnd": foreground.0 as usize,
            "foreground_pid": owner,
        })
    });
    let identity = process_identity(unsafe { GetCurrentProcess() })?;
    writeln!(
        stream,
        "{}",
        serde_json::json!({
            "nonce": nonce,
            "pid": std::process::id(),
            "created": identity.created,
            "hwnd": hwnd.0 as usize,
            "activation": activation,
        })
    )?;
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(90);
    let expected_release = format!("release {nonce}\n");
    let mut release = Vec::new();
    loop {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            let _ = unsafe { TranslateMessage(&message) };
            unsafe { DispatchMessageW(&message) };
        }
        let mut buffer = [0_u8; 128];
        match stream.read(&mut buffer) {
            Ok(0) => break, // Parent panic/exit also releases the receiver.
            Ok(count) => {
                release.extend_from_slice(&buffer[..count]);
                if release == expected_release.as_bytes() {
                    break;
                }
                if release.len() >= expected_release.len() {
                    return Err(std::io::Error::other("invalid receiver release"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::other("receiver expired without release"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe { DestroyWindow(hwnd) }.map_err(std::io::Error::other)
}
