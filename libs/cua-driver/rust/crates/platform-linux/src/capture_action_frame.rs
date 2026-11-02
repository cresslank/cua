use cua_driver_core::capture_runtime::{
    CaptureActionAdmission, CaptureActionRequest, CaptureFrameGeometry, CapturePublication,
    CaptureService, CaptureTarget, EncodedScreenshotDimensions, NativeActionDimensions,
    ScreenshotToActionTransform,
};
use serde_json::Value;

fn window_target(pid: u32, window_id: u64) -> CaptureTarget {
    CaptureTarget::Window { pid, window_id }
}

fn publish(
    service: &CaptureService,
    args: &Value,
    png_bytes: &[u8],
    target: CaptureTarget,
    encoded_dimensions: (u32, u32),
    native_action_dimensions: (u32, u32),
    screenshot_to_action: ScreenshotToActionTransform,
    snapshot_identity: Option<cua_driver_core::element_token::SnapshotIdentity>,
    frame: Option<cua_driver_core::snapshot_store::ScreenshotFrameBinding>,
) -> anyhow::Result<String> {
    let binding = service.binding_from_args(args)?;
    let publication = CapturePublication {
        png_bytes: png_bytes.to_vec(),
        target,
        encoded_dimensions: EncodedScreenshotDimensions::new(
            encoded_dimensions.0,
            encoded_dimensions.1,
        )?,
        native_action_dimensions: NativeActionDimensions::new(
            native_action_dimensions.0,
            native_action_dimensions.1,
        )?,
        screenshot_to_action,
        session_id: binding.session_id().into(),
        session_generation: binding.session_generation(),
    };
    let capture_id = match (snapshot_identity, frame) {
        (Some(identity), Some(frame)) => service.publish_for_frame(publication, identity, frame),
        (Some(identity), None) => service.publish_for_snapshot(publication, identity),
        (None, _) => service.publish(publication),
    }?;
    Ok(capture_id.to_string())
}

pub fn publish_window(
    service: &CaptureService,
    args: &Value,
    png_bytes: &[u8],
    pid: u32,
    window_id: u64,
    encoded_dimensions: (u32, u32),
    native_action_dimensions: (u32, u32),
    snapshot_identity: cua_driver_core::element_token::SnapshotIdentity,
    frame: Option<cua_driver_core::snapshot_store::ScreenshotFrameBinding>,
) -> anyhow::Result<String> {
    publish(
        service,
        args,
        png_bytes,
        window_target(pid, window_id),
        encoded_dimensions,
        native_action_dimensions,
        scaled_transform(encoded_dimensions, native_action_dimensions)?,
        Some(snapshot_identity),
        frame,
    )
}

pub fn publish_desktop(
    service: &CaptureService,
    args: &Value,
    png_bytes: &[u8],
    encoded_dimensions: (u32, u32),
    native_action_dimensions: (u32, u32),
) -> anyhow::Result<String> {
    // The desktop screenshot can be downsized below the action frame.
    publish(
        service,
        args,
        png_bytes,
        CaptureTarget::PrimaryDesktop,
        encoded_dimensions,
        native_action_dimensions,
        scaled_transform(encoded_dimensions, native_action_dimensions)?,
        None,
        None,
    )
}

/// Screenshot-to-action scaling for a capture encoded at `encoded` pixels of
/// a `native` action frame. The resizer preserves aspect ratio before rounding
/// each encoded axis, so both ratios are derived independently: a one-pixel
/// rounded height must not skew Y coordinates.
fn scaled_transform(
    encoded: (u32, u32),
    native: (u32, u32),
) -> anyhow::Result<ScreenshotToActionTransform> {
    anyhow::ensure!(
        encoded.0 > 0 && encoded.1 > 0,
        "capture has an empty encoded frame: {}x{}",
        encoded.0,
        encoded.1
    );
    let scale_x = f64::from(native.0) / f64::from(encoded.0);
    let scale_y = f64::from(native.1) / f64::from(encoded.1);
    Ok(ScreenshotToActionTransform::new(
        scale_x, 0.0, 0.0, scale_y, 0.0, 0.0,
    )?)
}

#[cfg(test)]
fn admit_with_live_dimensions(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    target: CaptureTarget,
    current_native_action_dimensions: NativeActionDimensions,
    screenshot_x: f64,
    screenshot_y: f64,
) -> anyhow::Result<CaptureActionAdmission> {
    let binding = service.binding_from_args(args)?;
    let capture_id = service.parse_capture_id(capture_id)?;
    let admission = service.admit_action(CaptureActionRequest {
        capture_id,
        binding,
        target,
        current_native_action_dimensions,
        screenshot_x,
        screenshot_y,
    })?;
    Ok(admission)
}

#[cfg(target_os = "linux")]
pub(crate) fn desktop_action_dimensions(native: (u32, u32)) -> anyhow::Result<(u32, u32)> {
    let logical = if crate::wayland::is_wayland() {
        crate::wayland::compositor_logical_frame().transpose()?
    } else {
        None
    };
    select_desktop_action_dimensions(native, logical)
}

fn select_desktop_action_dimensions(
    native: (u32, u32),
    compositor_logical: Option<(u32, u32)>,
) -> anyhow::Result<(u32, u32)> {
    let dimensions = compositor_logical.unwrap_or(native);
    anyhow::ensure!(
        dimensions.0 > 0 && dimensions.1 > 0,
        "desktop action frame is empty: {}x{}",
        dimensions.0,
        dimensions.1
    );
    Ok(dimensions)
}

/// Publish only geometry obtained with the pixels, never discovery metadata.
pub(crate) fn publish_qualified_window(
    service: &CaptureService,
    args: &Value,
    png: &[u8],
    pid: u32,
    window_id: u64,
    encoded: (u32, u32),
    geometry: CaptureFrameGeometry,
    identity: cua_driver_core::element_token::SnapshotIdentity,
    frame: cua_driver_core::snapshot_store::ScreenshotFrameBinding,
) -> anyhow::Result<String> {
    let binding = service.binding_from_args(args)?;
    let native = (geometry.width(), geometry.height());
    Ok(service
        .publish_for_frame_with_geometry(
            CapturePublication {
                png_bytes: png.to_vec(),
                target: window_target(pid, window_id),
                encoded_dimensions: EncodedScreenshotDimensions::new(encoded.0, encoded.1)?,
                native_action_dimensions: NativeActionDimensions::new(native.0, native.1)?,
                screenshot_to_action: scaled_transform(encoded, native)?,
                session_id: binding.session_id().into(),
                session_generation: binding.session_generation(),
            },
            identity,
            frame,
            geometry,
        )?
        .to_string())
}

pub(crate) fn unqualified() -> cua_driver_core::protocol::ToolResult {
    cua_driver_core::protocol::ToolResult::error(
        "capture-bound clicks require a composited X11 drawable or GNOME WinRects window capture; non-composited X11, desktop, composited popup and other compositor captures remain unqualified",
    ).with_structured(serde_json::json!({"code":"capture_route_unqualified", "effect":"refused"}))
}

/// Capture-bound clicks have one unambiguous source coordinate frame.
pub(crate) fn validate_click(args: &Value) -> Result<(), cua_driver_core::protocol::ToolResult> {
    if args.get("capture_id").is_none() {
        return Ok(());
    }
    if !args.get("capture_id").is_some_and(Value::is_string) {
        return Err(
            cua_driver_core::protocol::ToolResult::error("capture_id must be a string")
                .with_structured(
                    serde_json::json!({"code":"capture_id_invalid", "effect":"refused"}),
                ),
        );
    }
    if args.get("scope").and_then(Value::as_str) == Some("desktop")
        || args.get("coordinate_frame").and_then(Value::as_str) == Some("desktop")
    {
        return Err(unqualified());
    }
    if args.get("element_token").is_some()
        || args.get("element_index").is_some()
        || args
            .get("from_zoom")
            .is_some_and(|v| v != &Value::Bool(false))
        || !args
            .get("pid")
            .and_then(Value::as_u64)
            .is_some_and(|v| v > 0 && v <= u32::MAX.into())
        || !args
            .get("window_id")
            .and_then(Value::as_u64)
            .is_some_and(|v| v > 0)
        || args.get("x").and_then(Value::as_f64).is_none()
        || args.get("y").and_then(Value::as_f64).is_none()
    {
        return Err(cua_driver_core::protocol::ToolResult::error(
            "capture-bound click requires explicit pid, window_id and screenshot x/y; element and zoom targets cannot be combined with capture_id",
        ).with_structured(serde_json::json!({"code":"invalid_arguments", "effect":"refused"})));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn qualified_backend() -> bool {
    crate::wayland::is_gnome_wayland_session()
        || (!crate::wayland::is_wayland() && !crate::capture_safety::gnome_session())
}

#[cfg(target_os = "linux")]
pub(crate) fn fingerprint_frame(
    png: &[u8],
    geometry: CaptureFrameGeometry,
) -> anyhow::Result<CaptureFrameGeometry> {
    use sha2::{Digest, Sha256};
    let rgba = image::load_from_memory(png)?.to_rgba8();
    Ok(geometry.with_pixel_digest(Sha256::digest(rgba.as_raw()).into()))
}

#[cfg(target_os = "linux")]
pub(crate) fn capture_window(
    pid: u32,
    window_id: u64,
) -> anyhow::Result<(Vec<u8>, CaptureFrameGeometry)> {
    if crate::wayland::is_gnome_wayland_session() {
        return crate::wayland::shell_helper::screenshot_window_frame(window_id, Some(pid));
    }
    anyhow::ensure!(qualified_backend(), "capture_route_unqualified");
    let (captured, exclusion) = cursor_overlay::capture_exclusion::capture_excluding_overlays(
        &crate::overlay_capture::OverlayExcluder,
        |hidden| -> anyhow::Result<_> {
            let before = crate::x11::capture_frame_geometry(window_id, pid)?;
            let png = crate::capture::screenshot_window_bytes(window_id)?;
            let after = crate::x11::capture_frame_geometry(window_id, pid)?;
            if before != after
                || crate::capture::png_dimensions_pub(&png)?
                    != (after.pixel_width(), after.pixel_height())
            {
                return Err(
                    cua_driver_core::capture_runtime::CaptureActionError::NativeActionFrameMismatch
                        .into(),
                );
            }
            // Residual patches use root coordinates; this PNG uses the exact
            // client drawable. Translate before inspecting, never stage-crop.
            let translated = hidden.cloned().map(|mut hidden| {
                for patch in &mut hidden.residual {
                    patch.x = patch.x.saturating_sub(after.origin_x());
                    patch.y = patch.y.saturating_sub(after.origin_y());
                }
                hidden
            });
            let (png, residual) =
                crate::overlay_capture::verify_hidden_capture(png, translated.as_ref());
            // Save-under repair is adequate for observation, not evidence that
            // the app's current pixels are unchanged. Require a clean repaint.
            if !matches!(
                &residual,
                cursor_overlay::capture_exclusion::ResidualCheck::Clean
            ) {
                return Err(
                    cua_driver_core::capture_runtime::CaptureActionError::NativeActionFrameMismatch
                        .into(),
                );
            }
            let proof = fingerprint_frame(&png, after)?;
            Ok(((png, proof), residual))
        },
    )?;
    if !exclusion.is_clean() {
        return Err(
            cua_driver_core::capture_runtime::CaptureActionError::NativeActionFrameMismatch.into(),
        );
    }
    Ok(captured)
}

/// Quantize once in action units. Never truncate a half-pixel into a different
/// recipient, and never pass Wayland's legacy (0,0) no-point sentinel.
pub(crate) fn rounded_point(
    x: f64,
    y: f64,
    geometry: CaptureFrameGeometry,
    wayland: bool,
) -> anyhow::Result<(i32, i32)> {
    let (x, y) = (x.round(), y.round());
    if !x.is_finite()
        || !y.is_finite()
        || x < 0.0
        || y < 0.0
        || x >= f64::from(geometry.width())
        || y >= f64::from(geometry.height())
        || (wayland
            && x + f64::from(geometry.origin_x()) == 0.0
            && y + f64::from(geometry.origin_y()) == 0.0)
    {
        return Err(
            cua_driver_core::capture_runtime::CaptureActionError::InvalidScreenshotPoint.into(),
        );
    }
    Ok((x as i32, y as i32))
}

/// Revalidate inside the native worker, after any asynchronous cursor glide.
/// Detached work retains the admission (and therefore its generation permit).
#[cfg(target_os = "linux")]
pub(crate) fn revalidate_window(
    pid: u32,
    window_id: u64,
    admission: &CaptureActionAdmission,
) -> anyhow::Result<()> {
    admission.validate_frame()?;
    let expected = admission
        .frame_geometry
        .ok_or_else(|| anyhow::anyhow!("capture_route_unqualified"))?;
    let live = capture_window(pid, window_id)
        .map(|(_, geometry)| geometry)
        .map_err(|error| {
            if error.to_string() == "capture_route_unqualified" {
                error
            } else {
                cua_driver_core::capture_runtime::CaptureActionError::NativeActionFrameMismatch
                    .into()
            }
        })?;
    if live != expected {
        return Err(
            cua_driver_core::capture_runtime::CaptureActionError::NativeActionFrameMismatch.into(),
        );
    }
    Ok(())
}

/// Check the screen recipient separately from drawable freshness. Hide only
/// Driver-owned overlays, never application popups, while querying the root
/// child. This gate is shared by foreground, MPX and synthetic capture clicks.
#[cfg(target_os = "linux")]
pub(crate) fn revalidate_x11_point(window_id: u64, x: i32, y: i32) -> anyhow::Result<()> {
    let result = cursor_overlay::capture_exclusion::capture_excluding_overlays(
        &crate::overlay_capture::OverlayExcluder,
        |_| -> anyhow::Result<_> {
            crate::x11::require_capture_click_target(window_id, x, y)?;
            Ok(((), cursor_overlay::capture_exclusion::ResidualCheck::Clean))
        },
    );
    match result {
        Ok(((), exclusion)) if exclusion.is_clean() => Ok(()),
        _ => Err(
            cua_driver_core::capture_runtime::CaptureActionError::NativeActionFrameMismatch.into(),
        ),
    }
}

#[cfg(target_os = "linux")]
struct WindowCaptureMetadata {
    id: cua_driver_core::capture_runtime::CaptureId,
    binding: cua_driver_core::capture_runtime::CaptureBinding,
    frame: CaptureFrameGeometry,
    encoded: EncodedScreenshotDimensions,
}

#[cfg(target_os = "linux")]
fn window_capture_metadata(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    pid: u32,
    window_id: u64,
) -> anyhow::Result<WindowCaptureMetadata> {
    use cua_driver_core::capture_runtime::{CaptureActionError, CaptureLookupError};
    let id = service.parse_capture_id(capture_id)?;
    let binding = service.binding_from_args(args)?;
    let capture = service
        .read_for_perception(id, &binding)
        .map_err(CaptureActionError::Lookup)?;
    if capture.target() != &window_target(pid, window_id) {
        return Err(CaptureActionError::Lookup(CaptureLookupError::TargetMismatch).into());
    }
    anyhow::ensure!(
        capture.frame_geometry().is_some() && qualified_backend(),
        "capture_route_unqualified"
    );
    Ok(WindowCaptureMetadata {
        id,
        binding,
        frame: capture.frame_geometry().unwrap(),
        encoded: capture.encoded_dimensions(),
    })
}

/// Check target/binding before any native target resolution, without consuming.
#[cfg(target_os = "linux")]
pub(crate) fn preflight_window_click(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    pid: u32,
    window_id: u64,
) -> anyhow::Result<()> {
    window_capture_metadata(service, args, capture_id, pid, window_id).map(|_| ())
}

// Admit on the originating dispatch task: the registry's runtime scope is
// task/thread-local and must not be implicitly adopted by a blocking worker.
#[cfg(target_os = "linux")]
pub fn admit_window_click(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    pid: u32,
    window_id: u64,
    screenshot_x: f64,
    screenshot_y: f64,
    geometry: CaptureFrameGeometry,
) -> anyhow::Result<CaptureActionAdmission> {
    use cua_driver_core::capture_runtime::CaptureActionError;
    let capture = window_capture_metadata(service, args, capture_id, pid, window_id)?;
    if capture.frame != geometry {
        return Err(CaptureActionError::NativeActionFrameMismatch.into());
    }
    let encoded = capture.encoded;
    rounded_point(
        screenshot_x * f64::from(geometry.width()) / f64::from(encoded.width()),
        screenshot_y * f64::from(geometry.height()) / f64::from(encoded.height()),
        geometry,
        crate::wayland::wayland_input_enabled(),
    )?;
    Ok(service.admit_action_in_frame(
        CaptureActionRequest {
            capture_id: capture.id,
            binding: capture.binding,
            target: window_target(pid, window_id),
            current_native_action_dimensions: NativeActionDimensions::new(
                geometry.width(),
                geometry.height(),
            )?,
            screenshot_x,
            screenshot_y,
        },
        geometry,
    )?)
}

pub fn retire_runtime(service: &CaptureService) {
    service.retire_runtime();
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::capture_runtime::admission_error_code;
    use sha2::{Digest, Sha256};

    fn snapshot(pid: u32, window: u64) -> cua_driver_core::element_token::SnapshotIdentity {
        let registry = cua_driver_core::element_token::global();
        let candidate = registry
            .prepare_current(
                i32::try_from(pid).unwrap(),
                window,
                0,
                std::sync::Arc::new(()),
            )
            .unwrap();
        let identity = candidate.identity();
        registry
            .publish(candidate, std::time::Duration::from_secs(2))
            .unwrap();
        identity
    }

    fn publish_window(
        service: &CaptureService,
        args: &Value,
        png: &[u8],
        pid: u32,
        window: u64,
        encoded: (u32, u32),
        native: (u32, u32),
    ) -> anyhow::Result<String> {
        super::publish_window(
            service,
            args,
            png,
            pid,
            window,
            encoded,
            native,
            snapshot(pid, window),
            None,
        )
    }

    #[test]
    fn replaced_snapshot_revokes_capture_and_admission_holds_the_generation() {
        use cua_driver_core::element_token::{self, RegistryError};
        let service = CaptureService::default();
        let args = args("snapshot-bound-linux");
        let pid = std::process::id();
        let window = 0x7f31_0001;
        let identity = snapshot(pid, window);
        let id = super::publish_window(
            &service,
            &args,
            &png(4, 4, 1),
            pid,
            window,
            (4, 4),
            (4, 4),
            identity,
            None,
        )
        .unwrap();
        let second_id = super::publish_window(
            &service,
            &args,
            &png(4, 4, 2),
            pid,
            window,
            (4, 4),
            (4, 4),
            identity,
            None,
        )
        .unwrap();
        let admission = admit_with_live_dimensions(
            &service,
            &args,
            &id,
            window_target(pid, window),
            NativeActionDimensions::new(4, 4).unwrap(),
            1.0,
            1.0,
        )
        .unwrap();
        let registry = element_token::global();
        let replacement = registry
            .prepare_current(pid as i32, window, 0, std::sync::Arc::new(()))
            .unwrap();
        assert_eq!(
            registry.publish(replacement, std::time::Duration::ZERO),
            Err(RegistryError::PublicationTimeout)
        );
        drop(admission);
        snapshot(pid, window);
        let binding = service.binding_from_args(&args).unwrap();
        let error = service
            .read_for_perception(service.parse_capture_id(&second_id).unwrap(), &binding)
            .unwrap_err();
        assert_eq!(
            error,
            cua_driver_core::capture_runtime::CaptureLookupError::GenerationMismatch
        );
    }

    fn png(width: u32, height: u32, value: u8) -> Vec<u8> {
        let rgba = vec![value; (width * height * 4) as usize];
        cua_driver_core::image_utils::encode_rgba_to_png(&rgba, width, height)
            .expect("encode fixture")
    }

    #[test]
    fn capture_click_shape_is_explicit_and_unsupported_is_not_stale() {
        use serde_json::json;
        let request = json!({"capture_id":"capture_00000000000000000000000000000001_0000000000000001",
            "pid":7,"window_id":9,"x":1.5,"y":2.0});
        assert!(validate_click(&request).is_ok());
        for (field, value, code) in [
            ("capture_id", Value::Null, "capture_id_invalid"),
            ("pid", Value::Null, "invalid_arguments"),
            ("window_id", Value::Null, "invalid_arguments"),
            ("x", Value::Null, "invalid_arguments"),
            ("y", Value::Null, "invalid_arguments"),
            ("element_token", json!("token"), "invalid_arguments"),
            ("element_index", json!(1), "invalid_arguments"),
            ("from_zoom", json!(true), "invalid_arguments"),
            ("scope", json!("desktop"), "capture_route_unqualified"),
            (
                "coordinate_frame",
                json!("desktop"),
                "capture_route_unqualified",
            ),
        ] {
            let mut invalid = request.clone();
            invalid[field] = value;
            let error = validate_click(&invalid).unwrap_err();
            let data = error.structured_content.unwrap();
            assert_eq!(data["code"], code, "{field}");
            assert_eq!(data["effect"], "refused");
        }
        assert!(validate_click(&json!({"element_token":"legacy"})).is_ok());
        assert!(validate_click(&json!({"x":1,"y":2,"scope":"desktop"})).is_ok());
    }

    #[test]
    fn capture_quantization_rounds_once_without_wayland_center_retargeting() {
        let geometry = CaptureFrameGeometry::new(0, 0, 500, 300, 1000, 600).unwrap();
        assert_eq!(
            rounded_point(199.5, 134.5, geometry, true).unwrap(),
            (200, 135)
        );
        assert_eq!(rounded_point(0.5, 0.75, geometry, true).unwrap(), (1, 1));
        for point in [(0.0, 0.0), (499.75, 20.0), (20.0, 299.75)] {
            let error = rounded_point(point.0, point.1, geometry, true).unwrap_err();
            assert_eq!(
                cua_driver_core::capture_runtime::admission_error_code(&error),
                "capture_coordinate_invalid"
            );
        }
        assert_eq!(rounded_point(0.0, 0.0, geometry, false).unwrap(), (0, 0));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn source_pixel_freshness_ignores_png_metadata_but_not_repaint() {
        let geometry = CaptureFrameGeometry::new(31, 23, 4, 3, 8, 6).unwrap();
        let original = png(8, 6, 0x55);
        let frame = fingerprint_frame(&original, geometry).unwrap();
        use image::{
            codecs::png::{CompressionType, FilterType, PngEncoder},
            ImageEncoder,
        };
        let image = image::load_from_memory(&original).unwrap().to_rgba8();
        let mut alternate = Vec::new();
        PngEncoder::new_with_quality(
            &mut alternate,
            CompressionType::Uncompressed,
            FilterType::NoFilter,
        )
        .write_image(image.as_raw(), 8, 6, image::ExtendedColorType::Rgba8)
        .unwrap();
        assert_ne!(original, alternate);
        assert_eq!(frame, fingerprint_frame(&alternate, geometry).unwrap());
        assert_ne!(
            frame,
            fingerprint_frame(&png(8, 6, 0x56), geometry).unwrap()
        );
    }

    fn args(session: &str) -> Value {
        serde_json::json!({"_session_id": session})
    }

    fn admit_window(
        service: &CaptureService,
        args: &Value,
        capture_id: &str,
        pid: u32,
        window_id: u64,
        point: (f64, f64),
        native: (u32, u32),
    ) -> anyhow::Result<(f64, f64)> {
        admit_with_live_dimensions(
            service,
            args,
            capture_id,
            window_target(pid, window_id),
            NativeActionDimensions::new(native.0, native.1).unwrap(),
            point.0,
            point.1,
        )
        .map(|admission| (admission.action_x, admission.action_y))
    }

    fn admit_desktop(
        service: &CaptureService,
        args: &Value,
        capture_id: &str,
        point: (f64, f64),
        native: (u32, u32),
    ) -> anyhow::Result<(f64, f64)> {
        admit_with_live_dimensions(
            service,
            args,
            capture_id,
            CaptureTarget::PrimaryDesktop,
            NativeActionDimensions::new(native.0, native.1).unwrap(),
            point.0,
            point.1,
        )
        .map(|admission| (admission.action_x, admission.action_y))
    }

    #[test]
    fn window_publication_digests_the_exact_returned_png() {
        let service = CaptureService::default();
        let delivered = png(2, 2, 0x41);
        let id = publish_window(
            &service,
            &args("digest"),
            &delivered,
            std::process::id(),
            99,
            (2, 2),
            (4, 4),
        )
        .expect("publish window capture");
        let binding = service.binding_from_args(&args("digest")).unwrap();
        let capture = service
            .read_for_perception(service.parse_capture_id(&id).unwrap(), &binding)
            .expect("read published capture");

        assert_eq!(capture.png_bytes().as_ref(), delivered.as_slice());
        let expected: [u8; 32] = Sha256::digest(&delivered).into();
        assert_eq!(capture.digest().as_bytes(), &expected);
    }

    #[test]
    fn target_mismatch_does_not_consume_but_successful_admission_does() {
        let service = CaptureService::default();
        let call_args = args("one-action");
        let id = publish_window(
            &service,
            &call_args,
            &png(10, 10, 0x22),
            std::process::id(),
            11,
            (10, 10),
            (20, 20),
        )
        .unwrap();

        assert!(admit_window(
            &service,
            &call_args,
            &id,
            std::process::id(),
            12,
            (2.0, 3.0),
            (20, 20)
        )
        .is_err());
        assert_eq!(
            admit_window(
                &service,
                &call_args,
                &id,
                std::process::id(),
                11,
                (2.0, 3.0),
                (20, 20)
            )
            .unwrap(),
            (4.0, 6.0)
        );
        assert!(admit_window(
            &service,
            &call_args,
            &id,
            std::process::id(),
            11,
            (2.0, 3.0),
            (20, 20)
        )
        .is_err());
    }

    #[test]
    fn retired_session_capture_is_stale() {
        let service = CaptureService::default();
        let call_args = args("retired");
        let id = publish_desktop(&service, &call_args, &png(3, 2, 0x77), (3, 2), (3, 2)).unwrap();

        let binding = service.binding_from_args(&call_args).unwrap();
        service.retire_session(&binding);

        assert!(admit_desktop(&service, &call_args, &id, (1.0, 1.0), (3, 2)).is_err());
    }

    #[test]
    fn window_transform_preserves_each_rounded_axis() {
        let service = CaptureService::default();
        let call_args = args("rounding");
        let id = publish_window(
            &service,
            &call_args,
            &png(3, 2, 0x33),
            std::process::id(),
            13,
            (3, 2),
            (5, 3),
        )
        .unwrap();

        let (x, y) = admit_window(
            &service,
            &call_args,
            &id,
            std::process::id(),
            13,
            (1.0, 1.0),
            (5, 3),
        )
        .unwrap();
        assert_eq!(x, 5.0 / 3.0);
        assert_eq!(y, 3.0 / 2.0);
    }

    #[test]
    fn another_session_cannot_admit_or_consume_a_capture() {
        let service = CaptureService::default();
        let owner = args("owner");
        let id = publish_desktop(&service, &owner, &png(4, 3, 0x21), (4, 3), (4, 3)).unwrap();

        let refusal = admit_desktop(&service, &args("other"), &id, (2.0, 1.0), (4, 3))
            .expect_err("cross-session admission must fail");
        assert_eq!(
            admission_error_code(&refusal),
            "capture_generation_mismatch"
        );
        assert_eq!(
            admit_desktop(&service, &owner, &id, (2.0, 1.0), (4, 3)).unwrap(),
            (2.0, 1.0)
        );
    }

    #[test]
    fn hidpi_desktop_uses_the_compositors_logical_action_frame() {
        let native = (3200, 2000);
        let logical = select_desktop_action_dimensions(native, Some((1600, 1000))).unwrap();
        assert_eq!(logical, (1600, 1000));

        let service = CaptureService::default();
        let call_args = args("hidpi");
        let id = publish_desktop(
            &service,
            &call_args,
            &png(logical.0, logical.1, 0x31),
            logical,
            logical,
        )
        .unwrap();
        assert_eq!(
            admit_desktop(&service, &call_args, &id, (800.0, 500.0), logical).unwrap(),
            (800.0, 500.0)
        );
    }

    #[test]
    fn downsized_desktop_capture_maps_back_to_the_action_frame() {
        let service = CaptureService::default();
        let call_args = args("downsized");
        let id = publish_desktop(&service, &call_args, &png(4, 3, 0x42), (4, 3), (8, 6)).unwrap();
        assert_eq!(
            admit_desktop(&service, &call_args, &id, (2.0, 1.5), (8, 6)).unwrap(),
            (4.0, 3.0)
        );
    }

    #[test]
    fn desktop_publication_uses_the_post_normalization_bytes() {
        let service = CaptureService::default();
        let normalized = png(4, 3, 0x18);
        let id = publish_desktop(&service, &args("desktop"), &normalized, (4, 3), (4, 3)).unwrap();
        let binding = service.binding_from_args(&args("desktop")).unwrap();
        let capture = service
            .read_for_perception(service.parse_capture_id(&id).unwrap(), &binding)
            .unwrap();

        assert_eq!(capture.png_bytes().as_ref(), normalized.as_slice());
        assert_eq!(
            admit_desktop(&service, &args("desktop"), &id, (2.0, 1.0), (4, 3)).unwrap(),
            (2.0, 1.0)
        );
    }

    #[test]
    fn live_resize_refuses_before_dispatch_without_consuming_capture() {
        let service = CaptureService::default();
        let call_args = args("resize");
        let id = publish_window(
            &service,
            &call_args,
            &png(4, 3, 0x44),
            std::process::id(),
            14,
            (4, 3),
            (8, 6),
        )
        .unwrap();
        let mut dispatches = 0;
        let refusal = admit_window(
            &service,
            &call_args,
            &id,
            std::process::id(),
            14,
            (1.25, 1.5),
            (9, 6),
        );
        if refusal.is_ok() {
            dispatches += 1;
        }
        assert_eq!(dispatches, 0);
        assert!(refusal
            .unwrap_err()
            .downcast_ref::<cua_driver_core::capture_runtime::CaptureActionError>()
            .is_some_and(|error| {
                *error
                == cua_driver_core::capture_runtime::CaptureActionError::NativeActionFrameMismatch
            }));
        assert_eq!(
            admit_window(
                &service,
                &call_args,
                &id,
                std::process::id(),
                14,
                (1.25, 1.5),
                (8, 6),
            )
            .unwrap(),
            (2.5, 3.0)
        );
    }
}
