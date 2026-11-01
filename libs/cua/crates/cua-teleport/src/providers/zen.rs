// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Zen → Firefox normalized interoperability. No raw profile, keys, history,
//! preferences, containers, workspaces or extension state enter the bundle.

use crate::gecko_profile::{self, ResolvedGeckoProfile, fail};
use crate::host::{HostEffects, default_host};
use crate::{
    AppRef, ExportProvider, ManifestItem, Platform, Result, TransferManifest, TransferScope,
};
use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

// Wire keys are a closed allowlist, not source file-copy authority.
pub use crate::layout::zen::{APP_IDS, COOKIES, DISPLAY, ID, LOGINS, STORAGE, TABS_JSON};

pub struct ZenProvider {
    profile: Option<PathBuf>,
    host: Arc<dyn HostEffects>,
    max_total_bytes: u64,
    // Bind review and capture to the same directory incarnation, even if an
    // installation default changes while authorization is pending.
    resolved: OnceLock<ResolvedGeckoProfile>,
}
impl Default for ZenProvider {
    fn default() -> Self {
        Self::new()
    }
}
impl ZenProvider {
    pub fn new() -> Self {
        Self {
            profile: None,
            host: default_host(),
            max_total_bytes: crate::bundle::DEFAULT_MAX_TOTAL_BYTES,
            resolved: OnceLock::new(),
        }
    }
    pub fn with_host(mut self, host: Arc<dyn HostEffects>) -> Self {
        self.host = host;
        self.resolved.take();
        self
    }
    pub fn with_profile(mut self, selector: impl Into<PathBuf>) -> Self {
        self.profile = Some(selector.into());
        self.resolved.take();
        self
    }
    pub fn with_max_total_bytes(mut self, bytes: u64) -> Self {
        self.max_total_bytes = bytes;
        self
    }
    pub fn profiles(&self, platform: Platform) -> Result<Vec<ResolvedGeckoProfile>> {
        gecko_profile::profiles(&*self.host, platform)
    }
    pub fn resolve_profile(&self, platform: Platform) -> Result<ResolvedGeckoProfile> {
        if !self.platform_supported(platform) {
            return Err(fail(
                "zen_platform_unsupported",
                "Zen export requires Linux",
            ));
        }
        if self.resolved.get().is_none() {
            let p = gecko_profile::resolve_profile(&*self.host, platform, self.profile.as_deref())?;
            let _ = self.resolved.set(p);
        }
        let p = self.resolved.get().unwrap().clone();
        #[cfg(target_os = "linux")]
        p.validate()?;
        Ok(p)
    }
    fn check_app(&self, app: &AppRef) -> Result<()> {
        if !self.matches(app) {
            return Err(fail(
                "zen_selection_invalid",
                "application does not identify Zen",
            ));
        }
        Ok(())
    }
}
fn item(label: &str, key: &str, sensitive: bool, count: Option<u64>) -> ManifestItem {
    ManifestItem {
        label: label.to_owned(),
        rel_path: key.to_owned(),
        est_bytes: 0,
        sensitive,
        count,
        count_noun: count.map(|_| "tabs".to_owned()),
        default_checked: !sensitive,
    }
}
impl ExportProvider for ZenProvider {
    fn id(&self) -> &str {
        ID
    }
    fn display_name(&self) -> &str {
        DISPLAY
    }
    fn host(&self) -> &dyn HostEffects {
        &*self.host
    }
    fn platform_supported(&self, platform: Platform) -> bool {
        cfg!(target_os = "linux") && platform == Platform::Linux
    }
    fn matches(&self, app: &AppRef) -> bool {
        APP_IDS
            .iter()
            .any(|id| id.eq_ignore_ascii_case(&app.app_id))
    }
    fn app_ids(&self) -> &[&str] {
        APP_IDS
    }
    fn manifest(
        &self,
        app: &AppRef,
        window: Option<&crate::WindowRef>,
        scope: TransferScope,
    ) -> Result<TransferManifest> {
        self.check_app(app)?;
        if window.is_some() {
            return Err(fail(
                "zen_selection_invalid",
                "window-scoped Zen capture is not supported",
            ));
        }
        let p = self.resolve_profile(app.platform)?;
        p.ensure_closed()?;
        let tabs = crate::gecko_store::read_tabs(&p)?;
        let mut items = vec![item("Open tabs", TABS_JSON, false, Some(tabs.len() as u64))];
        #[cfg(target_os = "linux")]
        if scope == TransferScope::FullProfile {
            // Metadata only: saved passwords are never decrypted during review.
            if p.dir_fd
                .open(std::path::Path::new("cookies.sqlite"))?
                .is_some()
            {
                items.push(item("Cookies and sessions", COOKIES, true, None));
            }
            if p.dir_fd
                .child(std::path::Path::new("storage/default"))?
                .is_some()
            {
                items.push(item(
                    "Local storage (may contain credentials)",
                    STORAGE,
                    true,
                    None,
                ));
            }
            if p.dir_fd
                .open(std::path::Path::new("logins.json"))?
                .is_some()
            {
                items.push(item(
                    "Saved passwords (separate explicit approval)",
                    LOGINS,
                    true,
                    None,
                ));
            }
        }
        p.ensure_closed()?;
        Ok(TransferManifest { provider_id: ID.to_owned(), app_display_name: DISPLAY.to_owned(), scope, items, total_est_bytes: 0,
            notes: vec!["Zen → Firefox migration: only normalized HTTP(S) tabs and explicitly selected cookies, localStorage or saved logins.".to_owned(),
                "Close Zen before capture. Active POSIX locks and stale symlink locks are refused, never removed; file-set snapshots are not atomic across databases.".to_owned(),
                "History, keys, raw session snapshots, preferences, containers, workspaces, extensions and arbitrary site storage are withheld. Non-default container/partition state is refused, never flattened.".to_owned()] })
    }
    fn capture_selected(
        &self,
        app: &AppRef,
        scope: TransferScope,
        include: Option<&HashSet<String>>,
        out: &mut dyn Write,
    ) -> Result<()> {
        let manifest = self.manifest(app, None, scope)?;
        if let Some(include) = include {
            for key in include {
                if !manifest.items.iter().any(|i| &i.rel_path == key) {
                    return Err(fail(
                        "zen_selection_invalid",
                        "selection contains an unavailable or unsupported Zen item",
                    ));
                }
            }
        }
        // No implicit sensitive capture. Even full scope with no selection only
        // delivers tabs; saved passwords require this exact key in a named set.
        let wants = |key: &str| include.map_or(key == TABS_JSON, |s| s.contains(key));
        let p = self.resolve_profile(app.platform)?;
        p.ensure_closed()?;
        let mut entries: Vec<(&str, zeroize::Zeroizing<Vec<u8>>)> = Vec::new();
        if wants(TABS_JSON) {
            entries.push((
                TABS_JSON,
                zeroize::Zeroizing::new(serde_json::to_vec(&crate::gecko_store::read_tabs(&p)?)?),
            ));
        }
        if wants(COOKIES) {
            entries.push((
                cua_teleport_bundle::cookies::COOKIES_ENTRY,
                zeroize::Zeroizing::new(serde_json::to_vec(&crate::gecko_store::read_cookies(
                    &p,
                )?)?),
            ));
        }
        if wants(STORAGE) {
            entries.push((
                cua_teleport_bundle::local_storage::LOCAL_STORAGE_ENTRY,
                zeroize::Zeroizing::new(serde_json::to_vec(
                    &crate::gecko_store::read_local_storage(&p)?,
                )?),
            ));
        }
        if wants(LOGINS) {
            entries.push((
                cua_teleport_bundle::logins::LOGINS_ENTRY,
                zeroize::Zeroizing::new(serde_json::to_vec(
                    &crate::gecko_store::read_logins(&p)?.logins,
                )?),
            ));
        }
        p.ensure_closed()?;
        // Parse and validate everything before emitting the first bundle byte.
        let mut writer =
            crate::bundle::BundleWriter::with_limit(out, ID, DISPLAY, scope, self.max_total_bytes);
        for (name, bytes) in entries {
            writer.add_bytes(name, 0o600, &bytes)?;
        }
        writer.finish()?;
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "zen_tests.rs"]
mod tests;
