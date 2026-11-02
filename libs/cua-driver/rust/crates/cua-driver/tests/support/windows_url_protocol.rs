//! Native, per-user, single-use URL protocol registration for ignored tests.
//! Never changes a default browser, machine-wide association or existing key.

use std::io;
use std::path::Path;
use std::thread;

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_ALL_ACCESS, KEY_READ, REG_CREATED_NEW_KEY,
    REG_CREATE_KEY_DISPOSITION, REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE,
};

use super::windows_launch_receiver::ReceiverLease;

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        if let Err(error) = unsafe { RegCloseKey(self.0) }.ok() {
            eprintln!("[launch_app_foreground] registry handle close failed: {error}");
            if !thread::panicking() {
                panic!("close fixture registry key: {error}");
            }
        }
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn create(parent: HKEY, path: &str) -> (Key, REG_CREATE_KEY_DISPOSITION) {
    let path = wide(path);
    let mut key = HKEY::default();
    let mut disposition = REG_CREATE_KEY_DISPOSITION::default();
    unsafe {
        RegCreateKeyExW(
            parent,
            PCWSTR(path.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_ALL_ACCESS,
            None,
            &mut key,
            Some(&mut disposition),
        )
    }
    .ok()
    .expect("create owned protocol registry key");
    (Key(key), disposition)
}

fn read(key: HKEY, name: &str) -> Vec<u16> {
    let name = wide(name);
    let mut value = vec![0_u16; 4096];
    let mut bytes = (value.len() * 2) as u32;
    let mut kind = REG_VALUE_TYPE::default();
    unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(value.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    }
    .ok()
    .expect("read back protocol registry value");
    assert_eq!(kind, REG_SZ, "fixture registry value type");
    assert_eq!(bytes % 2, 0, "fixture registry UTF-16 byte count");
    value.truncate(bytes as usize / 2);
    value
}

fn write(key: HKEY, name: &str, value: &str) {
    let name_w = wide(name);
    let value_w = wide(value);
    let bytes = unsafe { std::slice::from_raw_parts(value_w.as_ptr().cast(), value_w.len() * 2) };
    unsafe { RegSetValueExW(key, PCWSTR(name_w.as_ptr()), None, REG_SZ, Some(bytes)) }
        .ok()
        .expect("write owned protocol registry value");
    assert_eq!(read(key, name), value_w, "protocol value readback");
}

pub struct UrlProtocol {
    pub url: String,
    path: Vec<u16>,
    key: Option<Key>,
}

impl UrlProtocol {
    pub fn new(image: &Path, receiver: &ReceiverLease) -> Self {
        let scheme = format!("cua-launch-{}", uuid::Uuid::new_v4().simple());
        let path = format!(r"Software\Classes\{scheme}");
        let (key, disposition) = create(HKEY_CURRENT_USER, &path);
        // No registry writes, deletion authority or retry if the name exists.
        assert_eq!(
            disposition, REG_CREATED_NEW_KEY,
            "refuse existing URL scheme"
        );
        // Arm cleanup immediately after exclusive creation, before any writes.
        let registration = Self {
            url: format!("{scheme}://owned-foreground-regression"),
            path: wide(&path),
            key: Some(key),
        };
        let root = registration.key.as_ref().unwrap().0;
        write(root, "", "URL:Cua owned launch regression");
        write(root, "URL Protocol", "");
        let (command, _) = create(root, r"shell\open\command");
        // Deliberately does not consume %1: only our private receiver arguments
        // reach the fixture. No interpreter, admin or global browser change.
        let command_line = format!(
            "\"{}\" {} {} --activate",
            image.display(),
            receiver.address,
            receiver.nonce,
        );
        write(command.0, "", &command_line);
        eprintln!("[launch_app_foreground] owned HKCU URL scheme registered: {scheme}");
        registration
    }

    fn remove(&mut self) -> io::Result<()> {
        if self.key.is_none() {
            return Ok(());
        }
        // The only admitted root was freshly created, never an existing key.
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(self.path.as_ptr())) }
            .ok()
            .map_err(io::Error::other)?;
        self.key.take();
        let mut probe = HKEY::default();
        let result = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(self.path.as_ptr()),
                None,
                KEY_READ,
                &mut probe,
            )
        };
        if result.is_ok() {
            drop(Key(probe));
        }
        if result != ERROR_FILE_NOT_FOUND {
            return Err(io::Error::other(format!(
                "owned URL scheme cleanup readback: expected ERROR_FILE_NOT_FOUND, got {result:?}"
            )));
        }
        eprintln!("[launch_app_foreground] owned HKCU URL scheme removed (native readback)");
        Ok(())
    }
}

impl Drop for UrlProtocol {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            eprintln!("[launch_app_foreground] URL scheme cleanup failed: {error}");
            if !thread::panicking() {
                panic!("owned URL scheme cleanup failed: {error}");
            }
        }
    }
}
