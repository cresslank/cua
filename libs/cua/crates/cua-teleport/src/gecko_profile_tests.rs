// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

use super::*;
use crate::FakeHost;
use std::fs;
use std::os::unix::fs::symlink;

fn fixture() -> (tempfile::TempDir, FakeHost, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join(".zen");
    fs::create_dir_all(root.join("Profiles/synthetic")).unwrap();
    fs::write(
        root.join("profiles.ini"),
        "[Profile0]\nName=synthetic\nIsRelative=1\nPath=Profiles/synthetic\nDefault=1\n",
    )
    .unwrap();
    let host = FakeHost::new().with_home(temp.path());
    (temp, host, root)
}
fn code<T: std::fmt::Debug>(r: Result<T>, expected: &str) {
    match r.unwrap_err() {
        TeleportError::Provider(s) => assert_eq!(s.split(':').next().unwrap(), expected),
        e => panic!("wrong error: {e}"),
    }
}
#[test]
fn native_and_flatpak_inventory_share_exact_resolution() {
    let (temp, host, root) = fixture();
    let p = resolve_profile(&host, Platform::Linux, None).unwrap();
    assert_eq!(p.provider_id, "zen");
    assert_eq!(p.root, root);
    assert_eq!(p.profile_id, "Profiles/synthetic");
    assert_eq!(profiles(&host, Platform::Linux).unwrap()[0].dir, p.dir);
    let flat = temp.path().join(".var/app/app.zen_browser.zen/.zen");
    fs::create_dir_all(flat.parent().unwrap()).unwrap();
    fs::rename(&root, &flat).unwrap();
    let q = resolve_profile(&host, Platform::Linux, None).unwrap();
    assert_eq!(q.root, flat);
    assert_eq!(q.profile_id, p.profile_id);
}
#[test]
fn only_documented_linux_roots_are_used() {
    let temp = tempfile::tempdir().unwrap();
    let host = FakeHost::new().with_home(temp.path());
    fs::create_dir_all(temp.path().join(".mozilla/firefox/Profiles/source")).unwrap();
    assert!(profiles(&host, Platform::Linux).unwrap().is_empty());
    code(
        resolve_profile(&host, Platform::Linux, None),
        "zen_profile_not_found",
    );
    code(
        resolve_profile(&host, Platform::MacOS, None),
        "zen_platform_unsupported",
    );
}
#[test]
fn competing_roots_and_duplicate_names_require_explicit_paths() {
    let (temp, host, root) = fixture();
    let flat = temp.path().join(".var/app/app.zen_browser.zen/.zen");
    fs::create_dir_all(flat.join("Profiles/synthetic")).unwrap();
    fs::copy(root.join("profiles.ini"), flat.join("profiles.ini")).unwrap();
    code(
        resolve_profile(&host, Platform::Linux, None),
        "zen_profile_ambiguous",
    );
    code(
        resolve_profile(&host, Platform::Linux, Some(Path::new("synthetic"))),
        "zen_profile_ambiguous",
    );
    assert_eq!(
        resolve_profile(
            &host,
            Platform::Linux,
            Some(&flat.join("Profiles/synthetic"))
        )
        .unwrap()
        .root,
        flat
    );
}
#[test]
fn missing_and_competing_defaults_do_not_choose_first() {
    let (_temp, host, root) = fixture();
    fs::create_dir_all(root.join("Profiles/second")).unwrap();
    fs::write(root.join("profiles.ini"), "[Profile0]\nName=synthetic\nPath=Profiles/synthetic\n[Profile1]\nName=second\nPath=Profiles/second\n").unwrap();
    code(
        resolve_profile(&host, Platform::Linux, None),
        "zen_profile_ambiguous",
    );
    assert_eq!(
        resolve_profile(&host, Platform::Linux, Some(Path::new("second")))
            .unwrap()
            .profile_id,
        "Profiles/second"
    );
    fs::write(
        root.join("installs.ini"),
        "[HASH1]\nDefault=Profiles/synthetic\n[HASH2]\nDefault=Profiles/second\n",
    )
    .unwrap();
    code(
        resolve_profile(&host, Platform::Linux, None),
        "zen_profile_ambiguous",
    );
    fs::write(
        root.join("installs.ini"),
        "[HASH1]\nDefault=Profiles/second\n",
    )
    .unwrap();
    assert_eq!(
        resolve_profile(&host, Platform::Linux, None).unwrap().name,
        "second"
    );
}
#[test]
fn absolute_containment_and_relative_escape_are_checked_before_lookup() {
    let (temp, host, root) = fixture();
    let good = root.join("Profiles/synthetic");
    fs::write(
        root.join("profiles.ini"),
        format!(
            "[Profile0]\nName=synthetic\nIsRelative=0\nPath={}\n",
            good.display()
        ),
    )
    .unwrap();
    assert_eq!(
        resolve_profile(&host, Platform::Linux, None).unwrap().dir,
        good
    );
    for raw in [
        "../.mozilla/firefox",
        "Profiles/../../outside",
        "/etc",
        "/home/other/.zen/Profiles/secret",
    ] {
        fs::write(
            root.join("profiles.ini"),
            format!("[Profile0]\nName=synthetic\nIsRelative=1\nPath={raw}\n"),
        )
        .unwrap();
        code(
            resolve_profile(&host, Platform::Linux, None),
            "zen_profile_unsafe",
        );
    }
    fs::write(
        root.join("profiles.ini"),
        format!(
            "[Profile0]\nIsRelative=0\nPath={}\n",
            temp.path().join(".mozilla/firefox").display()
        ),
    )
    .unwrap();
    code(profiles(&host, Platform::Linux), "zen_profile_unsafe");
}
#[test]
fn install_defaults_never_escape_or_name_another_browser() {
    let (_temp, host, root) = fixture();
    for raw in ["../../.mozilla/firefox/x", "/etc", "Profiles/unregistered"] {
        fs::write(
            root.join("installs.ini"),
            format!("[HASH]\nDefault={raw}\n"),
        )
        .unwrap();
        code(profiles(&host, Platform::Linux), "zen_profile_unsafe");
    }
}
#[test]
fn symlink_root_ini_profile_and_ancestor_are_refused() {
    let (temp, host, root) = fixture();
    let real = temp.path().join("real");
    fs::rename(&root, &real).unwrap();
    symlink(&real, &root).unwrap();
    code(profiles(&host, Platform::Linux), "zen_profile_unsafe");
    fs::remove_file(&root).unwrap();
    fs::rename(&real, &root).unwrap();
    fs::rename(root.join("profiles.ini"), root.join("real.ini")).unwrap();
    symlink("real.ini", root.join("profiles.ini")).unwrap();
    code(profiles(&host, Platform::Linux), "zen_profile_unsafe");
    fs::remove_file(root.join("profiles.ini")).unwrap();
    fs::rename(root.join("real.ini"), root.join("profiles.ini")).unwrap();
    fs::rename(root.join("Profiles/synthetic"), root.join("Profiles/real")).unwrap();
    symlink("real", root.join("Profiles/synthetic")).unwrap();
    code(profiles(&host, Platform::Linux), "zen_profile_unsafe");
    fs::remove_file(root.join("Profiles/synthetic")).unwrap();
    fs::rename(root.join("Profiles/real"), root.join("Profiles/synthetic")).unwrap();
    fs::rename(root.join("Profiles"), root.join("actual")).unwrap();
    symlink("actual", root.join("Profiles")).unwrap();
    code(profiles(&host, Platform::Linux), "zen_profile_unsafe");
}
#[test]
fn retained_profile_and_root_refuse_rename_replacement() {
    let (temp, host, root) = fixture();
    let p = resolve_profile(&host, Platform::Linux, None).unwrap();
    fs::rename(&p.dir, root.join("Profiles/old")).unwrap();
    fs::create_dir_all(&p.dir).unwrap();
    code(p.ensure_closed(), "zen_profile_unsafe");
    let q = resolve_profile(&host, Platform::Linux, None).unwrap();
    fs::rename(&root, temp.path().join("old-root")).unwrap();
    fs::create_dir_all(root.join("Profiles/synthetic")).unwrap();
    code(q.ensure_closed(), "zen_profile_unsafe");
}
#[test]
fn locks_including_stale_symlinks_are_refused_without_removal() {
    let (_temp, host, _root) = fixture();
    let p = resolve_profile(&host, Platform::Linux, None).unwrap();
    for lock in ["parent.lock", ".parentlock", "lock"] {
        symlink("127.0.0.1:+99999999", p.dir.join(lock)).unwrap();
        assert_eq!(profiles(&host, Platform::Linux).unwrap().len(), 1);
        code(p.ensure_closed(), "zen_profile_locked");
        assert!(
            fs::symlink_metadata(p.dir.join(lock))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_file(p.dir.join(lock)).unwrap();
    }
    p.ensure_closed().unwrap();
}
#[test]
fn explicit_unregistered_path_is_allowed_only_beneath_zen_root() {
    let (temp, host, root) = fixture();
    fs::create_dir_all(root.join("Profiles/explicit-fixture")).unwrap();
    assert_eq!(
        resolve_profile(
            &host,
            Platform::Linux,
            Some(&root.join("Profiles/explicit-fixture"))
        )
        .unwrap()
        .profile_id,
        "Profiles/explicit-fixture"
    );
    code(
        resolve_profile(&host, Platform::Linux, Some(&temp.path().join("other"))),
        "zen_profile_unsafe",
    );
    code(
        resolve_profile(&host, Platform::Linux, Some(Path::new("../synthetic"))),
        "zen_profile_unsafe",
    );
}
#[test]
fn duplicate_ini_keys_and_sections_refuse() {
    let (_temp, host, root) = fixture();
    for text in [
        "[Profile0]\nPath=Profiles/synthetic\nPath=Profiles/synthetic\n",
        "[Profile0]\nPath=Profiles/synthetic\n[Profile0]\nPath=Profiles/synthetic\n",
    ] {
        fs::write(root.join("profiles.ini"), text).unwrap();
        code(profiles(&host, Platform::Linux), "zen_profile_ambiguous");
    }
}
#[test]
fn retained_fd_never_follows_replaced_source_symlink() {
    let (_temp, host, root) = fixture();
    let p = resolve_profile(&host, Platform::Linux, None).unwrap();
    fs::write(root.join("outside.json"), "not a session").unwrap();
    symlink(
        root.join("outside.json"),
        p.dir.join("sessionstore.jsonlz4"),
    )
    .unwrap();
    code(
        p.read(Path::new("sessionstore.jsonlz4"), 1024),
        "zen_profile_unsafe",
    );
}
