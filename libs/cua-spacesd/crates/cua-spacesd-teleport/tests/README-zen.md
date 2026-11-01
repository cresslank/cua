# Contained normalized Zen receiver qualification

This is **Zen → Firefox**, not an installation of Zen in the guest. Linux is
currently the only advertised destination. `GetCapabilities` adds the existing
`teleport.zen` feature with these attributes, without changing protobufs:

```text
supported = true
format = normalized-gecko-v1
destination = firefox
platform = linux
```

A sender must require that exact format before capture; an old guest's missing
feature/attribute is a refusal, not permission to relabel Zen as Firefox.
`TeleportService.GetManifest(app="zen")` also lists `zen`. The installed-app
probe checks Firefox. No Zen package or download is required.

Accepted entries are exactly `zen/tabs.json` and the reserved `cookies.json`,
`localstorage.json`, `logins.json`. The latter contain the shared structured
item types, **not** profile files. Unknown/duplicate fields or entries, corrupt
records, missing declared entries, mismatched lengths/checksums, unsupported
origins, partitions/containers and non-lossless raw strings fail closed.
The profile is always `.mozilla/firefox/Profiles/cua.zen-import` under the
explicit destination home. Global `profiles.ini` and ordinary Firefox profiles
are not changed. An existing unmarked destination is refused.

The importer stages an owned-profile copy with bounded file/byte quotas,
validates supported destination SQLite schemas, uses Firefox's POSIX profile
lock plus a nonblocking import flock, and atomically renames/exchanges the
profile. Each staged generation creates and POSIX-locks its **own** regular
`.parentlock` before the atomic rename; its FD remains held through the durable
ledger callback and transaction commit. The separately locked predecessor stays
available until the rollback decision. No parent descriptor is reopened/closed
on the locked inode after acquisition (POSIX locks are process-owned). The live
browser lock is dropped only immediately before deliberate launch, while import
admission stays held through the spawn attempt. This handoff cannot atomically
reserve Firefox's own acquisition against an unrelated same-UID launcher.
Wipe/TTL admission uses the same locks and retries busy profiles; it removes an
owned `.parentlock` only after the profile's data mutations, never before them.
It never closes or kills Firefox. A launch failure is distinct from import
success.

`ImportSession`'s async Drop guard explicitly stops its blocking import; dropping
a Tokio JoinHandle alone would not cancel `spawn_blocking`. The **cancellation
point is one atomic commit-admission decision immediately before invoking the
service's durable ledger callback**, not the earlier profile exchange. If stop
wins that decision, the callback is never invoked: the worker rolls back the
predecessor (or removes a fresh generation) and never launches. A failed callback
also rolls back. If commit admission wins, that callback must finish even if the
RPC is dropped while it writes: success is an **operation committed with a lost
reply**, not a rollback promise. Cancellation observed before launch admission
suppresses that unstarted launch; it does not undo a durable import or stop a
launch already admitted. Inspect/wipe the import's durable ledger to resolve a
lost response. The blocking owner retains its upload mutex and removes its
staging upload even when the handler is dropped. Cancellation does not interrupt
in-progress filesystem syscalls: rollback is a bounded-generation rename/fsync
and quota-bounded staging cleanup when the worker reaches the decision. A
pre-commit unwind also restores its predecessor. This is not a crash-recovery
journal across a process/host kill in the
rename-to-ledger interval, nor a defense against hostile same-UID filesystem or
ptrace access. Pre-commit staging may be retained after an unclean process kill;
there is no automatic recovery/secure-erase claim.

Supported cookies are persistent, unpartitioned, default-container cookies;
SameSite is accepted only in Gecko’s lossless range `0..=2`; `-1` and other
unsupported values refuse the complete bundle before any destination mutation.
Session-only cookies explicitly refuse rather than becoming expired or
persistent cookies. localStorage accepts canonical HTTP(S) origins and lossless
Unicode strings. The qualified Firefox 159 LS schema is version 80 (5 << 4),
not an unversioned database. Saved logins need the **destination's own** NSS key;
no key yields an explicit not-sent notice. Primary Password or damaged key
state refuses. Both legacy 3DES and modern AES-256 SDR keys are supported; the
original Firefox importer/writer is unchanged.

## Hermetic regression gates

```sh
cargo test --manifest-path libs/cua-spacesd/Cargo.toml -p cua-spacesd-teleport --all-targets
cargo clippy --manifest-path libs/cua-spacesd/Cargo.toml -p cua-spacesd-teleport --all-targets -- -D warnings
cargo test --manifest-path libs/cua-spacesd/Cargo.toml -p cua-spacesd-server --test zen_rpc_cancellation
cargo test --manifest-path libs/cua-spacesd/Cargo.toml -p cua-spacesd-server --lib services::teleport::tests
cargo test --manifest-path libs/cua-spacesd/Cargo.toml -p cua-spacesd-server --test conformance
python3 -B -m unittest discover -s libs/cua-spacesd/crates/cua-spacesd-teleport/tests -p test_zen_provider_fixture.py
```

The process-lock regression uses a disposable child holding a real POSIX lock;
no browser, container or desktop starts in ordinary Cargo tests. Pure crypto
round trips supplement, but do not replace, actual NSS/browser readback.

## Opt-in Docker/browser gate (off by default)

Build the small explicit-home receiver probe, then choose an already-present
Spaces image by its exact config ID. The image must have Python 3 and Firefox.
The harness does not pull/build an image or change a live Space.

```sh
cargo build --manifest-path libs/cua-spacesd/Cargo.toml -p cua-spacesd-teleport --example zen-receiver
python3 libs/cua-spacesd/crates/cua-spacesd-teleport/tests/contained_zen.py \
  --allow-disposable-docker \
  --image sha256:<exact-local-image-id> \
  --receiver-bin <cargo-target>/debug/examples/zen-receiver \
  --evidence <new-private-evidence-directory>
```

The disposable non-root container is read-only, networkless, capability-free,
resource-bounded, with private tmpfs state and **no host bind mounts, published
ports, desktop/session bus or Docker socket**. Code is streamed into its private
writable tmpfs. SIGTERM/SIGINT and assertion failures dispose only its exact
allocated container ID, then verify absence. The caller retains evidence and
its pre-existing image. Use the repository/operator's resource serialization
wrapper around builds and this opt-in gate.

The fixture creates a new real Gecko source browser from scratch, logs in via
HTTP forms on two distinct loopback hosts, captures that browser's real cookie,
localStorage and tab state at the normalized consumer boundary, and imports it
using the public receiver. It is **not a test of the Zen source provider,
Keyvault, polkit presence, consent selection or a live `teleport push`**. Those
need their own sender/disposable-vault qualification; this harness never bypasses
them or touches an installed cua/daemon or an owner's profiles/vault.

Assertions cover both guest `/account` pages, browser-read localStorage,
authenticated HTTP callbacks, two real guest tabs, actual PNG screenshots,
selected-first-site exclusion and tabs-only exclusion. Guest page JavaScript
never creates the transferred storage value. A saved-login case creates a real
independent destination NSS key and reads the imported password back through
Firefox's login service, without copying source key material. Generated
synthetic logins are fixtures, not an OS/Keyvault authentication bypass.
`result.json`, per-case callbacks/states/capabilities, six PNGs, and
`cleanup.json` are evidence. Failed attempts remain failed receipts.

## Opt-in real retained Zen provider → Firefox gate

To exercise the **source provider**, supply the separately approved retained
synthetic Zen profile, its original browser receipt and closed-profile SHA-256
inventory. This lane is deliberately restricted to an explicit profile below
`TMPDIR`; it never discovers an owner's Zen roots. The receipt must identify a
real `browserName=zen`, Linux/headless fixture whose browser capability profile
matches the explicit selected profile. It requires the exact seven named browser
checks and exact three cleanup checks, each literally JSON boolean `true` (not
truthy values or vacuous empty dictionaries), clean integer exit code zero, and
consistent two-host HTTP/account/tab/storage/saved-login evidence. Missing,
additional, nonboolean or wrong-profile evidence refuses **before inventory or
copy**. This validates consistency of the explicitly approved receipt, not its
cryptographic authenticity or permission to discover another profile. Every
retained file is checked against its original hash before copying.

```sh
# Run builds and the browser gate under the operator's native.lock and bounded
# systemd scope (MemoryMax=48G, MemorySwapMax=0, nice 10). Use an owned target.
export CARGO_TARGET_DIR=<owned-cargo-target>
cargo build --offline --manifest-path libs/cua/Cargo.toml \
  -p cua-teleport --example zen-export
cargo build --offline --manifest-path libs/cua-spacesd/Cargo.toml \
  -p cua-spacesd-teleport --example zen-receiver
python3 -B libs/cua-spacesd/crates/cua-spacesd-teleport/tests/contained_zen.py \
  --allow-disposable-docker \
  --image sha256:<exact-local-image-id> \
  --receiver-bin "$CARGO_TARGET_DIR/debug/examples/zen-receiver" \
  --exporter-bin "$CARGO_TARGET_DIR/debug/examples/zen-export" \
  --source-profile <approved-retained-scratch-home>/.zen/Profiles/<synthetic-profile> \
  --source-receipt <original-fixture-evidence>/fixture-result.json \
  --source-sha256 <original-fixture-evidence>/closed-profile-sha256.json \
  --require-saved-login \
  --evidence <new-private-evidence-directory>
```

The two source tabs must be the fixture's ordinary `/account` pages at
`localhost` and `127.0.0.1` on the same port. The guest hosts the oracle at that
**same port inside its private network namespace**, never at a host listener.
The retained cookies must still be persistent and unexpired. Expiry is a real
failure; regenerate an authorized real Zen fixture rather than modifying expiry,
nonce, cookie, storage, login or tab data to manufacture a pass.

`zen_provider_fixture.py` makes a new offline `.zen/Profiles/retained.synthetic`
copy and writes its own minimal profile inventory/explicit synthetic marker.
The ordinary empty `.parentlock` file is preserved in the offline copy: the
provider queries the actual POSIX lock and accepts a cleanly exited profile.
Only root `lock` and `parent.lock` markers are omitted **from the new offline
copy**, based on the original clean-exit receipt; original locks are never
modified. Other symlinks refuse. Held POSIX locks and stale symlink locks refuse
with `zen_profile_locked`.
The real resolver/closed-profile/snapshot/format parsers run unchanged. Original
and staged regular-file bytes and symlink inventories must remain identical
across provider capture; the harness never opens source SQLite itself.

`zen-export` uses `ZenProvider::manifest` and **the public raw
`ExportProvider::capture_selected` format seam** with a supplied-home descriptor
that denies every process, DevTools and authorization effect. It does not call
`export_selected`, authorize anything, grant fake presence, use a live daemon or
claim a Keyvault/polkit push. This is source-format-to-receiver integration,
not qualification of the gated export/approval path. No product authorization
switch is set, and source NSS keys never enter a bundle or the container.

The full session case directly imports provider-emitted cookies, localStorage
and normalized tabs. Tabs-only and full-scope defaults are separate real
provider captures and must contain no credential-bearing entries. The first-site
negative uses the shared typed `BundleReader`/`BundleWriter` to retain **only
existing captured** first-host cookies and first-origin localStorage, while
keeping both real tabs as recipient oracles. It does not synthesize browser
records or prove provider/broker site-filter authorization: capture is broad,
then the harness performs this explicitly labelled transport-boundary subset.
CLI/MCP/SDK selection and reviewed source-site access remain separate gates.

The guest (`zen_provider_browser.py`) imports only those actual bundles through
the public receiver. It checks persistent/HttpOnly/SameSite cookie semantics,
browser-read localStorage, both real browser tab URLs, independent authenticated
HTTP callbacks, saved-password absence in the negative cases, eight actual PNG
screenshots, wipe behavior and cleanup. The guest page never seeds storage and
never calls `/login`. Saved-login capture is a separate real provider operation;
when successful it is imported after Firefox creates its **independent** NSS
key, then read back with Firefox's login service. `--require-saved-login` makes
any source decode failure or missing destination readback fail the gate. Without
that flag, a decode failure is preserved in `saved-login-capture.json`, never
replaced by plaintext from the old receipt or counted as a saved-login pass.

Evidence adds `source-integrity.json`, provider stdout/stderr, source manifest,
actual private bundle tars, source and destination capabilities, per-case
browser/callback receipts, and exact-ID `cleanup.json`. Bundles and the offline
fixture copy contain synthetic secrets and stay in private evidence. No release,
installed CLI, daemon, owner profile, vault, live agent or remote ref changes.
SIGTERM/SIGINT disposal also recovers an interrupted allocation by its unique
name **and matching ownership label**, then removes/inspects its exact ID.

For a deliberate selection-fault control, wrap the real exporter in a private
scratch executable that, after successful real capture, replaces
`selected-first.tar` with the unchanged real `full.tar`. The ordinary guest must
fail on the second site's signed-in state and still verify container absence.
For cancellation, send SIGTERM to the harness's exact child PID after its private
`container.json` exists; require a nonzero exit, `cancelled by signal 15`, and
`container_absent:true` for the recorded ID. These controls do not change product
source or fabricate records. Keep their failed receipts separate from a final
unchanged positive run.
