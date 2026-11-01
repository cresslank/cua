//! Persistent focus operations. One immutable, attested helper owner/epoch per action.
use super::*;

#[derive(Debug, Clone)]
pub(crate) struct SwitchTarget {
    pub window_id: u64,
    pub pid: u32,
    pub app_id: String,
    target_id: String,
}

#[derive(Clone)]
pub(crate) struct SwitchPeer {
    owner: String,
    epoch: String,
    capabilities: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct SwitchState {
    protocol_version: u64,
    epoch: String,
    pub overview_visible: bool,
    pub overlay_key_is_super: bool,
    pub session_locked: bool,
    pub search_entry_has_key_focus: bool,
    pub key_focus_in_shell_ui: bool,
    modal_count: u32,
    foreground_transaction_open: bool,
    persistent_switch_open: bool,
    focus_window: Option<String>,
    focus_pid: Option<u32>,
}

impl SwitchState {
    pub fn search_ready(&self) -> bool {
        self.overview_visible
            && !self.session_locked
            && !self.foreground_transaction_open
            && !self.persistent_switch_open
            && self.search_entry_has_key_focus
            && self.key_focus_in_shell_ui
            && self.modal_count <= 1
    }

    pub fn shell_focused(&self) -> bool {
        !self.session_locked
            && self.key_focus_in_shell_ui
            && !self.foreground_transaction_open
            && !self.persistent_switch_open
    }

    pub fn idle(&self) -> bool {
        !self.foreground_transaction_open && !self.persistent_switch_open
    }

    pub fn target_focused(&self, target: &SwitchTarget) -> bool {
        !self.session_locked
            && !self.overview_visible
            && !self.key_focus_in_shell_ui
            && self.idle()
            && self.modal_count == 0
            && self.focus_pid == Some(target.pid)
            && self.focus_window.as_deref() == Some(target.target_id.as_str())
    }
}

impl SwitchPeer {
    pub fn connect() -> anyhow::Result<Self> {
        let owner = shell_owner(false)
            .ok_or_else(|| anyhow::anyhow!("shell_not_ready: no attested WinRects owner"))?;
        let raw = transport::call_result(
            &owner,
            PATH,
            &format!("{IFACE}.GetCapabilities"),
            &[],
            Duration::from_millis(800),
        )?;
        let caps: Capabilities = serde_json::from_str(extract_json_object(&raw).unwrap_or(""))?;
        anyhow::ensure!(
            caps.protocol_version == REQUIRED_EXACT_TARGET_PROTOCOL
                && !caps.epoch.is_empty()
                && caps.capabilities.iter().any(|cap| cap == "exact-target-v2"),
            "unsupported: helper lacks exact-target protocol 4"
        );
        anyhow::ensure!(
            shell_owner(false).as_deref() == Some(&owner),
            "shell_not_ready: helper owner changed"
        );
        Ok(Self {
            owner,
            epoch: caps.epoch,
            capabilities: caps.capabilities,
        })
    }

    pub fn has(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|item| item == cap)
    }

    fn call(&self, method: &str, args: &[String], timeout: Duration) -> anyhow::Result<String> {
        cua_driver_core::blocking::check_cancelled()?;
        anyhow::ensure!(
            shell_owner(false).as_deref() == Some(&self.owner),
            "shell_not_ready: helper owner changed"
        );
        let raw = transport::call_result(
            &self.owner,
            PATH,
            &format!("{IFACE}.{method}"),
            args,
            timeout,
        )?;
        anyhow::ensure!(
            shell_owner(false).as_deref() == Some(&self.owner),
            "shell_not_ready: helper owner changed"
        );
        Ok(raw)
    }

    pub fn focus_search(&self, target: &SwitchTarget) -> anyhow::Result<SwitchState> {
        self.validate_target(target)?;
        let raw = self.call(
            "FocusSwitchSearch",
            &[target.target_id.clone()],
            Duration::from_millis(800),
        )?;
        let value: serde_json::Value =
            serde_json::from_str(extract_json_object(&raw).unwrap_or(""))?;
        let state: SwitchState = serde_json::from_value(value.clone())?;
        anyhow::ensure!(
            state.protocol_version == REQUIRED_EXACT_TARGET_PROTOCOL && state.epoch == self.epoch,
            "shell_not_ready: invalid search-focus epoch/protocol"
        );
        if let Some(code) = value.get("code").and_then(serde_json::Value::as_str) {
            anyhow::bail!("{code}: Shell search focus could not be established");
        }
        Ok(state)
    }

    pub fn deliver_search(&self, target: &SwitchTarget, query: &str) -> anyhow::Result<()> {
        self.validate_target(target)?;
        let raw = self.call(
            "DeliverSwitchSearch",
            &[target.target_id.clone(), query.into()],
            Duration::from_millis(800),
        )?;
        let payload: serde_json::Value =
            serde_json::from_str(extract_json_object(&raw).unwrap_or(""))?;
        anyhow::ensure!(
            payload["protocol_version"] == REQUIRED_EXACT_TARGET_PROTOCOL
                && payload["epoch"] == self.epoch
                && payload["target"] == target.target_id
                && payload["delivery"] == "shell_search",
            "switch_unverified: mismatched search delivery identity"
        );
        anyhow::ensure!(
            payload["delivered"] == true,
            "{}: Shell search delivery refused",
            payload["code"].as_str().unwrap_or("switch_unverified")
        );
        Ok(())
    }

    pub fn cancel_search(&self, target: &SwitchTarget) -> anyhow::Result<()> {
        self.call(
            "CancelSwitchSearch",
            &[target.target_id.clone()],
            Duration::from_millis(800),
        )?;
        Ok(())
    }

    pub fn targets(&self, pid: u32) -> anyhow::Result<Vec<SwitchTarget>> {
        let raw = self.call("GetRects", &[], Duration::from_millis(800))?;
        let windows = parse_shell_windows(&raw)
            .ok_or_else(|| anyhow::anyhow!("shell_not_ready: invalid exact window list"))?;
        anyhow::ensure!(
            windows
                .iter()
                .all(|window| window.helper_epoch == self.epoch),
            "shell_not_ready: helper epoch changed"
        );
        Ok(windows
            .into_iter()
            .filter(|window| window.pid == pid)
            .map(|window| SwitchTarget {
                window_id: window.public_id,
                pid: window.pid,
                app_id: window.app_id,
                target_id: window.target_id,
            })
            .collect())
    }

    pub fn state(&self) -> anyhow::Result<SwitchState> {
        anyhow::ensure!(
            self.has("shell-search-focus-v1"),
            "unsupported: helper lacks shell-search-focus-v1"
        );
        let raw = self.call("GetSwitchState", &[], Duration::from_millis(800))?;
        let state: SwitchState = serde_json::from_str(extract_json_object(&raw).unwrap_or(""))?;
        anyhow::ensure!(
            state.protocol_version == REQUIRED_EXACT_TARGET_PROTOCOL && state.epoch == self.epoch,
            "shell_not_ready: invalid switch-state epoch/protocol"
        );
        Ok(state)
    }

    pub fn validate_target(&self, target: &SwitchTarget) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.targets(target.pid)?
                .iter()
                .any(|current| current.window_id == target.window_id
                    && current.target_id == target.target_id),
            "window_not_found: exact switch target no longer exists"
        );
        Ok(())
    }

    pub fn native(&self, target: &SwitchTarget) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.has("persistent-switch-v1"),
            "unsupported: helper lacks persistent-switch-v1"
        );
        anyhow::ensure!(
            self.has("shell-search-focus-v1"),
            "unsupported: helper lacks switch focus readback"
        );
        self.validate_target(target)?;
        let transaction = new_foreground_token().replacen("cua-fg-", "cua-switch-", 1);
        let raw = self.call(
            "SwitchToTarget",
            &[transaction.clone(), target.target_id.clone()],
            Duration::from_secs(2),
        )?;
        let payload: serde_json::Value =
            serde_json::from_str(extract_json_object(&raw).unwrap_or(""))?;
        validate_switch_reply(&payload, &self.epoch, &transaction, &target.target_id)?;
        // A positive RPC is insufficient. Read the compositor's current focus.
        anyhow::ensure!(
            self.state()?.target_focused(target),
            "switch_unverified: native focus readback differs"
        );
        Ok(())
    }
}

fn validate_switch_reply(
    payload: &serde_json::Value,
    epoch: &str,
    transaction: &str,
    target: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        payload
            .get("protocol_version")
            .and_then(serde_json::Value::as_u64)
            == Some(REQUIRED_EXACT_TARGET_PROTOCOL)
            && payload.get("epoch").and_then(serde_json::Value::as_str) == Some(epoch)
            && payload
                .get("transaction")
                .and_then(serde_json::Value::as_str)
                == Some(transaction)
            && payload.get("target").and_then(serde_json::Value::as_str) == Some(target),
        "switch_unverified: mismatched switch response identity"
    );
    if payload.get("focused").and_then(serde_json::Value::as_bool) != Some(true) {
        anyhow::bail!(
            "{}",
            payload
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("switch_unverified")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn switch_reply_requires_every_identity_and_positive_readback() {
        let good = json!({"protocol_version":4,"epoch":"epoch","transaction":"tx","target":"epoch:7","focused":true});
        validate_switch_reply(&good, "epoch", "tx", "epoch:7").unwrap();
        for key in [
            "protocol_version",
            "epoch",
            "transaction",
            "target",
            "focused",
        ] {
            let mut missing = good.clone();
            missing.as_object_mut().unwrap().remove(key);
            let expected = if key == "focused" {
                "switch_unverified"
            } else {
                "switch_unverified: mismatched switch response identity"
            };
            assert_eq!(
                validate_switch_reply(&missing, "epoch", "tx", "epoch:7")
                    .unwrap_err()
                    .to_string(),
                expected,
                "{key}"
            );
            let mut wrong = good.clone();
            wrong[key] = json!("wrong");
            assert_eq!(
                validate_switch_reply(&wrong, "epoch", "tx", "epoch:7")
                    .unwrap_err()
                    .to_string(),
                expected,
                "{key}"
            );
        }
        let refused = json!({"protocol_version":4,"epoch":"epoch","transaction":"tx","target":"epoch:7","focused":false,"reason":"session_locked"});
        assert_eq!(
            validate_switch_reply(&refused, "epoch", "tx", "epoch:7")
                .unwrap_err()
                .to_string(),
            "session_locked"
        );
    }

    #[test]
    fn search_and_target_readbacks_are_strict_and_distinct() {
        let target = SwitchTarget {
            window_id: 7,
            pid: 42,
            app_id: "app".into(),
            target_id: "epoch:7".into(),
        };
        let base = json!({"protocol_version":4,"epoch":"epoch","overview_visible":false,"overlay_key_is_super":true,"session_locked":false,
            "search_entry_has_key_focus":false,"key_focus_in_shell_ui":false,"modal_count":0,"focus_window":"epoch:7","focus_pid":42,
            "foreground_transaction_open":false,"persistent_switch_open":false});
        let state =
            |value: serde_json::Value| serde_json::from_value::<SwitchState>(value).unwrap();
        assert!(state(base.clone()).target_focused(&target));
        assert!(!state(base.clone()).search_ready());
        for (key, value) in [
            ("overview_visible", json!(true)),
            ("session_locked", json!(true)),
            ("key_focus_in_shell_ui", json!(true)),
            ("modal_count", json!(1)),
            ("foreground_transaction_open", json!(true)),
            ("persistent_switch_open", json!(true)),
            ("focus_window", json!("other:7")),
            ("focus_pid", json!(99)),
        ] {
            let mut changed = base.clone();
            changed[key] = value;
            assert!(!state(changed).target_focused(&target), "{key}");
        }
        let mut search = base;
        search["overview_visible"] = json!(true);
        search["search_entry_has_key_focus"] = json!(true);
        search["key_focus_in_shell_ui"] = json!(true);
        assert!(state(search.clone()).search_ready());
        let mut additional_modal = search.clone();
        additional_modal["modal_count"] = json!(2);
        assert!(!state(additional_modal).search_ready());
        for key in [
            "session_locked",
            "overview_visible",
            "search_entry_has_key_focus",
            "key_focus_in_shell_ui",
            "modal_count",
            "foreground_transaction_open",
            "persistent_switch_open",
        ] {
            let mut missing = search.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert_eq!(
                serde_json::from_value::<SwitchState>(missing)
                    .unwrap_err()
                    .to_string(),
                format!("missing field `{key}`")
            );
        }
        assert!(!state(search.clone()).target_focused(&target));
        for (key, value) in [
            ("overview_visible", false),
            ("search_entry_has_key_focus", false),
            ("key_focus_in_shell_ui", false),
            ("session_locked", true),
        ] {
            let mut changed = search.clone();
            changed[key] = json!(value);
            assert!(!state(changed).search_ready(), "{key}");
        }
    }
}
