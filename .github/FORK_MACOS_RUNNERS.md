# Fork macOS CI runner routing

This fork can send owner-initiated macOS CI to disposable ARM64 VMs. Routing is
**off unless `FORK_MACOS_CI_ENABLED` is `true` and `FORK_MACOS_RUNNER` is set**.
The separate activation gate lets the earlier two-job runner qualification use
its existing variable without activating this broader set. This commit does not provision runners
or set repository variables. Enable only after runner qualification.

## Variables and guards

- `FORK_MACOS_CI_ENABLED`: unset by default; set exactly `true` only after
  qualification and exclusions are configured. Unset it to disable broad routing.
- `FORK_MACOS_RUNNER`: JSON runner-label array, normally
  `["self-hosted","macOS","ARM64","fork-vm"]`.
- `FORK_MACOS_EXCLUDE`: JSON array of stable `workflow-stem/job-id` keys below;
  unset means `[]`. A matching key returns that job to its original hosted label,
  including all its macOS matrix cells. Set exclusions **before** enabling routing.
- Both `github.actor` and `github.triggering_actor` must be the repository owner.
  A pull request must originate in this repository, not an external fork.
- Matrix jobs route only `macos-latest` or `macos-26` cells. Linux, Windows,
  version-pinned macOS and Intel labels retain their original runners.
- Reusable installer and macOS E2E workflows additionally require their own
  `github.workflow_ref`: calls from CD/release workflows stay hosted. Direct
  dispatches can route. Release metadata's installer call also remains hosted.
- No release/publish/CD/signing or docs-nightly workflow is changed. Existing
  upstream self-hosted Lume routes are untouched; they are not this VM pool.
- Malformed JSON is an operator configuration error, not an invitation to choose
  a runner implicitly. Remove `FORK_MACOS_RUNNER` to disable all fork routing.

## Initial exclusions

```json
[
  "ci-cua-spaces-macos/swift",
  "ci-jev-use/released-driver-macos",
  "ci-jev-use/native-appkit-macos",
  "e2e-rust-macos/matrix"
]
```

- `ci-cua-spaces-macos/swift`: XCUITest and app launch/snapshot checks; guest GUI/TCC not qualified; Sparkle appcast dry run uses signing material when configured.
- `ci-jev-use/released-driver-macos`: Seeds Accessibility/Screen Recording using seed-tcc-guest.sh, assumes SIP-off disposable VirtualMac and Chrome; guest image not qualified.
- `ci-jev-use/native-appkit-macos`: Seeds TCC then performs real AppKit background inputs; guest GUI/TCC not qualified.
- `e2e-rust-macos/matrix`: run-hosted-rust-e2e.sh drives GUI/capture/browser lanes and TCC setup; image not qualified.

These are pending image qualifications, not proof that each job is impossible.
The `e2e-rust-macos/probe` job is a non-input environment diagnostic; it can run
in the VM independently of the excluded GUI lanes. There is no newly routed
nested-Lume job: `ci-swift-lume` stays pinned to macos-15, `e2e-cua-sdk` is Linux
and explicitly excludes its Lume lane, and the existing macOS Lume dispatch
path remains unchanged. Native docs generation's Lume row builds metadata; it
does not boot a VM.

## Inventory

The entries below have the opt-in guard, not unconditional self-hosted routing.
Unlisted workflows and jobs retain their original `runs-on`.

| Stable job key | Hosted fallback | Initial disposition / step evidence |
| --- | --- | --- |
| `ci-check-docs/native-reference` | `${{ matrix.os }}` | Native docs generator rows: cua-driver and lume; metadata/build, not nested virtualization |
| `ci-cua-driver-bench/harness` | `macos-26` | Build BenchLab and BenchSentinel; Run harness, checker and sentinel tests |
| `ci-cua-driver-contract-clients/mcp-client-discovery` | `${{ matrix.os }}` | Build the release-mode MCP server; Verify discovery without model or account calls; Verify modern MCP and embedded skill resources |
| `ci-cua-driver-contract-clients/runtime-parity` | `${{ matrix.os }}` | Record Windows runner session; Prove portable contracts match the live registry; Verify released CLI and MCP compatibility; Compile the UniFFI SDK library; Compile the frozen previous-release Rust application; Run the frozen previous-release Rust application; Prove the embedded host serves SDK and MCP clients; Prove Windows Session 0 refusal and desktop-free metadata |
| `ci-cua-driver-installer-compat/install` | `${{ matrix.os }}` | Run the current macOS/Linux installer against the release; Run the current Windows installer against the release |
| `ci-cua-s1/macos-unit` | `macos-latest` | Run unit tests with the four-b inference extra |
| `ci-cua-sdk/rust` | `${{ matrix.os }}` | Format; Clippy (native); Clippy (browser API, host metadata build); Build (wasm32); Test Spaces discovery and post-create directory failures; Test (embedded, daemon over UDS and loopback, CLI, Cua Spaces extensions); Test (shared media crates and clients); Clippy and test (cua-media-codec); Format and clippy (streaming bench); Client/server dependency boundary |
| `ci-cua-sdk/spaces-e2e` | `${{ matrix.os }}` | Format; Clippy; Test |
| `ci-cua-sdk/bindings` | `${{ matrix.os }}` | Generated bindings are up to date; Spaces contract manifest and fixtures are up to date |
| `ci-cua-sdk/python` | `${{ matrix.os }}` | Build the SDK, fixtures and CLIs; Build the platform wheel; Smoke-test the installed wheel |
| `ci-cua-sdk/node` | `${{ matrix.os }}` | Build and stage; Test |
| `ci-cua-sdk/swift` | `macos-26` | Dev dylib; spaces-sdk-swift (the CuaSpaces overlay, against the fixtures); XCFramework (static) binary target |
| `ci-cua-spaces-macos/swift` | `macos-26` | XCUITest and app launch/snapshot checks; guest GUI/TCC not qualified; Sparkle appcast dry run uses signing material when configured. |
| `ci-cua-spaces/bundle` | `${{ matrix.os }}` | Cache cargo; Rust command-layer tests (hermetic); Stage the cua CLI sidecar; Build unsigned bundles; Bundles carry the cua sidecar; Upload bundles |
| `ci-cua-spacesd/test` | `${{ matrix.os }}` | Format; Test; Clippy; ScreenCaptureKit single-version gate; Server-only boundary |
| `ci-infinite-canvas-swift/swift` | `macos-26` | Build and stage the cua SDK for Swift; Cursor theme is the driver's; Test the sample; Benchmark (headless hot paths) |
| `ci-jev-use/released-driver-macos` | `macos-26` | Seeds Accessibility/Screen Recording using seed-tcc-guest.sh, assumes SIP-off disposable VirtualMac and Chrome; guest image not qualified. |
| `ci-jev-use/native-appkit-macos` | `macos-26` | Seeds TCC then performs real AppKit background inputs; guest GUI/TCC not qualified. |
| `ci-openkoalabot-example-samples/swift` | `macos-26` | Build and stage the cua SDK for Swift; Test the sample |
| `ci-platform-linux-portability/platform-linux-off-linux` | `macos-latest` | Test platform-linux off Linux |
| `ci-rust-format/format` | `${{ matrix.os }}` | Check Rust formatting |
| `ci-rust-macos/unit` | `macos-latest` | Run platform-macos unit tests; Run cua-driver-core unit tests on macOS; Run macOS protocol schema contract; Run hermetic cua-driver-e2e tests on macOS |
| `ci-swift-linker/link` | `macos-latest` | Verify native linker output; Preserve linker evidence |
| `ci-test-scripts/macos-uninstall` | `macos-26` | Test uninstall contracts, system Bash syntax, and script encoding; Test Computer History purge during uninstall |
| `e2e-rust-macos/probe` | `macos-26` | Initialize evidence directory; Validate requested source SHA; Verify exact checkout; Probe hosted macOS GUI environment; Upload hosted environment evidence |
| `e2e-rust-macos/matrix` | `macos-26` | run-hosted-rust-e2e.sh drives GUI/capture/browser lanes and TCC setup; image not qualified. |
