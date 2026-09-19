/// Stable address on one AT-SPI bus connection, including the owning frame.
/// Unique bus names prevent a restarted process from reusing an observed path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtspiIdentity {
    pub bus_name: String,
    pub path: String,
    pub frame_bus_name: String,
    pub frame_path: String,
}

#[derive(Clone, Debug)]
pub struct AtspiNode {
    pub element_index: Option<usize>,
    pub role: String,
    pub name: Option<String>,
    pub value: Option<String>,
    /// Checked state when the accessibility backend exposes one for a toggle.
    pub checked: Option<bool>,
    /// Enabled state when the accessibility backend returned a state set.
    pub enabled: Option<bool>,
    /// Toggle/selection state for selectable controls.
    pub selected: Option<bool>,
    pub description: Option<String>,
    pub actions: Vec<String>,
    /// For AT-SPI: stable FNV-1a hash of the bus name and object path.
    /// For X11 fallback: element_key = xid.
    pub element_key: u64,
    pub identity: Option<AtspiIdentity>,
    /// Depth in the markdown tree (0 = top-level window child).
    /// Defaults to 0 when not tracked (e.g. X11 fallback path).
    pub depth: usize,
    /// `element_index` of the nearest actionable ancestor, if any.
    /// Mirrors what the markdown indent shows.
    pub parent_element_index: Option<usize>,
    /// True when the native AT-SPI walker observed this node below renderer
    /// web content. Browser-owned consent UI must never match such nodes.
    pub in_web_content: bool,
}

/// No input has been delivered; this control needs a real pointer click.
#[derive(Debug)]
pub(crate) struct ElementClickNeedsForeground;
impl std::fmt::Display for ElementClickNeedsForeground {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("editable text or table cell selection requires real foreground pointer input")
    }
}
impl std::error::Error for ElementClickNeedsForeground {}

/// The requested indexed click has no supported AX activation route. This
/// classification is made before submitting any action, so pointer fallback
/// is safe. Other action errors must not be treated as this pre-input result.
#[derive(Debug)]
pub(crate) struct ClickActionUnavailable(pub String);
impl std::fmt::Display for ClickActionUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ClickActionUnavailable {}

/// Only typed pre-dispatch refusals can select a second delivery route.
pub(crate) fn click_error_allows_pointer_fallback(error: &anyhow::Error) -> bool {
    error.is::<ClickActionUnavailable>() || error.is::<ElementClickNeedsForeground>()
}

#[cfg(test)]
mod click_error_tests {
    use super::*;

    #[test]
    fn only_explicit_no_input_errors_allow_pointer_fallback() {
        assert!(click_error_allows_pointer_fallback(
            &ClickActionUnavailable("no safe action".into()).into()
        ));
        assert!(click_error_allows_pointer_fallback(
            &ElementClickNeedsForeground.into()
        ));
    }

    #[test]
    fn failed_or_unknown_dispatch_must_not_replay() {
        for message in [
            "doAction returned false",
            "doAction failed: disconnected",
            "indexed click action timed out",
            "observed object is defunct",
            "no safe action", // Text resembling a typed refusal is not authority.
        ] {
            assert!(!click_error_allows_pointer_fallback(&anyhow::anyhow!(
                message
            )));
        }
    }
}
