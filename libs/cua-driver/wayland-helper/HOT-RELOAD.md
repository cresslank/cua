# Stable WinRects loader ABI 1

GNOME caches the manager-facing `extension.js` for the entire login. The root
bootstrap stays unchanged across implementation updates. It does **not** call
GNOME's private `reloadExtension`, report implementation failures as Shell
ERROR, or return a promise from `enable()`. GNOME stays ACTIVE while loader
status reports `loading`, `ready`, `retiring`, `error`, or `disabled`.

This is GNOME-specific; other compositor and OS adapters are unchanged. Only
GNOME Shell 50.6 has native hot-swap qualification here. The declared older
Shell versions retain their existing API compatibility, not a claim of tested
hot swapping on those versions.

## Source and installed layouts

The readable source is `winrects@cua/impl/{main,policy}.js`. Do not install that
source directory directly. Build an extension tree with:

```sh
python3 package.py /owned/staging/winrects@cua
# For a zip, after packaging:
gnome-extensions pack --extra-source=impl --extra-source=selected.json /owned/staging/winrects@cua
```

`package.py` is a deterministic, offline packager, not a maintenance installer.
It refuses an existing output directory and creates new runtime directories
as 0700 and files as 0600, independent of the caller's umask (restored on exit).
It never changes an already-installed selection or closure. The installed tree is:

```text
winrects@cua/
  extension.js                    # stable bootstrap, loader ABI 1
  metadata.json                   # bootstrap release, not implementation version
  selected.json                   # one atomic selection document
  impl/<64-lowercase-hex>/
    manifest.json
    main.js
    policy.js                     # all relative imports/assets in this closure
```

The closure digest is SHA-256 of the **exact UTF-8 manifest bytes**, including
any final newline. The packager emits sorted-key compact JSON plus LF; readers
hash bytes, not reserialized JSON. Its manifest is:

```json
{"entrypoint":"main.js","files":[{"path":"main.js","sha256":"<sha256-of-file-bytes>","size":123}],"helper_api":16,"loader_abi":1,"protocol":4}
```

Each `files` element has exactly `path`, `sha256`, `size`; the manifest has
exactly the illustrated keys. Paths are relative ASCII segments containing
letters/digits/underscore/hyphen with a single filename extension. No absolute
paths, dot segments, symlinks, unexpected entry types, duplicate or unlisted
files, or empty/unlisted directories. `manifest.json` is not listed in itself.
Budget: 64 files, 2 MiB total, 32 KiB manifest, 4 KiB selector. Every path segment,
file owner (current UID), type and group/other write bits is checked without
following a final symlink; every member size and digest is verified. The root
is GNOME's already-resolved extension directory. These checks are integrity
and accidental-path safeguards, **not** a sandbox against the owning user.
Same-UID code can mutate both files and the compositor. The installer must
publish immutable directories and never edit an imported closure in place.

The selector has exactly these keys:

```json
{"loader_abi":1,"selected":"<manifest-sha256>","previous":null}
```

`previous` is null for initial installation, otherwise a lowercase SHA-256.
It is durable rollback bookkeeping, not an instruction for the loader to
silently fall back. Both fields are validated; only `selected` is loaded.
Current ABI admits helper API >=16 and protocol exactly 4. Runtime capabilities
remain the driver's authority; metadata/version alone never grants input.

## Bounded control API

Session-bus name and interface: `org.cua.WinRects.Loader`.
Object: `/org/cua/WinRects/Loader`. Runs inside the same Shell process as the
helper; a maintenance client must attest that owner just as the driver attests
`org.cua.WinRects`. Bus ownership alone is not code provenance.

- `GetStatus() -> s`: bounded JSON with `loader_abi`, `selected_digest`,
  `loaded_digest`, `state`, `epoch`, `last_error` (at most 512 characters), and
  `generation`, and `quarantined` (true when the actual raw-input lease is
  retained). Digests/epoch are null when unavailable. During retirement the
  old loaded digest may remain visible, but is **not ready authority**.
- `ReloadSelected() -> s`: no arguments, code path, URI, or automatic replay.
  Requires enabled bootstrap, unlocked `user` session, no operation in progress,
  and a freshly validated selection. **Swaps are idle-only.** Any foreground
  transaction gets `.Busy` / `foreground_transaction_open` without abort,
  restoration, epoch change or loss of the healthy implementation. An open
  canonical raw-input window gets `.Busy` / `raw_input_window_open`.
  Validation precedes retirement, so a bad selection does not discard a
  healthy implementation. Concurrent requests
  receive `org.cua.WinRects.Loader.Busy`; disabled/locked/poisoned retirement
  gets `.Unavailable`; invalid selector/closure gets `.InvalidSelection`.
  Successful RPC completion returns status, which can still be `error`: the
  client **must** require `ready`, expected selected/loaded digest, compatible
  helper capabilities and a fresh matching epoch.

Hot reload requires util-linux `flock` on GNOME Shell's PATH. Before retirement,
the bootstrap opens and validates the driver's canonical
`/run/user/<effective-uid>/cua-driver-gnome-raw-input.lock` (not HOME, TMPDIR or
XDG_RUNTIME_DIR), with its 0700 parent and 0600, current-UID, single-link regular
file/inode checks. A bounded, nonblocking `flock` child locks a duplicated **same
open file description** passed as FD 3, then exits. The original Shell-owned
Gio stream retains the actual lease across all retirement/import awaits; the
launcher and temporary descriptors close immediately. New cooperative raw-input
windows cannot open during that interval. No child is left holding a reopened
path. Missing/invalid locking prerequisites fail unavailable, not unlocked.
A foreground transaction appearing during acquisition refuses before retirement;
a transaction/uncertainty observed after retirement begins poisons admission.

Import and retirement each have a five-second deadline; the nonblocking lock
child has a one-second deadline. An import timeout
fences its late result; native module evaluation cannot be cancelled. Module
scope must therefore acquire **no persistent resources**. A synchronous loop,
crash or memory exhaustion inside imported code cannot be preempted by JS
promise deadlines and can still break Shell.

Retirement failure/timeout poisons the bootstrap for that login. No successor
or rollback runtime is admitted, even if retirement later finishes. The helper
is unavailable; status retains the error and `quarantined` flag. The actual
canonical raw-input lease is retained until Shell exits; disable/re-enable does
not release it. Recover through logout, not a retry
of an uncertain native input action. Ordinary import/enable errors with proven
retirement leave the bootstrap ACTIVE and can be recovered by an explicit
selector rollback followed by `ReloadSelected`.

## Runtime retirement contract

`main.js` exports an ordinary synchronous-constructible runtime (not another
GNOME Extension). Constructor receives `{root, implementationDir, digest}`;
`enable()` is synchronous and allocates a fresh epoch. Only the bootstrap does
once-per-login GI promisification. All relative dependencies/assets must stay
inside the content-addressed directory. No mutable `current` URI or query-string
cache busting is permitted.

`stopAdmission()` is synchronous and idempotent. It invalidates the activation
generation, refuses all pending exported invocations with
`org.cua.WinRects.Retired` / `helper_retired`, removes exported helper service,
bus ownership/subscription, ordinary GLib sources/signals and all cursor actors.
The once-only invocation wrapper suppresses double/late replies. Resource
ownership begins at allocation, including a partially constructed overlay or
partially failed enable. Destroying actors also removes transitions and chrome.

`reloadBlockedReason()` is a read-only idle predicate: any foreground record
(including activating, restoring, reconciling or uncertain state) refuses a
maintenance request. `retire()` is idempotent and returns a promise. It closes
admission and checks this predicate before draining capture continuations and
again afterwards. It **never calls foreground End/Abort/restore to clear the
way for a swap**. A late transaction or uncertain cleanup fails closed. Native
capture continuations must drain before a successor starts; every capture await
checks generation before observing or publishing anything. Visibility idles and
ordinary polling are generation-fenced and tracked. Existing foreground method
behavior, diagnostic errors and optional refusal-reasons capability remain
unchanged; their cleanup timers are tracked, not redesigned.
Cleanup attempts every resource even if one removal throws, then fails the
retirement rather than certifying an uncertain cleanup. A timed-out capture
can finish later only into its fenced refusal; it never opens successor
admission. The helper does not itself inject native input. Query/End/Abort
reporting `terminal`, an EIS sync, or session close is **not proof that queued
native input was consumed**. This loader adds no consumption capability and
does not enable foreground native right/double click. Idle raw-lease exclusion
is not advertised as a native consumption acknowledgement; any known
uncertainty keeps the helper unavailable and the held lease quarantined.

Old target IDs include the old epoch and are refused as stale. Driver public
IDs hash that full target; `available()` already rediscovers capabilities.
No exact-version coupling, target remapping, or automatic action retry is
added. Clients reacquire discovery and a screenshot before a later action.

## Installer handoff: transaction required

`install.sh` remains a **first-bootstrap, explicit** rollout command. It packages
and atomically replaces a pre-bootstrap extension, never downgrades a newer
metadata release, and never replaces an existing stable bootstrap, selector or
retained closures. The binary updater still must not mutate the helper.
A follow-up managed installer owns routine updates; it must:

1. Pin reviewed source commit/tree, root bootstrap hash/ABI, complete closure
   digest, helper API/protocol, and expected old selected/previous values.
2. Under an installer transaction lock, validate anchored owner/types/modes,
   budgets, all hashes and exact inventory; stage privately, fsync files and
   directories, atomically publish `impl/<digest>`, fsync parent. Refuse any
   existing digest whose complete bytes differ. Never overwrite a closure.
3. Journal expected predecessor and candidate; compare expected selector,
   atomically replace/fsync `selected.json` with selected=candidate and
   previous=known-good. Do not replace root extension or metadata routinely.
4. Call `ReloadSelected` with finite timeout and read back status AND helper
   capabilities. Require exact digest, `ready`, fresh epoch and protocol. File
   success or a bus name is not activation success.
5. For an import/enable failure, compare-and-swap selection back to the saved
   predecessor, call ReloadSelected, and verify a fresh predecessor runtime.
   Selector rollback alone does not retire running code. For retirement
   poison, leave unavailable and require logout; do not retry native input.
6. Recover interrupted journals explicitly; retain selected, previous, active
   and delayed-import referenced closures. Coordinate all installers sharing
   UUID `winrects@cua`; no automatic whole-tree replacement/pruning.

### First deployment while the user is already logged out

If the user is already at GDM, install **before their next login**: that login
loads the bootstrap and no additional logout is necessary. Do not toggle the
extension, run `--enable`, call gsettings, or change the enabled-extensions set.
The greeter's Shell is not the user's extension session. A parent/operator
first pins the merged reviewed commit and the reported bootstrap/closure hashes,
then runs (substitute the reviewed hash values, not values read from the target):

```sh
set -euo pipefail
cd /home/lantzk/Development/cua
EXPECTED_SOURCE_COMMIT='<approved source commit>'
test "$(git rev-parse HEAD)" = "$EXPECTED_SOURCE_COMMIT"
test -z "$(git status --porcelain)"
EXPECTED_BOOTSTRAP_SHA='<reviewed extension.js SHA-256>'
EXPECTED_CLOSURE_SHA='<reviewed manifest SHA-256>'
STAGE=$(mktemp -d /home/lantzk/.hermes/cache/scratch/winrects-first-login.XXXXXX)
HELPER="$PWD/libs/cua-driver/wayland-helper"
DEST=/home/lantzk/.local/share/gnome-shell/extensions/winrects@cua
test ! -e "$DEST/selected.json" # Existing bootstrap needs its managed transaction.
python3 "$HELPER/package.py" "$STAGE/winrects@cua" > "$STAGE/closure-sha256"
test "$(cat "$STAGE/closure-sha256")" = "$EXPECTED_CLOSURE_SHA"
printf '%s  %s\n' "$EXPECTED_BOOTSTRAP_SHA" "$STAGE/winrects@cua/extension.js" | sha256sum --check
# No --enable: stage-only install never reads or writes enabled-extensions.
env XDG_DATA_HOME=/home/lantzk/.local/share bash "$HELPER/install.sh"
python3 - "$STAGE/winrects@cua" "$DEST" <<'PY'
import hashlib, json, os, stat, sys
from pathlib import Path
expected, installed = map(Path, sys.argv[1:])
files = {p.relative_to(expected) for p in expected.rglob('*') if p.is_file()}
entries = {p.relative_to(expected) for p in expected.rglob('*')}
actual, actual_entries = set(), set()
for p in [installed, *installed.rglob('*')]:
    s = p.lstat()
    assert s.st_uid == os.geteuid() and not stat.S_ISLNK(s.st_mode), p
    assert stat.S_ISDIR(s.st_mode) or stat.S_ISREG(s.st_mode), p
    assert not (s.st_mode & 0o022), p
    if p != installed: actual_entries.add(p.relative_to(installed))
    if stat.S_ISREG(s.st_mode): actual.add(p.relative_to(installed))
assert actual == files and actual_entries == entries, 'installed closure inventory differs'
for name in files:
    assert (installed/name).read_bytes() == (expected/name).read_bytes(), name
selection = json.loads((installed/'selected.json').read_bytes())
closure = installed/'impl'/selection['selected']
manifest_bytes = (closure/'manifest.json').read_bytes()
assert hashlib.sha256(manifest_bytes).hexdigest() == selection['selected']
for item in json.loads(manifest_bytes)['files']:
    data = (closure/item['path']).read_bytes()
    assert len(data) == item['size'] and hashlib.sha256(data).hexdigest() == item['sha256']
print('HASH-VERIFIED FIRST-LOGIN BOOTSTRAP', selection['selected'])
PY
```

The install command copies the bootstrap, metadata, selector and complete
`impl/<digest>` closure atomically; readback compares all bytes, hashes,
ownership, permissions and inventory. It does not change settings. A newer
installed metadata release or an existing stable bootstrap is deliberately kept;
if it differs, verification fails instead of silently claiming installation.
After the user's next login, require GNOME ACTIVE, loader `ready`, matching
selected/loaded digest, fresh matching helper epoch and protocol/capabilities.
Do not ask for another logout just to activate this first bootstrap if it was
staged before that login.

### Routine operator calls

If replacing the cached monolith while the user **is** logged in, the first
bootstrap still requires one separately approved logout/login. No deployment is
performed by this PR.

After an approved transaction publishes a new selection, routine activation
needs **no logout and no GNOME extension toggle**:

```sh
gdbus call --session --dest org.cua.WinRects.Loader --object-path /org/cua/WinRects/Loader --method org.cua.WinRects.Loader.ReloadSelected
gdbus call --session --dest org.cua.WinRects.Loader --object-path /org/cua/WinRects/Loader --method org.cua.WinRects.Loader.GetStatus
```

Changing bootstrap bytes, incompatible ABI, or uncertain retirement can still
require logout. ES modules stay cached for the entire login: disk pruning does
not unload JS. Bound update frequency/retained versions, monitor long-session
memory, and retain logout as the escape hatch. See
[`tests/hotreload_README.md`](tests/hotreload_README.md) for the private native
qualification and its memory/receiver evidence limitations.
