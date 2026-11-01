// SPDX-License-Identifier: FSL-1.1-MIT
// Copyright (c) 2026 Cua AI, Inc.

//! A direct sender has no authority to capture Zen, even with AutoApprove.
use cua_spacesd_client::testing::{MockAuth, MockServer};
use cua_spacesd_client::{ConnectOptions, SpacesdClient};
use cua_teleport::{
    AppRef, AutoApprove, Error, HostEffects, Platform, Selection, Teleporter, TransferScope,
};
use std::sync::Arc;
use std::sync::atomic::Ordering;

struct NoSource;
impl HostEffects for NoSource {
    fn home_dir(&self) -> Option<std::path::PathBuf> {
        panic!("source lookup before Keyvault")
    }
    fn run(&self, _: &cua_teleport::HostCommand) -> std::io::Result<cua_teleport::HostOutput> {
        panic!("host effect before Keyvault")
    }
    fn authorize_sensitive_export(
        &self,
        _: &str,
    ) -> Result<(), cua_teleport::biometric::AuthError> {
        panic!("presence before Keyvault")
    }
    fn devtools_page_urls(&self, _: &str) -> std::io::Result<Vec<String>> {
        panic!("browser read before Keyvault")
    }
}

#[tokio::test]
async fn zen_direct_send_refuses_before_source_approval_presence_or_network() {
    let mock = MockServer::start(MockAuth::default()).await;
    let env = SpacesdClient::connect(ConnectOptions::parse(&mock.url()).unwrap())
        .await
        .unwrap();
    let before = mock.state.observed.grpc_requests.load(Ordering::SeqCst);
    let sender = Teleporter::with_host(Arc::new(NoSource));
    for alias in ["zen", "zen-browser", "app.zen_browser.zen", "ZEN"] {
        for selection in [
            Selection::Default,
            Selection::All,
            Selection::Items(vec!["zen/logins.json".into()]),
        ] {
            let app = AppRef {
                app_id: alias.into(),
                display_name: "not Firefox".into(),
                platform: Platform::Linux,
            };
            for approval in [
                Arc::new(AutoApprove) as Arc<dyn cua_teleport::Approval>,
                Arc::new(|_: &cua_teleport::ApprovalRequest<'_>| -> bool {
                    panic!("approval before Keyvault")
                }),
            ] {
                let err = sender
                    .send(
                        &env,
                        &app,
                        TransferScope::FullProfile,
                        selection.clone(),
                        approval,
                    )
                    .await
                    .unwrap_err();
                assert!(matches!(err, Error::ZenKeyvaultRequired), "{err}");
                assert!(err.to_string().starts_with("zen_keyvault_required:"));
            }
        }
    }
    assert_eq!(
        mock.state.observed.grpc_requests.load(Ordering::SeqCst),
        before,
        "no GetManifest or upload"
    );
    assert!(mock.state.teleport_imports().is_empty());
}
