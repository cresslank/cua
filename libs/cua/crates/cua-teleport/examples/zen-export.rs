// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.
//! Opt-in format-boundary qualification of a closed, synthetic Zen profile.
//! Uses the real provider's raw public capture seam, NOT export authorization,
//! Keyvault, polkit, CLI selection, or production source-site filtering.
use cua_teleport::bundle::{BundleReader, BundleWriter};
use cua_teleport::providers::zen::{COOKIES, LOGINS, STORAGE, TABS_JSON, ZenProvider};
use cua_teleport::{
    AppRef, AuthError, ExportProvider, HostCommand, HostEffects, HostOutput, Platform,
    TransferScope,
};
use cua_teleport_bundle::{cookies::CookieItem, local_storage::LocalStorageItem};
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

struct FixtureHost(PathBuf);
impl HostEffects for FixtureHost {
    fn home_dir(&self) -> Option<PathBuf> {
        Some(self.0.clone())
    }
    fn run(&self, _: &HostCommand) -> io::Result<HostOutput> {
        Err(io::Error::other("fixture forbids host effects"))
    }
    fn authorize_sensitive_export(&self, _: &str) -> Result<(), AuthError> {
        Err(AuthError::Denied)
    }
    fn devtools_page_urls(&self, _: &str) -> io::Result<Vec<String>> {
        Err(io::Error::other("fixture forbids DevTools"))
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "linux")]
    // SAFETY: process-local permissions, set before any output or worker exists.
    unsafe {
        libc::umask(0o077);
    }
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 || args[1] != "--allow-synthetic-capture" {
        return Err("usage: zen-export --allow-synthetic-capture EXPLICIT_FIXTURE_HOME PROFILE NEW_OUTPUT_DIR".into());
    }
    let home = PathBuf::from(&args[2]).canonicalize()?;
    if !home.join(".cua-synthetic-zen-fixture").is_file() {
        return Err("missing synthetic fixture marker".into());
    }
    let profile = PathBuf::from(&args[3]).canonicalize()?;
    if !profile.starts_with(home.join(".zen/Profiles")) {
        return Err("profile is not under explicit synthetic Zen home".into());
    }
    let out = PathBuf::from(&args[4]);
    std::fs::create_dir(&out)?;
    let provider = ZenProvider::new()
        .with_host(Arc::new(FixtureHost(home)))
        .with_profile(profile);
    let app = AppRef {
        app_id: "zen".into(),
        display_name: "Zen".into(),
        platform: Platform::Linux,
    };
    let manifest = provider.manifest(&app, None, TransferScope::FullProfile)?;
    std::fs::write(
        out.join("source-manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let selected: HashSet<String> = [TABS_JSON, COOKIES, STORAGE]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut full = Vec::new();
    provider.capture_selected(&app, TransferScope::FullProfile, Some(&selected), &mut full)?;
    std::fs::write(out.join("full.tar"), &full)?;
    let mut reader = BundleReader::open(full.as_slice())?;
    let mut tabs = Vec::<String>::new();
    let mut payloads = Vec::new();
    while let Some(e) = reader.next_entry()? {
        if e.rel_path == TABS_JSON {
            tabs = serde_json::from_slice(&e.bytes)?;
        }
        payloads.push(e);
    }
    if tabs.len() != 2 {
        return Err("fixture must have exactly two real HTTP account tabs".into());
    }
    let first = url::Url::parse(&tabs[0])?;
    let first_origin = first.origin().ascii_serialization();
    let first_host = first.host_str().ok_or("fixture tab has no host")?;
    let mut filtered = Vec::new();
    let mut writer = BundleWriter::new(&mut filtered, "zen", "Zen", TransferScope::FullProfile);
    let mut counts = serde_json::Map::new();
    for e in payloads {
        let bytes = match e.rel_path.as_str() {
            "cookies.json" => {
                let all: Vec<CookieItem> = serde_json::from_slice(&e.bytes)?;
                let kept: Vec<_> = all.iter().filter(|c| c.host_key == first_host).collect();
                if all.len() != 2 || kept.len() != 1 {
                    return Err("fixture cookie host cardinality".into());
                }
                counts.insert(
                    "cookies".into(),
                    serde_json::json!({"captured": all.len(), "selected":kept.len()}),
                );
                serde_json::to_vec(&kept)?
            }
            "localstorage.json" => {
                let all: Vec<LocalStorageItem> = serde_json::from_slice(&e.bytes)?;
                let kept: Vec<_> = all.iter().filter(|s| s.origin == first_origin).collect();
                if all.len() != 2 || kept.len() != 1 {
                    return Err("fixture storage origin cardinality".into());
                }
                counts.insert(
                    "localstorage".into(),
                    serde_json::json!({"captured": all.len(), "selected":kept.len()}),
                );
                serde_json::to_vec(&kept)?
            }
            TABS_JSON => e.bytes,
            _ => return Err("unexpected provider payload".into()),
        };
        writer.add_bytes(&e.rel_path, e.mode, &bytes)?;
    }
    writer.finish()?;
    std::fs::write(out.join("selected-first.tar"), filtered)?;
    for (name, scope) in [
        ("tabs-only", TransferScope::TabsOnly),
        ("defaults", TransferScope::FullProfile),
    ] {
        let mut bytes = Vec::new();
        provider.capture_selected(&app, scope, None, &mut bytes)?;
        std::fs::write(out.join(format!("{name}.tar")), bytes)?;
    }
    // Independent diagnostic: never replace a failing NSS decode with fixture
    // plaintext or a synthetic LoginItem. Only a real capture may be imported.
    let logins: HashSet<String> = [LOGINS.to_owned()].into_iter().collect();
    let mut bytes = Vec::new();
    let login_result = match provider.capture_selected(
        &app,
        TransferScope::FullProfile,
        Some(&logins),
        &mut bytes,
    ) {
        Ok(()) => {
            std::fs::write(out.join("logins.tar"), &bytes)?;
            serde_json::json!({"captured":true})
        }
        Err(e) => {
            if !bytes.is_empty() {
                return Err("failed login capture emitted partial bundle".into());
            }
            serde_json::json!({"captured":false,"error":e.to_string()})
        }
    };
    std::fs::write(
        out.join("saved-login-capture.json"),
        serde_json::to_vec_pretty(&login_result)?,
    )?;
    println!(
        "{}",
        serde_json::json!({"boundary":"real ZenProvider capture_selected; NO export authorization, Keyvault or polkit approval", "provider":"zen", "tabs":tabs,"selected_first_origin":first_origin,"site_filter_boundary":"typed filtering of real captured records; NOT provider/broker source-site consent", "counts":counts,"saved_logins":login_result})
    );
    Ok(())
}
