//! Owned foreground precondition for the ignored Edge desktop test only.
//!
//! A hosted runner's arbitrary foreground HWND may disappear or belong to a
//! process that cannot delegate foreground permission. Keep a real message pump
//! alive through the launch and grant permission to the exact test daemon. This
//! does not change ForegroundLockTimeout, unlock another process's foreground
//! lock, synthesize input, or relax the driver's restoration/error contract.

use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CreateWindowExW, DestroyWindow, DispatchMessageW,
    GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, PostThreadMessageW,
    SetForegroundWindow, TranslateMessage, MSG, WINDOW_EX_STYLE, WM_QUIT, WS_OVERLAPPEDWINDOW,
    WS_VISIBLE,
};

pub struct ForegroundFixture {
    hwnd: usize,
    thread_id: u32,
    pump: Option<JoinHandle<()>>,
}

impl ForegroundFixture {
    pub fn new() -> Self {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let pump = thread::spawn(move || unsafe {
            // The built-in STATIC class needs no global class registration.
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Cua Edge launch foreground fixture"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                480,
                240,
                None,
                None,
                None,
                None,
            )
            .expect("create foreground fixture window");
            // CreateWindow initializes the thread's message queue before the
            // controller can post WM_QUIT, including during assertion unwind.
            ready_tx
                .send((hwnd.0 as usize, GetCurrentThreadId()))
                .expect("publish foreground fixture identity");
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            DestroyWindow(hwnd).expect("destroy owned foreground fixture window");
        });
        let (hwnd, thread_id) = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("foreground fixture readiness");
        let fixture = Self {
            hwnd,
            thread_id,
            pump: Some(pump),
        };
        assert!(
            unsafe { SetForegroundWindow(fixture.hwnd()) }.as_bool(),
            "Windows denied activation of the owned foreground fixture"
        );
        fixture.assert_foreground();
        eprintln!(
            "Edge launch prior: owned fixture HWND={:#x}, PID={}",
            fixture.hwnd,
            std::process::id()
        );
        fixture
    }

    fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut _)
    }

    pub fn grant_to(&self, daemon_pid: u32) {
        self.assert_foreground();
        unsafe { AllowSetForegroundWindow(daemon_pid) }
            .expect("foreground fixture must grant permission to the exact test daemon");
        eprintln!("Edge launch foreground permission granted to daemon PID={daemon_pid}");
    }

    pub fn assert_foreground(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let current = unsafe { GetForegroundWindow() };
            let mut owner = 0;
            unsafe { GetWindowThreadProcessId(current, Some(&mut owner)) };
            if current == self.hwnd() && owner == std::process::id() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "expected owned foreground HWND={:#x} PID={}, got HWND={:#x} PID={owner}",
                self.hwnd,
                std::process::id(),
                current.0 as usize
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ForegroundFixture {
    fn drop(&mut self) {
        // Post to the owning message-pump thread; never destroy its HWND from
        // the test thread or leave a fixture behind on an assertion failure.
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if posted.is_ok() {
            if let Some(pump) = self.pump.take() {
                let _ = pump.join();
            }
        }
    }
}
