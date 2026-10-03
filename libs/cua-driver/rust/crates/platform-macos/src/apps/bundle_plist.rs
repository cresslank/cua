//! One non-localized Info.plist snapshot, shared by metadata and background filtering.
//! Synchronous disk I/O: callers must stay on the scan's blocking worker.

use core_foundation::{
    array::CFArray,
    base::{CFType, TCFType},
    boolean::CFBoolean,
    data::CFData,
    date::CFDate,
    dictionary::CFDictionary,
    number::{CFNumber, CFNumberIsFloatType},
    propertylist::{create_with_data, kCFPropertyListImmutable},
    string::CFString,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

type RawValue = Result<Option<String>, ()>;

pub(super) struct BundlePlist {
    pub bundle_id: RawValue,
    pub display_name: RawValue,
    pub name: RawValue,
    ui_element: RawValue,
    background_only: RawValue,
}

/// Per-call snapshots, including failed reads. A cold installed scan can reuse
/// the running scan's plist without retaining running metadata across calls.
#[derive(Default)]
pub(super) struct BundlePlists {
    entries: HashMap<PathBuf, Result<BundlePlist, ()>>,
}

impl BundlePlists {
    pub fn read(&mut self, path: &Path) -> Result<&BundlePlist, ()> {
        self.entries
            .entry(path.to_owned())
            .or_insert_with(|| BundlePlist::read(path))
            .as_ref()
            .map_err(|_| ())
    }
}

impl BundlePlist {
    pub fn read(path: &Path) -> Result<Self, ()> {
        let bytes = std::fs::read(path).map_err(|_| ())?;
        // create_with_data calls CFPropertyListCreateWithData and accepts both
        // XML and binary plists. Own its +1 reference even for a non-dictionary.
        let (raw, _) = create_with_data(CFData::from_buffer(&bytes), kCFPropertyListImmutable)
            .map_err(|_| ())?;
        let plist = unsafe { CFType::wrap_under_create_rule(raw) };
        let dictionary = plist.downcast::<CFDictionary>().ok_or(())?;
        let read = |key: &str| -> RawValue {
            let key = CFString::new(key);
            let Some(value) = dictionary.find(key.as_CFTypeRef()) else {
                return Ok(None);
            };
            // All values in a parsed property-list dictionary are CF objects.
            let value = unsafe { CFType::wrap_under_get_rule(*value) };
            let raw = raw_value(&value)?;
            let raw = raw.trim();
            Ok((!raw.is_empty()).then(|| raw.to_owned()))
        };
        Ok(Self {
            bundle_id: read("CFBundleIdentifier"),
            display_name: read("CFBundleDisplayName"),
            name: read("CFBundleName"),
            ui_element: read("LSUIElement"),
            background_only: read("LSBackgroundOnly"),
        })
    }

    pub fn is_background(&self) -> bool {
        [&self.ui_element, &self.background_only]
            .into_iter()
            .any(|value| matches!(value, Ok(Some(raw)) if raw == "1" || raw.eq_ignore_ascii_case("true")))
    }
}

/// Match `plutil -extract KEY raw`, including its scalar rendering for unusual
/// key types. In particular, boolean true renders as "true", integer 1 as "1",
/// and real 1.0 as "1.000000"; only the first two imply background operation.
fn raw_value(value: &CFType) -> Result<String, ()> {
    if let Some(value) = value.downcast::<CFString>() {
        return Ok(value.to_string());
    }
    if let Some(value) = value.downcast::<CFBoolean>() {
        return Ok(bool::from(value).to_string());
    }
    if let Some(value) = value.downcast::<CFNumber>() {
        return if unsafe { CFNumberIsFloatType(value.as_concrete_TypeRef()) } != 0 {
            value
                .to_f64()
                .map(|n| format!("{n:.6}").to_ascii_lowercase())
        } else {
            value.to_i64().map(|n| n.to_string())
        }
        .ok_or(());
    }
    if let Some(value) = value.downcast::<CFData>() {
        use base64::Engine;
        return Ok(base64::engine::general_purpose::STANDARD.encode(value.bytes()));
    }
    if let Some(value) = value.downcast::<CFDate>() {
        // CF absolute time uses 2001-01-01; plutil emits UTC whole seconds.
        return cua_driver_core::timestamp::unix_secs_to_rfc3339(
            (value.abs_time() + 978_307_200.0).floor() as i64,
        )
        .ok_or(());
    }
    if let Some(value) = value.downcast::<CFArray>() {
        return Ok(value.len().to_string());
    }
    if let Some(value) = value.downcast::<CFDictionary>() {
        let (keys, _) = value.get_keys_and_values();
        let mut keys = keys
            .into_iter()
            .map(|key| {
                // Property list dictionary keys are strings.
                unsafe { CFString::wrap_under_get_rule(key.cast()) }.to_string()
            })
            .collect::<Vec<_>>();
        keys.sort();
        return Ok(keys.join("\n"));
    }
    Err(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_call_snapshot_reuses_success_and_failure_but_next_call_is_live() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Info.plist");
        let mut first_call = BundlePlists::default();
        assert!(matches!(first_call.read(&path), Err(())));
        std::fs::write(
            &path,
            "<plist version=\"1.0\"><dict><key>CFBundleIdentifier</key><string>test.first</string></dict></plist>",
        )
        .unwrap();
        assert!(matches!(first_call.read(&path), Err(())));
        let mut second_call = BundlePlists::default();
        assert_eq!(
            second_call
                .read(&path)
                .unwrap()
                .bundle_id
                .as_ref()
                .unwrap()
                .as_deref(),
            Some("test.first")
        );
        std::fs::remove_file(&path).unwrap();
        // Running and installed metadata can reuse the snapshot even after
        // the source disappears: no second disk read occurs within this call.
        assert_eq!(
            second_call
                .read(&path)
                .unwrap()
                .bundle_id
                .as_ref()
                .unwrap()
                .as_deref(),
            Some("test.first")
        );
        assert!(matches!(BundlePlists::default().read(&path), Err(())));
    }
}
