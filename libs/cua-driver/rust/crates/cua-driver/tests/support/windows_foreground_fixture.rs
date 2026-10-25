//! Owned foreground precondition for the ignored Edge desktop test only.
//!
//! Keep a responsive owned window alive through launch, grant permission only
//! to the test daemon, and independently verify the exact restored HWND/PID.
//! With explicit hosted-CI opt-in, temporarily normalize the runner's foreground
//! lock timeout and restore/read back its original value. Product callers retain
//! their Windows policy and the driver's honest restoration/error contract.

use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CreateWindowExW, DestroyWindow, DispatchMessageW,
    GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, PostThreadMessageW,
    SetForegroundWindow, SystemParametersInfoW, TranslateMessage, MSG, SPIF_SENDCHANGE,
    SPI_GETFOREGROUNDLOCKTIMEOUT, SPI_SETFOREGROUNDLOCKTIMEOUT,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINDOW_EX_STYLE, WM_QUIT, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use super::windows_foreground_policy::{policy_opt_in, PolicyOptIn};

/// Normalize an OS prerequisite of the positive AppsFolder gate, not a product
/// permission bypass. Hosted images can set this timeout to i32::MAX, making
/// restoration depend on an incidental foreground permission chain. The setting
/// is session-scoped, explicitly opted in, never persisted, and read back both
/// after the change and restoration. No input or global foreground unlock is used.
struct ForegroundPolicyScope {
    original: u32,
    active: bool,
}

impl ForegroundPolicyScope {
    fn read() -> windows::core::Result<u32> {
        let mut timeout = 0_u32;
        unsafe {
            SystemParametersInfoW(
                SPI_GETFOREGROUNDLOCKTIMEOUT,
                0,
                Some((&mut timeout as *mut u32).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )?;
        }
        Ok(timeout)
    }

    fn write_and_confirm(timeout: u32) -> bool {
        // SPI_SETFOREGROUNDLOCKTIMEOUT encodes the value in pvParam itself,
        // not in a pointed-to integer. SPIF_SENDCHANGE does not persist it.
        unsafe {
            SystemParametersInfoW(
                SPI_SETFOREGROUNDLOCKTIMEOUT,
                0,
                Some(timeout as usize as *mut std::ffi::c_void),
                SPIF_SENDCHANGE,
            )
        }
        .is_ok()
            && Self::read().ok() == Some(timeout)
    }

    fn for_ci() -> Option<Self> {
        let allow = std::env::var("CUA_TEST_ALLOW_FOREGROUND_POLICY").ok();
        let actions = std::env::var("GITHUB_ACTIONS").ok();
        let runner = std::env::var("RUNNER_ENVIRONMENT").ok();
        match policy_opt_in(allow.as_deref(), actions.as_deref(), runner.as_deref()) {
            PolicyOptIn::Unchanged => return None,
            PolicyOptIn::HostedCi => {}
            PolicyOptIn::Refuse => {
                panic!("foreground policy opt-in is only for disposable hosted runners")
            }
        }
        let scope = Self {
            original: Self::read().expect("read original CI foreground lock timeout"),
            active: true,
        };
        assert!(
            Self::write_and_confirm(0),
            "normalize CI foreground lock timeout"
        );
        eprintln!(
            "CI ForegroundLockTimeout: {} -> 0 (API readback; not persisted)",
            scope.original
        );
        Some(scope)
    }

    fn restore(&mut self) -> bool {
        if !self.active {
            return true;
        }
        if !Self::write_and_confirm(self.original) {
            return false;
        }
        self.active = false;
        eprintln!(
            "CI ForegroundLockTimeout restored: {} (API readback)",
            self.original
        );
        true
    }
}

impl Drop for ForegroundPolicyScope {
    fn drop(&mut self) {
        if !self.restore() {
            eprintln!("CI ForegroundLockTimeout restoration FAILED");
            if !thread::panicking() {
                panic!("CI foreground policy was not restored");
            }
        }
    }
}

pub struct ForegroundFixture {
    hwnd: usize,
    thread_id: u32,
    pump: Option<JoinHandle<()>>,
    policy: Option<ForegroundPolicyScope>,
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
        let mut fixture = Self {
            hwnd,
            thread_id,
            pump: Some(pump),
            policy: None,
        };
        assert!(
            unsafe { SetForegroundWindow(fixture.hwnd()) }.as_bool(),
            "Windows denied activation of the owned foreground fixture"
        );
        fixture.assert_foreground();
        fixture.policy = ForegroundPolicyScope::for_ci();
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
        // Restore while the owned prior HWND is still alive. A failed readback
        // must fail the test, but still tear down its message-pump thread.
        let restored = self.policy.as_mut().is_none_or(|policy| policy.restore());
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if posted.is_ok() {
            if let Some(pump) = self.pump.take() {
                let _ = pump.join();
            }
        }
        if !restored && !thread::panicking() {
            panic!("CI foreground policy was not restored before fixture teardown");
        }
    }
}
