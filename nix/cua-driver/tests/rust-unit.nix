# Source-built Linux Rust unit/compile checks.
#
# This is intentionally separate from package.nix: the shipped daemon package
# must remain display-independent, while this check validates the Rust source
# and testkit in Nix's reproducible build environment.
{
  pkgs,
  src,
  sourceSubdir ? null,
  ...
}:

let
  rustSrc = if sourceSubdir == null then src else "${src}/${sourceSubdir}";
in
pkgs.rustPlatform.buildRustPackage {
  pname = "cua-driver-rust-unit-tests";
  version = (pkgs.lib.importTOML "${rustSrc}/Cargo.toml").workspace.package.version;
  inherit src;

  cargoLock.lockFile = "${rustSrc}/Cargo.lock";
  postUnpack = pkgs.lib.optionalString (sourceSubdir != null) ''
    sourceRoot="$sourceRoot/${sourceSubdir}"
  '';
  cargoTestFlags = [
    "-p"
    "cua-driver"
    "-p"
    "cua-driver-core"
    "-p"
    "cua-driver-testkit"
    "-p"
    "platform-linux"
    "--all-targets"
    "--"
    "--skip"
    "direct_mcp_runtime_matches_the_released_protocol_contract"
    "--skip"
    "released_mcp_initialize_tools_list_and_error_fields_remain_compatible"
    # These daemon-topology integration tests require an admitted desktop
    # service. The Nix build sandbox intentionally has no AT-SPI session; Linux
    # CI runs them on a host session, while this derivation retains unit/direct
    # MCP and the explicit strict-admission process matrix.
    "--skip"
    "capability_manifest_narrows_standard_mode"
    "--skip"
    "capability_manifest_narrows_unrestricted_mode"
    "--skip"
    "bounded_manifest_is_an_immutable_deny_by_default_layer"
    "--skip"
    "cli_call_succeeds_through_test_owned_daemon"
    "--skip"
    "implicitly_started_named_session_survives_across_one_shot_cli_calls"
    "--skip"
    "named_cli_session_cleanup_is_isolated"
    "--skip"
    "named_session_survives_across_one_shot_cli_calls"
    "--skip"
    "revoke_cli_ends_the_exact_live_session"
    "--skip"
    "standard_mode_refuses_existing_profile_without_a_launch_grant"
    "--skip"
    "unrestricted_mode_skips_runtime_existing_profile_consent"
    "--skip"
    "embedded_host_serves_sdk_and_mcp_with_one_contract"
    "--skip"
    "dropping_the_host_cannot_orphan_its_daemon"
    "--skip"
    "concurrent_start_coalesces_and_restart_rotates_generation"
    "--skip"
    "standard_mode_refuses_protected_permission_prompt_over_real_socket"
  ];

  nativeBuildInputs = with pkgs; [
    pkg-config
    rustPlatform.bindgenHook
    clang
    dbus
  ];
  buildInputs = with pkgs; [
    libx11
    libxi
    libxtst
    libxext
    pipewire
    libei
  ];

  # The default Cargo test set is deliberately headless. The ignored desktop
  # matrix is run by the manual Linux e2e workflow and is not hidden in Nix.
  doCheck = true;
  preCheck = ''
    export CUA_DRIVER_TEST_DBUS_SESSION_CONFIG=${pkgs.dbus}/share/dbus-1/session.conf
  '';

  installPhase = ''
    touch $out
  '';
}
