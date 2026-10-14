//! Background routes through an app's scripting interface (Apple Events).
//!
//! Some commands cannot run in a background app through accessibility or key
//! events. TextEdit keeps File > Save disabled while it is not the active app
//! (AppKit validates Save against the main window, and a background app has
//! none), and background key events never reach a menu's key equivalents.
//! Finder closes an inline rename field when it is not active, and an AXValue
//! write to a file's name cell changes only the list. An Apple Event runs the
//! same command inside the app without activating it, raising a window, or
//! touching the pointer, so these routes keep the user's front app untouched.
//!
//! Apple Events need the user's Automation consent for each target app. The
//! permission is read first without asking. When macOS has never asked, it
//! shows its consent prompt once, on the first route that needs that app.
//! When the user has turned it off, the caller gets an honest refusal.

use std::ffi::c_void;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Whether this process may send Apple Events to an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationPermission {
    Granted,
    /// macOS has not asked the user yet; the first event shows its prompt.
    NeedsConsent,
    /// The user turned it off in System Settings > Privacy & Security >
    /// Automation.
    Denied,
    /// The target app is not running.
    NotRunning,
    Unknown(i32),
}

#[repr(C)]
struct AeDesc {
    descriptor_type: u32,
    data_handle: *mut c_void,
}

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn AECreateDesc(
        type_code: u32,
        data_ptr: *const c_void,
        data_size: isize,
        result: *mut AeDesc,
    ) -> i16;
    fn AEDisposeDesc(desc: *mut AeDesc) -> i16;
    fn AEDeterminePermissionToAutomateTarget(
        target: *const AeDesc,
        event_class: u32,
        event_id: u32,
        ask_user_if_needed: u8,
    ) -> i32;
}

const TYPE_APPLICATION_BUNDLE_ID: u32 = u32::from_be_bytes(*b"bund");
const TYPE_WILDCARD: u32 = u32::from_be_bytes(*b"****");

fn permission_from_status(status: i32) -> AutomationPermission {
    match status {
        0 => AutomationPermission::Granted,
        -1743 => AutomationPermission::Denied,
        -1744 => AutomationPermission::NeedsConsent,
        -600 => AutomationPermission::NotRunning,
        other => AutomationPermission::Unknown(other),
    }
}

/// Read this process's Automation permission for `bundle_id` without asking.
pub fn automation_permission(bundle_id: &str) -> AutomationPermission {
    let mut desc = AeDesc {
        descriptor_type: 0,
        data_handle: std::ptr::null_mut(),
    };
    let bytes = bundle_id.as_bytes();
    let created = unsafe {
        AECreateDesc(
            TYPE_APPLICATION_BUNDLE_ID,
            bytes.as_ptr().cast(),
            bytes.len() as isize,
            &mut desc,
        )
    };
    if created != 0 {
        return AutomationPermission::Unknown(created as i32);
    }
    let status =
        unsafe { AEDeterminePermissionToAutomateTarget(&desc, TYPE_WILDCARD, TYPE_WILDCARD, 0) };
    unsafe { AEDisposeDesc(&mut desc) };
    permission_from_status(status)
}

/// A scripting failure with explicit mutation uncertainty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptRefusal {
    pub code: &'static str,
    pub reason: String,
    /// Execution may already have applied the requested mutation.
    pub mutation_possible: bool,
}

/// Shared with the admitted worker so a task failure cannot erase a send.
#[derive(Clone, Default)]
pub(crate) struct ScriptMutation(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl ScriptMutation {
    fn arm(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn possible(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

pub(crate) fn script_failure_result(
    operation: &str,
    code: &str,
    reason: &str,
    mutation_possible: bool,
) -> cua_driver_core::protocol::ToolResult {
    use serde_json::json;
    let advice = if mutation_possible {
        "The operation may already have applied. Take a fresh observation of the file and window before acting again; do not retry or fall back automatically."
    } else {
        "The scripting mutation was refused before execution; no scripting mutation was sent."
    };
    cua_driver_core::protocol::ToolResult::error(format!(
        "{operation} failed ({code}: {reason}). {advice}"
    ))
    .with_structured(json!({
        "code": code,
        "cause": code,
        "reason": reason,
        "path": "apple_events",
        "effect": if mutation_possible { "unverifiable" } else { "refused" },
        "verified": false,
        "mutation_possible": mutation_possible,
        "fresh_observation_required": mutation_possible,
    }))
}

fn check_cancelled() -> Result<(), ScriptRefusal> {
    cua_driver_core::blocking::check_cancelled().map_err(|error| ScriptRefusal {
        mutation_possible: false,
        code: "automation_cancelled",
        reason: error.to_string(),
    })
}

/// Run `script` with `osascript`, passing `args` to its `on run argv`
/// handler. Arguments never become script source, so a file name cannot
/// inject AppleScript. Returns the script's result text.
pub fn run_script(script: &str, args: &[&str], timeout: Duration) -> Result<String, ScriptRefusal> {
    // Arbitrary public scripts may mutate; only known observation scripts
    // use the untracked read-only route.
    run_script_tracked(script, args, timeout, Some(&ScriptMutation::default()))
}

fn run_script_tracked(
    script: &str,
    args: &[&str],
    timeout: Duration,
    mutation: Option<&ScriptMutation>,
) -> Result<String, ScriptRefusal> {
    check_cancelled()?;
    // Arm before execution, not after a successful response: an execution error
    // or timeout may follow application. The admitted worker retains ownership
    // through subprocess cleanup and readback, including cancellation.
    if let Some(mutation) = mutation {
        mutation.arm();
    }
    // Once sent, an Apple Event is a native batch: cancellation cannot recall
    // it. Keep admission and authority until the child completes or is reaped.
    let mut child = Command::new("/usr/bin/osascript")
        .arg("-")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ScriptRefusal {
            mutation_possible: false,
            code: "automation_failed",
            reason: format!("osascript could not start: {error}"),
        })?;
    // Starting the child can block. Revalidate cancellation before providing
    // any script; a child with no stdin script cannot apply this mutation.
    if let Err(error) = check_cancelled() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(error) = stdin.write_all(script.as_bytes()) {
            // A partial write may already have been executed.
            let _ = child.kill();
            let _ = child.wait();
            return Err(ScriptRefusal {
                mutation_possible: mutation.is_some(),
                code: "automation_failed",
                reason: format!("osascript input could not be written: {error}"),
            });
        }
    } else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ScriptRefusal {
            mutation_possible: false,
            code: "automation_failed",
            reason: "osascript input was unavailable before execution".into(),
        });
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ScriptRefusal {
                    mutation_possible: mutation.is_some(),
                    code: "automation_timeout",
                    reason: format!(
                        "the app did not answer within {} s (a macOS Automation prompt may be \
                         waiting for the user)",
                        timeout.as_secs()
                    ),
                });
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ScriptRefusal {
                    mutation_possible: mutation.is_some(),
                    code: "automation_failed",
                    reason: format!("osascript wait failed: {error}"),
                });
            }
        }
    }
    let output = child.wait_with_output().map_err(|error| ScriptRefusal {
        mutation_possible: mutation.is_some(),
        code: "automation_failed",
        reason: format!("osascript output could not be read: {error}"),
    })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let mut error = script_error_refusal(&stderr);
    error.mutation_possible = mutation.is_some();
    Err(error)
}

/// Map an osascript error line ("... (-1743)") to a refusal.
fn script_error_refusal(stderr: &str) -> ScriptRefusal {
    if stderr.contains("(-1743)") {
        ScriptRefusal {
            mutation_possible: false,
            code: "automation_denied",
            reason: "macOS does not allow Cua Driver to control this app (System Settings > \
                     Privacy & Security > Automation)"
                .into(),
        }
    } else {
        // osascript prints "<offset>: execution error: <message> (<code>)".
        let message = stderr
            .split_once("execution error: ")
            .map(|(_, rest)| rest)
            .unwrap_or(stderr);
        ScriptRefusal {
            mutation_possible: false,
            code: "automation_failed",
            reason: message.to_owned(),
        }
    }
}

/// Check the permission before a route runs: refuse when the user turned it
/// off, so no event is sent.
fn ensure_permitted(bundle_id: &str, app_name: &str) -> Result<(), ScriptRefusal> {
    match automation_permission(bundle_id) {
        AutomationPermission::Denied => Err(ScriptRefusal {
            mutation_possible: false,
            code: "automation_denied",
            reason: format!(
                "macOS does not allow Cua Driver to control {app_name}. The user can allow it in \
                 System Settings > Privacy & Security > Automation"
            ),
        }),
        AutomationPermission::NotRunning => Err(ScriptRefusal {
            mutation_possible: false,
            code: "automation_failed",
            reason: format!("{app_name} is not running"),
        }),
        // NeedsConsent: macOS shows its prompt once, on this event.
        _ => Ok(()),
    }
}

pub(crate) fn require_exact_app(pid: i32, bundle_id: &str) -> Result<(), ScriptRefusal> {
    let matching: Vec<_> = crate::apps::list_running_apps()
        .into_iter()
        .filter(|app| app.bundle_id.as_deref() == Some(bundle_id))
        .map(|app| app.pid)
        .collect();
    if unique_app_matches(pid, &matching) {
        return Ok(());
    }
    Err(ScriptRefusal {
        mutation_possible: false,
        code: "automation_target_unqualified",
        reason: "the scripting application does not resolve to the requested unique process".into(),
    })
}

fn unique_app_matches(pid: i32, matching: &[i32]) -> bool {
    matching == [pid]
}

const FINDER_BUNDLE_ID: &str = "com.apple.finder";

const FINDER_RENAME_SCRIPT: &str = r#"on run argv
	set theItem to (POSIX file (item 1 of argv)) as alias
	tell application id "com.apple.finder"
		set name of theItem to (item 2 of argv)
	end tell
	return "ok"
end run
"#;

/// Whether `name` can be a single file name (no path separators, not empty
/// or a dot entry).
pub fn valid_file_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

/// Rename the file or folder at `path` to `new_name` through Finder, then
/// read the folder back. Returns the new path once it exists and the old
/// name is gone.
pub fn finder_rename(path: &Path, new_name: &str) -> Result<std::path::PathBuf, ScriptRefusal> {
    finder_rename_tracked(path, new_name, &ScriptMutation::default())
}

pub(crate) fn finder_rename_tracked(
    path: &Path,
    new_name: &str,
    mutation: &ScriptMutation,
) -> Result<std::path::PathBuf, ScriptRefusal> {
    if !valid_file_name(new_name) {
        return Err(ScriptRefusal {
            mutation_possible: false,
            code: "invalid_file_name",
            reason: format!("\"{new_name}\" is not a single file name"),
        });
    }
    let parent = path.parent().ok_or_else(|| ScriptRefusal {
        mutation_possible: false,
        code: "automation_failed",
        reason: "the item has no parent folder".into(),
    })?;
    let target = parent.join(new_name);
    let same_item = path.file_name().and_then(|n| n.to_str()) == Some(new_name);
    if same_item {
        return Ok(target);
    }
    // A name that differs only in case is the same entry on a case-
    // insensitive volume; anything else must not exist yet.
    let case_only = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|old| old.eq_ignore_ascii_case(new_name));
    let target_exists = target.try_exists().map_err(|error| ScriptRefusal {
        mutation_possible: false,
        code: "automation_failed",
        reason: format!("cannot observe the target name: {error}"),
    })?;
    if !case_only && target_exists {
        return Err(ScriptRefusal {
            mutation_possible: false,
            code: "name_taken",
            reason: format!(
                "\"{new_name}\" already exists in {}; nothing was renamed",
                parent.display()
            ),
        });
    }
    ensure_permitted(FINDER_BUNDLE_ID, "Finder")?;
    let path_text = path.to_string_lossy();
    run_script_tracked(
        FINDER_RENAME_SCRIPT,
        &[&path_text, new_name],
        Duration::from_secs(30),
        Some(mutation),
    )?;
    // Read the folder back: the new name is listed and the old one is gone.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        check_cancelled().map_err(|mut error| {
            error.mutation_possible = true;
            error
        })?;
        if folder_lists(parent, new_name) && (case_only || matches!(path.try_exists(), Ok(false))) {
            return Ok(target);
        }
        if Instant::now() >= deadline {
            return Err(ScriptRefusal {
                mutation_possible: true,
                code: "rename_unconfirmed",
                reason: format!(
                    "Finder accepted the rename, but {} does not list \"{new_name}\"",
                    parent.display()
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn folder_lists(folder: &Path, name: &str) -> bool {
    std::fs::read_dir(folder).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().to_str() == Some(name))
    })
}

/// Save the open document whose file is `path` in the app `bundle_id`
/// through the Cocoa Standard Suite `save` command, then read its modified
/// flag back. Raw codes keep the script independent of the app's
/// terminology: `docu` document, `ppth` its path (TextEdit) or `file` its
/// file (the Standard Suite), `imod` modified, `core/save` save. Returns `false` when the app still reports unsaved
/// changes.
const SAVE_DOCUMENT_SCRIPT: &str = r#"on run argv
	set thePath to item 1 of argv
	tell application id (item 2 of argv)
		set matchedDocument to missing value
		repeat with theDocument in (every «class docu»)
			set theDocumentPath to missing value
			try
				set theDocumentPath to «property ppth» of theDocument
			end try
			if theDocumentPath is missing value then
				try
					set theDocumentPath to POSIX path of ((«property file» of theDocument) as alias)
				end try
			end if
			if theDocumentPath is thePath then
				if matchedDocument is not missing value then return "ambiguous-document"
				set matchedDocument to contents of theDocument
			end if
		end repeat
		if matchedDocument is missing value then return "no-document"
		«event coresave» matchedDocument
		return («property imod» of matchedDocument) as text
	end tell
end run
"#;

/// Read the modified flag of the open document whose file is `path`, the
/// same document lookup as [`SAVE_DOCUMENT_SCRIPT`] without saving.
const DOCUMENT_MODIFIED_SCRIPT: &str = r#"on run argv
	set thePath to item 1 of argv
	tell application id (item 2 of argv)
		repeat with theDocument in (every «class docu»)
			set theDocumentPath to missing value
			try
				set theDocumentPath to «property ppth» of theDocument
			end try
			if theDocumentPath is missing value then
				try
					set theDocumentPath to POSIX path of ((«property file» of theDocument) as alias)
				end try
			end if
			if theDocumentPath is thePath then
				return («property imod» of theDocument) as text
			end if
		end repeat
	end tell
	return "no-document"
end run
"#;

/// Whether the app reports unsaved changes in the document at `path`.
/// Only reads when Automation is already granted, so a readback never shows
/// the consent prompt; `None` when it cannot tell.
pub fn document_modified(bundle_id: &str, path: &Path) -> Option<bool> {
    if automation_permission(bundle_id) != AutomationPermission::Granted {
        return None;
    }
    let path_text = path.to_string_lossy();
    match run_script_tracked(
        DOCUMENT_MODIFIED_SCRIPT,
        &[&path_text, bundle_id],
        Duration::from_secs(5),
        None,
    )
    .ok()?
    .as_str()
    {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Outcome of a background save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedDocument {
    /// The file's modification time moved during the save.
    pub file_written: bool,
}

pub fn save_document(
    bundle_id: &str,
    app_name: &str,
    path: &Path,
) -> Result<SavedDocument, ScriptRefusal> {
    save_document_tracked(bundle_id, app_name, path, &ScriptMutation::default())
}

pub(crate) fn save_document_tracked(
    bundle_id: &str,
    app_name: &str,
    path: &Path,
    mutation: &ScriptMutation,
) -> Result<SavedDocument, ScriptRefusal> {
    ensure_permitted(bundle_id, app_name)?;
    let before = modified_time(path);
    let path_text = path.to_string_lossy();
    let result = run_script_tracked(
        SAVE_DOCUMENT_SCRIPT,
        &[&path_text, bundle_id],
        Duration::from_secs(30),
        Some(mutation),
    )?;
    match result.as_str() {
        "false" => {}
        "true" => {
            return Err(ScriptRefusal {
                mutation_possible: true,
                code: "save_unconfirmed",
                reason: format!(
                    "{app_name} accepted the save, but still reports unsaved changes in {}",
                    path.display()
                ),
            })
        }
        "no-document" | "ambiguous-document" => {
            return Err(ScriptRefusal {
                mutation_possible: false,
                code: "automation_target_unqualified",
                reason: format!(
                    "{app_name} could not uniquely identify the open document for {}",
                    path.display()
                ),
            })
        }
        other => {
            return Err(ScriptRefusal {
                mutation_possible: true,
                code: "save_unconfirmed",
                reason: format!("{app_name} answered \"{other}\" to the save"),
            })
        }
    }
    let file_written = wait_for_new_mtime(path, before, Duration::from_millis(1500));
    check_cancelled().map_err(|mut error| {
        error.mutation_possible = true;
        error
    })?;
    Ok(SavedDocument { file_written })
}

pub fn modified_time(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Wait up to `timeout` for `path`'s modification time to move past
/// `before`.
pub fn wait_for_new_mtime(
    path: &Path,
    before: Option<std::time::SystemTime>,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check_cancelled().is_err() {
            return false;
        }
        let now = modified_time(path);
        if mtime_advanced(before, now) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn mtime_advanced(
    before: Option<std::time::SystemTime>,
    now: Option<std::time::SystemTime>,
) -> bool {
    matches!((before, now), (Some(before), Some(now)) if now > before)
}

/// The file path of a `file://` URL string, as `AXDocument` and `AXURL`
/// report them.
pub fn file_url_to_path(url: &str) -> Option<std::path::PathBuf> {
    use core_foundation::{base::TCFType, string::CFString, url::CFURL};
    if !url.starts_with("file://") {
        return None;
    }
    let text = CFString::new(url);
    let raw = unsafe {
        core_foundation::url::CFURLCreateWithString(
            std::ptr::null(),
            text.as_concrete_TypeRef(),
            std::ptr::null(),
        )
    };
    if raw.is_null() {
        return None;
    }
    let url = unsafe { CFURL::wrap_under_create_rule(raw) };
    url.to_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripting_target_requires_one_exact_process() {
        assert!(unique_app_matches(7, &[7]));
        for matching in [&[][..], &[8][..], &[7, 8][..], &[7, 7][..]] {
            assert!(!unique_app_matches(7, matching));
        }
    }

    #[test]
    fn missing_or_failed_mtime_observation_never_confirms_a_write() {
        let before = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(2);
        let later = before + Duration::from_secs(1);
        assert!(mtime_advanced(Some(before), Some(later)));
        assert!(!mtime_advanced(None, Some(later)));
        assert!(!mtime_advanced(Some(before), None));
        assert!(!mtime_advanced(Some(before), Some(before)));
        assert!(!mtime_advanced(Some(later), Some(before)));
    }

    #[test]
    fn permission_codes_map_to_states() {
        assert_eq!(permission_from_status(0), AutomationPermission::Granted);
        assert_eq!(permission_from_status(-1743), AutomationPermission::Denied);
        assert_eq!(
            permission_from_status(-1744),
            AutomationPermission::NeedsConsent
        );
        assert_eq!(
            permission_from_status(-600),
            AutomationPermission::NotRunning
        );
        assert_eq!(
            permission_from_status(-50),
            AutomationPermission::Unknown(-50)
        );
    }

    #[test]
    fn script_errors_name_a_denied_permission() {
        let denied = script_error_refusal(
            "0:120: execution error: Not authorized to send Apple events to Finder. (-1743)",
        );
        assert_eq!(denied.code, "automation_denied");
        let other = script_error_refusal(
            "0:98: execution error: Finder got an error: The name \"b.txt\" is already taken. (-48)",
        );
        assert_eq!(other.code, "automation_failed");
        assert!(other.reason.starts_with("Finder got an error"));
    }

    #[test]
    fn file_names_are_single_path_components() {
        assert!(valid_file_name("renamed.txt"));
        assert!(valid_file_name("with space é.md"));
        for bad in ["", ".", "..", "a/b", "x\0y"] {
            assert!(!valid_file_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn file_urls_decode_to_paths() {
        assert_eq!(
            file_url_to_path("file:///Users/lume/xfb/te/a%20main.txt"),
            Some("/Users/lume/xfb/te/a main.txt".into())
        );
        assert_eq!(file_url_to_path("https://example.com/a"), None);
    }

    #[test]
    fn a_rename_onto_an_existing_name_is_refused_before_any_event() {
        let dir = std::env::temp_dir().join(format!("cua-ae-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("b.txt"), "b").unwrap();
        let refused = finder_rename(&dir.join("a.txt"), "b.txt").unwrap_err();
        assert_eq!(refused.code, "name_taken");
        assert_eq!(
            finder_rename(&dir.join("a.txt"), "x/y").unwrap_err().code,
            "invalid_file_name"
        );
        // Renaming to the current name is already done; nothing is sent.
        assert_eq!(
            finder_rename(&dir.join("a.txt"), "a.txt").unwrap(),
            dir.join("a.txt")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scripts_pass_values_as_arguments_only() {
        // Values reach the scripts through argv, never through the source.
        assert!(FINDER_RENAME_SCRIPT.contains("item 1 of argv"));
        assert!(SAVE_DOCUMENT_SCRIPT.contains("item 2 of argv"));
        assert!(!FINDER_RENAME_SCRIPT.contains('{'));
    }
}
