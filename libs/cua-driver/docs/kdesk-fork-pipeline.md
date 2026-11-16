# Fork package pipeline

This additive, Linux x86_64 lane is dormant. It never changes inherited upstream
workflows and has no hosted-runner fallback. Do not run its build or installer on
the shared workstation. The installer is confined to a disposable private VM.

## Control and dispatch

Both workflows must land on fork `main` before dispatch registration can work.
Only `cresslank`, also the triggering actor on reruns, can enter `private-vm`.
`CI_RUNNER=private-vm`, `KDESK_DRIVER_PIPELINE_ENABLED=true` and
`KDESK_DRIVER_BUILD_IMAGE_DIGEST=sha256:<64 hex>` must be separately configured by
the owner after the ordinary per-repository ARC set and reviewed image are ready.
The image digest must be the digest pinned in that runner's template; this value
is an administrative assertion, not runtime discovery of the Kata container.
First-run assignment/image readback must establish that correspondence.

Prepare at the reviewed control commit on `main`. Both workflows take one JSON
`request` string, with exactly these string fields:

```json
{
  "request_key": "unique-request-key-0001",
  "upstream_tag": "cua-driver-rs-v0.52.8",
  "upstream_oid": "9b5c3b7f42094f483bd286f2aa543dc691eafd33",
  "policy_oid": "<full reviewed control commit>",
  "carry_digest": "<SHA256 of canonical carry definition>",
  "build_image_digest": "sha256:<reviewed runner image digest>"
}
```

`carry_digest` hashes compact sorted-key JSON containing `carry_base`,
`carry_head`, and `carry` from `.github/kdesk-cua-driver-policy.json`. The carry
is the recorded `local-current` series, not fork main. Its actual head ends
`32526a6`; the transcribed `32525a6` is not a Git object.

The prepare CLI verifies the published component release/tag and the full carry
patch digests. A real `git rebase --onto` preserves original authors, refuses
conflicts/empty or dropped carries, and records original/rebased patches, patch
IDs, trees and range-diff. It disables hooks, rerere and automatic conflict
repair. The separate deterministic overlay copies only the policy allowlist and
applies the reviewed feature patch; existing overlay files or patch conflicts
hold. Upstream 0.52.8 moved Linux get_config from `impl_.rs` into `get_config.rs`,
so the overlay patch targets that release layout. Fork main's additive query is
in its older implementation location.

The source tag binds full upstream, carry and policy identities. Tags are never
force-updated. A repeated construction must produce the identical source commit.
Exit 78 means owner hold; bounded failure/conflict artifacts expire after 14 days.
An owner must repair the reviewed carry/policy, not resolve a conflict in CI.

After success, an owner-authenticated caller explicitly dispatches
`kdesk-cua-driver-build.yml` at the returned source tag using the same request.
No token-pushed-tag trigger is assumed. `github.sha` is reconstructed and checked
against the checkout compiled. The build has contents-read only, no OIDC or
publication authority, isolated installer HOME/XDG/Cargo state, four Cargo jobs,
and no persistent compilation cache. Rust must already be the exact numeric
policy/toolchain pin; missing/new toolchains hold rather than install stable.

## Package and publication

The real local installer builds release mode through one narrow Cargo wrapper
adding `--locked --features cua-driver/portal-input`. Its single complete release
directory is archived, not its absolute selector symlinks. Roster equality checks
both binaries, every skill and every Wayland-helper source member. Unexpected
members, links, special files and incomplete layouts hold.

The manifest records per-member path/type/mode/size/SHA256, source tree,
carry/overlay evidence, toolchain/features, configured image digest, run/attempt
and workflow identity, binary hashes and packaged smoke. SHA256SUMS covers all
public package/evidence files except itself and the three Sigstore bundles
(the checksums are themselves attested, avoiding circular bundle hashes).

A fresh publish job checks out only the reviewed control commit, downloads the
exact successful build artifact ID, hard-checks the transfer ZIP digest and
validates it as bounded data. It never executes package members or candidate
scripts. Only this job has contents-write, OIDC and attestations-write.
Full-SHA-pinned Actions attest the tarball, manifest and checksums; their bundles
are retained as release assets and GitHub attestations. Publication creates a
draft, uploads without clobber, reads back every asset name/size/GitHub digest,
then publishes. An existing complete byte-identical release returns its identity;
conflicting bytes, duplicates, partial drafts and incomplete releases hold for
owner inspection. A rerun that changes attempt evidence or signature bundles is
not permission to overwrite an earlier release. Consumers must independently
verify source construction, asset IDs/digests and exact source/workflow provenance.

## Qualification boundaries

Offline, without Cargo:

```sh
nice -n 19 python3 -m pytest .github/scripts/tests/test_kdesk_cua_pipeline.py -q
nice -n 19 actionlint .github/workflows/kdesk-cua-driver-{prepare,build}.yml
```

The fork has no `bin/ci --quick`; its equivalent script gate is the Python tests
in `.github/scripts/tests` and installer-script tests. New Action tags and their
full commit objects must be independently resolved before delivery.

Only inside the private VM: locked Linux driver/contract/core/testkit/platform,
cursor-overlay and cursor-theme tests, including the carried HTTP/protobuf
exporter tests; feature-enabled and disabled binary checks; real installer
output; ELF dependency inventory; and public packaged MCP smoke. GUI/uinput
ignored tests are not desktop qualification and are not enabled by this lane.
The disabled binary must deliberately fail the enabled-feature smoke before its
correct disabled assertion passes. Offline carry/asset corruption fixtures prove
refusal, not a live compiler or exporter run.

The smoke starts only its own daemon at explicit private socket/pid paths, with
no overlay or automation prompts, no inherited desktop/session bus/profile,
isolated HOME and all XDG paths, and vendor telemetry/update checks disabled.
It calls only public MCP `get_config`, decodes real HTTP/protobuf tool records
at a disposable loopback collector, checks source/features, event/outcome,
timing/correlation and privacy, and proves no export without opt-in or with SDK
disabled. A bound non-listening loopback socket tests collector outage. Cleanup
signals only owned child PIDs. This proves backend compilation, not portal
consent, screen capture or physical input.

No local test proves GitHub dispatch registration/token permissions, actual ARC
assignment/image identity, build resources, target ABI, release upload or the
self-hosted tag-dispatch attestation certificate/source fields. Those require the
first live owner-authorized run after runner/image setup. Failed provenance must
be repaired, never weakened. Merge does not authorize activation, installation,
unit changes, workflow dispatch, runner enrollment or repository settings changes.
