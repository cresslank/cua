//! Independent native process observations for Windows tests, never shipped.

use std::io;
use std::path::PathBuf;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_TIMEOUT};
use windows::Win32::Security::{
    CreateWellKnownSid, EqualSid, GetLengthSid, GetSidSubAuthority, GetSidSubAuthorityCount,
    GetTokenInformation, LookupPrivilegeNameW, TokenElevation, TokenGroups, TokenIntegrityLevel,
    TokenPrivileges, TokenSessionId, TokenStatistics, TokenUser, WinBuiltinAdministratorsSid,
    LUID_AND_ATTRIBUTES, PSID, SID_AND_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_GROUPS,
    TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TOKEN_QUERY,
    TOKEN_STATISTICS, TOKEN_USER,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

pub fn filetime_value(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub image: PathBuf,
    pub created: u64,
}

/// Read the exact process object behind a retained handle, not a new PID lookup.
pub fn process_identity(handle: HANDLE) -> io::Result<ProcessIdentity> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
        .map_err(io::Error::other)?;
    let mut image = vec![0_u16; 32768];
    let mut length = image.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(image.as_mut_ptr()),
            &mut length,
        )
    }
    .map_err(io::Error::other)?;
    let image = std::ffi::OsString::from_wide(&image[..length as usize]);
    Ok(ProcessIdentity {
        image: std::fs::canonicalize(image)?,
        created: filetime_value(created),
    })
}

use std::os::windows::ffi::OsStringExt;

/// Diagnostic/observation handles deliberately have no PROCESS_TERMINATE right.
pub struct ObservedProcess(HANDLE);

// SAFETY: an owned kernel process handle is process-wide, not thread-affine.
// Moving it transfers the sole CloseHandle owner; it never grants termination.
unsafe impl Send for ObservedProcess {}

impl ObservedProcess {
    pub fn open(pid: u32) -> io::Result<Self> {
        unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
        }
        .map(Self)
        .map_err(io::Error::other)
    }

    pub fn identity(&self) -> io::Result<ProcessIdentity> {
        process_identity(self.0)
    }

    pub fn is_alive(&self) -> bool {
        (unsafe { windows::Win32::System::Threading::WaitForSingleObject(self.0, 0) })
            == WAIT_TIMEOUT
    }

    pub fn token(&self) -> io::Result<TokenFacts> {
        token_facts(self.0)
    }
}

impl Drop for ObservedProcess {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenFacts {
    pub user_sid: Vec<u8>,
    pub authentication_id: u64,
    pub session_id: u32,
    pub elevated: bool,
    pub integrity_rid: u32,
    /// None means absent; Some holds the exact group attributes. A disabled
    /// allow-capable Administrators SID is not equivalent to a deny-only SID.
    pub administrators: Option<u32>,
    /// Include disabled privileges too: the token holder may enable them.
    pub privileges: Vec<String>,
}

impl TokenFacts {
    pub fn administrators_deny_only_or_absent(&self) -> bool {
        self.administrators
            .is_none_or(|attributes| attributes & 0x10 != 0)
    }

    pub fn requires_deelevation(&self) -> bool {
        self.elevated || !self.administrators_deny_only_or_absent() || self.integrity_rid >= 0x3000
    }
}

struct TokenHandle(HANDLE);
impl Drop for TokenHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn information(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<u64>> {
    let mut needed = 0;
    let _ = unsafe { GetTokenInformation(token, class, None, 0, &mut needed) };
    if needed == 0 {
        return Err(io::Error::other("token information returned no size"));
    }
    let mut data = vec![0_u64; (needed as usize).div_ceil(8)];
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(data.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .map_err(io::Error::other)?;
    Ok(data)
}

/// Independent OS readback, with no use of the production token builder/validator.
pub fn token_facts(process: HANDLE) -> io::Result<TokenFacts> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.map_err(io::Error::other)?;
    let token = TokenHandle(token);
    let user = information(token.0, TokenUser)?;
    let user_sid = unsafe { (*user.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let user_sid = unsafe {
        std::slice::from_raw_parts(user_sid.0.cast::<u8>(), GetLengthSid(user_sid) as usize)
    }
    .to_vec();
    let statistics = information(token.0, TokenStatistics)?;
    let logon = unsafe { (*statistics.as_ptr().cast::<TOKEN_STATISTICS>()).AuthenticationId };
    let authentication_id = ((logon.HighPart as u64) << 32) | u64::from(logon.LowPart);
    let session = information(token.0, TokenSessionId)?;
    let session_id = unsafe { *session.as_ptr().cast::<u32>() };
    let elevation = information(token.0, TokenElevation)?;
    let elevated = unsafe { (*elevation.as_ptr().cast::<TOKEN_ELEVATION>()).TokenIsElevated != 0 };
    let label = information(token.0, TokenIntegrityLevel)?;
    let sid = unsafe { (*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
    let count = unsafe { GetSidSubAuthorityCount(sid) };
    if count.is_null() || unsafe { *count } == 0 {
        return Err(io::Error::other("integrity SID has no subauthority"));
    }
    let integrity_rid = unsafe { *GetSidSubAuthority(sid, u32::from(*count - 1)) };

    let mut admin_sid = [0_u64; 12];
    let admin = PSID(admin_sid.as_mut_ptr().cast());
    let mut size = std::mem::size_of_val(&admin_sid) as u32;
    unsafe { CreateWellKnownSid(WinBuiltinAdministratorsSid, None, Some(admin), &mut size) }
        .map_err(io::Error::other)?;
    let groups = information(token.0, TokenGroups)?;
    let groups = groups.as_ptr().cast::<TOKEN_GROUPS>();
    let first = unsafe { std::ptr::addr_of!((*groups).Groups).cast::<SID_AND_ATTRIBUTES>() };
    let mut administrators = None;
    for index in 0..unsafe { (*groups).GroupCount } as usize {
        let group = unsafe { *first.add(index) };
        if unsafe { EqualSid(group.Sid, admin) }.is_ok() {
            administrators = Some(group.Attributes);
        }
    }

    let privileges = information(token.0, TokenPrivileges)?;
    let privileges = privileges.as_ptr().cast::<TOKEN_PRIVILEGES>();
    let first =
        unsafe { std::ptr::addr_of!((*privileges).Privileges).cast::<LUID_AND_ATTRIBUTES>() };
    let mut names = Vec::new();
    for index in 0..unsafe { (*privileges).PrivilegeCount } as usize {
        let luid = unsafe { (*first.add(index)).Luid };
        let mut name = [0_u16; 128];
        let mut length = name.len() as u32;
        unsafe {
            LookupPrivilegeNameW(
                PCWSTR::null(),
                &luid,
                Some(PWSTR(name.as_mut_ptr())),
                &mut length,
            )
        }
        .map_err(io::Error::other)?;
        names.push(String::from_utf16_lossy(&name[..length as usize]));
    }
    names.sort();
    Ok(TokenFacts {
        user_sid,
        authentication_id,
        session_id,
        elevated,
        integrity_rid,
        administrators,
        privileges: names,
    })
}

pub fn describe_process(pid: u32) -> String {
    match ObservedProcess::open(pid).and_then(|process| process.identity()) {
        Ok(identity) => format!(
            "PID={pid}, image={:?}, creation_FILETIME={:#x}",
            identity.image, identity.created
        ),
        Err(error) => format!("PID={pid}, identity unavailable: {error}"),
    }
}
