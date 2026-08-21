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
