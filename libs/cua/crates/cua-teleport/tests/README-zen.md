# Zen → Firefox session migration (Linux)

`zen`, `zen-browser` and `app.zen_browser.zen` identify **Zen**, never Firefox.
The destination is the Space's installed Firefox, in the dedicated owned
`.mozilla/firefox/Profiles/cua.zen-import` profile. This does not install Zen or
restore Zen workspaces, extensions, preferences, history, arbitrary IndexedDB,
containers or partitioned identities. The current Linux image recipe already
installs Firefox and disables its telemetry (`libs/images/linux/Dockerfile`);
no browser package or image recipe change is needed.

## Selection and refusal contract

The resolver reads only native `.zen` and Flatpak
`.var/app/app.zen_browser.zen/.zen` under the supplied host home. `profiles.ini`
and `installs.ini`, retained no-follow directory FDs and the exact `--profile`
selector are shared by manifest, inventory, readiness and capture. Multiple
profiles/install defaults require an explicit name or contained path. Absolute
selectors cannot escape the Zen root. No fallback to a Firefox profile occurs.

Close the selected source browser. Active POSIX `.parentlock` locks, other
lock markers, and stale/dangling lock symlinks refuse with
`zen_profile_locked`; locks are never removed. Gecko's ordinary empty regular
`.parentlock` file survives clean exit and is accepted only when its POSIX lock
is free. No source SQLite database is opened: main/WAL/journal components are
copied privately and revalidated (including inode, ctime and component-set
changes); only the validated private copy is recovered/read. Capture retries
are bounded. Per-database snapshots do not promise atomic cross-database state.

| Manifest key | Default | Wire entry |
| --- | --- | --- |
| `zen/tabs.json` | checked | normalized HTTP(S) current URL list |
| `zen/cookies.sqlite` | **off**, sign-ins group | `cookies.json` |
| `zen/storage` | **off**, site-storage group | `localstorage.json` |
| `zen/logins.json` | **off**, saved-password group | `logins.json` |

Full scope means supported categories, **not** raw-profile copy. Its defaults
are tabs only. Cookies, credential-bearing localStorage and saved passwords
need distinct explicit opt-ins. Unknown/raw keys refuse. Saved passwords also
require the existing second confirmation and a presence prompt that names
saved passwords. Source `key4.db`, prefs, raw sessionstore and container files
never travel. The source supports modern AES and legacy NSS logins; Primary
Password refuses, without guessing or prompting for it.

Host/origin selectors stay exact, including scheme and port for origin-bearing
state; they are not registrable-domain or child/sibling grants. Zen identity
survives structured Keyvault records, upserts, selectors and retries. Old guests
or guests missing the exact `teleport.zen` capability attributes
(`format=normalized-gecko-v1`, `destination=firefox`, `platform=linux`) refuse
before presence/capture. A fresh manifest must also report an installed
Firefox destination. Unregistered providers are unsupported, never ready.

`zen_profile_ambiguous`, `zen_profile_unsafe`, `zen_profile_not_found`,
`zen_source_busy`, `zen_format_invalid`, `zen_format_unsupported`,
`zen_primary_password`, `zen_selection_invalid`, `zen_platform_unsupported`
and `zen_source_io` are stable provider-string refusal prefixes. The CLI
refuses `--read-as-caller` for Zen with `zen_keyvault_required`; direct core/SDK
`send` refuses too. Use `send_through_keyvault` for SDK delivery. Selecting
`zen/logins.json` or `--all` is not the separate password confirmation: CLI
callers must also supply `--confirm-passwords`, and SDK callers must set
`TeleportOptions.confirm_saved_passwords=true`. Linux uses the shipped
`ai.cua.spaces.keyvault.passwords` action, whose native prompt explicitly names
saved passwords; older/missing policy fails closed.
No trust, sensitive-export or presence override was added or used.

Session-only cookies and container/partition state currently refuse rather
than silently becoming persistent/unpartitioned state. Closed-browser capture
cannot recover cookies that the browser never persisted. A fresh destination
without its own NSS key reports saved passwords **not sent**. Launch Firefox
once in the dedicated imported profile to create its independent key, close
that exact destination browser, then repeat an explicitly approved password
transfer. A not-sent notice is not a successful password-delivery claim.

## Hermetic and browser gates

Ordinary tests use temporary synthetic homes and actual SQLite files, not the
operator's profile. The test-host effects guard remains intact; a real POSIX
lock-holder child belongs to `tests/zen_lock.rs`, not the provider's source.
No ordinary Cargo test starts a VM, container or desktop.

```sh
cargo test --locked --manifest-path libs/cua/Cargo.toml \
  -p cua-teleport -p cua-teleport-bundle -p cua-keyvault
cargo test --locked --manifest-path libs/cua/Cargo.toml \
  -p cua-spaces-ext --test keyvault_zen --test keyvault_items --test keyvault_delivery
```

The explicit, disposable browser harness and its cleanup/negative controls are
in `libs/cua-spacesd/crates/cua-spacesd-teleport/tests/README-zen.md`. Its retained
real **synthetic** Zen profile is hash-checked, copied only into a new private
offline home, captured by the real public provider and imported by the real
receiver. Browser-read cookies/localStorage, authenticated local HTTP callbacks,
tab URLs, actual PNG screenshots and independent-destination NSS saved-password
readback are the oracle. No payload is manufactured from the old receipt.

That lane qualifies the source-format/normalized-consumer boundary, **not** a
Keyvault/polkit push. The first-site transport subset is labelled as such;
separate public broker tests exercise source-site authorization. Sensitive
bundles are plaintext inside the protected transport boundary and private
scratch directory; hashes are not encryption. Temporary files are unlinked,
not claimed forensically erased. Same-UID hostile filesystem/ptrace attacks
and process/host-kill crash recovery between profile rename and durable ledger
publication remain outside this implementation's guarantees.

## Owner-operated live Keyvault qualification (not executed automatically)

Do this only in a separately approved **disposable test host/account and Space**,
not the installed host daemon, personal Keyvault or `cua-spike-agent`. The test
host must have the candidate CLI installed under the existing root-owned trust
contract, the shipped polkit action, a real native authentication agent and its
own fresh home. An untrusted build-tree CLI is expected to refuse; do not set
unverified-daemon, sensitive-export or skip-auth overrides to get past that.
Preparing that trusted installation/native session is an owner prerequisite,
not an installation performed by this PR.

Inside that isolated test host, `CUA` below must name its trusted candidate
binary. Put only an approved, cleanly closed synthetic profile under that test
account's `.zen`, and register only the disposable receiver as `zen-test` using
the normal authenticated Space/sandbox registration flow. The guest must run
the candidate receiver and its exact capabilities must be inspected first.

```sh
# This shell, HOME, daemon, vault and receiver all belong to the disposable lab.
export DO_NOT_TRACK=1 CUA_TELEMETRY=0 CUA_SPACESD_TELEMETRY=0
export CUA_HOME="$HOME/.cua"
CUA=/usr/local/lib/cua-spaces/cua
"$CUA" daemon start
"$CUA" keyvault init --passphrase
# Owner types the throwaway passphrase and completes the real native prompt.
"$CUA" teleport doctor --app zen --profile synthetic
"$CUA" teleport manifest --app zen --profile synthetic --scope tabs
"$CUA" teleport push --app zen --profile synthetic --sandbox zen-test --scope tabs
"$CUA" teleport push --app zen --profile synthetic --sandbox zen-test --scope full \
  --include zen/tabs.json --include zen/cookies.sqlite --include zen/storage
# Named includes are explicit selection, NEVER a replacement for native presence.
```

Record the exact candidate OID/binary hash and guest identity; approve only the
native prompts naming that disposable vault, synthetic profile and Space.
Before repeating, close only the dedicated guest profile through the lab's
normal lifecycle (do not kill unrelated Firefox). Separately test missing
presence, wrong-password/cancel, untrusted CLI and missing policy refusals;
confirm no source capture/destination change on each. Then qualify first-site
source filtering via the Keyvault site flow, tabs-only/default exclusions,
MCP/SDK retries, optional saved-password confirmation (`--confirm-passwords`
plus `--include zen/logins.json`, only after separate approval), recipient browser
readback and ledger wipe. Stop/remove exact owned lab resources and verify
absence. None of these human-authentication results are implied by the
non-interactive tests or browser harness.
