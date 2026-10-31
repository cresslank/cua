"""Pin owner-only fork routing, hosted fallbacks and release-call isolation."""
from pathlib import Path
import re
import runpy
import unittest

import yaml

expressions = runpy.run_path(str(Path(__file__).with_name("workflow_expressions.py")))
evaluate = expressions["evaluate"]
macos_cases = expressions["macos_cases"]

ROOT = Path(__file__).resolve().parents[3]
JOBS = {'ci-check-docs/native-reference': '${{ matrix.os }}', 'ci-cua-driver-bench/harness': 'macos-26', 'ci-cua-driver-contract-clients/mcp-client-discovery': '${{ matrix.os }}', 'ci-cua-driver-contract-clients/runtime-parity': '${{ matrix.os }}', 'ci-cua-driver-installer-compat/install': '${{ matrix.os }}', 'ci-cua-s1/macos-unit': 'macos-latest', 'ci-cua-sdk/rust': '${{ matrix.os }}', 'ci-cua-sdk/rust-lint': '${{ matrix.os }}', 'ci-cua-sdk/spaces-e2e': '${{ matrix.os }}', 'ci-cua-sdk/bindings': '${{ matrix.os }}', 'ci-cua-sdk/python': '${{ matrix.os }}', 'ci-cua-sdk/node': '${{ matrix.os }}', 'ci-cua-sdk/swift': 'macos-26', 'ci-cua-spaces-macos/ffi': 'macos-26', 'ci-cua-spaces-macos/swift-tests': 'macos-26', 'ci-cua-spaces-macos/bindings': 'macos-26', 'ci-cua-spaces-macos/app-bundle': 'macos-26', 'ci-cua-spaces/bundle': '${{ matrix.os }}', 'ci-cua-spacesd/test': '${{ matrix.os }}', 'ci-infinite-canvas-swift/swift': 'macos-26', 'ci-jev-use/released-driver-macos': 'macos-26', 'ci-jev-use/native-appkit-macos': 'macos-26', 'ci-openkoalabot-example-samples/swift': 'macos-26', 'ci-platform-linux-portability/platform-linux-off-linux': 'macos-latest', 'ci-rust-format/format': '${{ matrix.os }}', 'ci-rust-macos/unit': 'macos-latest', 'ci-swift-linker/link': 'macos-latest', 'ci-test-scripts/macos-uninstall': 'macos-26', 'e2e-rust-macos/probe': 'macos-26', 'e2e-rust-macos/matrix': 'macos-26'}
REUSABLE = {"ci-cua-driver-installer-compat", "e2e-rust-macos"}
UPSTREAM_ONLY_JOBS = {"ci-cua-microvm/macos", "ci-cua-spaces-rust-slice/slice", "e2b-bindings/swift"}
UPSTREAM_ONLY_MATRICES = {"ci-cua-compat/hermetic", "ci-vm-density/unit"}
SPACES_MACOS_JOBS = {"ci-cua-spaces-macos/ffi", "ci-cua-spaces-macos/swift-tests",
                     "ci-cua-spaces-macos/bindings", "ci-cua-spaces-macos/app-bundle"}


def expression(key, original):
    workflow, _ = key.split("/")
    fallback = "matrix.os" if original == "${{ matrix.os }}" else repr(original)
    guard = ("vars.FORK_MACOS_CI_ENABLED == 'true' && vars.FORK_MACOS_RUNNER && github.actor == github.repository_owner && "
             "github.triggering_actor == github.repository_owner && "
             "(github.event_name != 'pull_request' || "
             "github.event.pull_request.head.repo.full_name == github.repository)")
    if fallback == "matrix.os":
        guard = "(matrix.os == 'macos-latest' || matrix.os == 'macos-26') && " + guard
    if workflow in REUSABLE:
        guard += (" && github.workflow_ref == format('{0}/.github/workflows/" + workflow +
                  ".yml@{1}', github.repository, github.ref)")
    if workflow == "ci-cua-spaces-macos":
        # All four replacements retain the old single-job exclusion as well.
        guard += " && !contains(fromJSON(vars.FORK_MACOS_EXCLUDE || '[]'), 'ci-cua-spaces-macos/swift')"
    guard += " && !contains(fromJSON(vars.FORK_MACOS_EXCLUDE || '[]'), '" + key + "')"
    return "${{ " + guard + " && fromJSON(vars.FORK_MACOS_RUNNER) || " + fallback + " }}"


def jobs(path):
    source = path.read_text().split("\njobs:\n", 1)[1]
    return {m[1]: m[2] for m in re.finditer(
        r"(?ms)^  ([\w-]+):\n(.*?)(?=^  [\w-]+:|\Z)", source)}


class ForkMacosRoutingTests(unittest.TestCase):
    def test_every_routed_job_has_exact_guard_and_original_fallback(self):
        seen = set()
        for path in (ROOT / ".github/workflows").glob("*.yml"):
            for job, body in jobs(path).items():
                key = path.stem + "/" + job
                match = re.search(r"^    runs-on: (.+)$", body, re.M)
                if not match:
                    continue
                actual = match[1]
                if "FORK_MACOS_RUNNER" in actual:
                    self.assertIn(key, JOBS)
                    self.assertEqual(actual, expression(key, JOBS[key]))
                    seen.add(key)
                eligible_file = (path.stem.startswith(("ci-", "e2e-", "test")) and
                                 not any(x in path.stem for x in ("release", "publish", "signing")))
                if eligible_file and key not in UPSTREAM_ONLY_JOBS | UPSTREAM_ONLY_MATRICES and (actual in ("macos-latest", "macos-26") or
                   (actual == "${{ matrix.os }}" and re.search(r"macos-(latest|26)\b", body))):
                    self.fail("Unrouted eligible macOS job: " + key)
        self.assertEqual(seen, set(JOBS))

    def test_new_macos_fanout_is_upstream_only_even_on_authorized_forks(self):
        for key in UPSTREAM_ONLY_JOBS | UPSTREAM_ONLY_MATRICES | SPACES_MACOS_JOBS:
            workflow, job_id = key.split("/")
            job = yaml.safe_load((ROOT / f".github/workflows/{workflow}.yml").read_text())["jobs"][job_id]
            for event in ("pull_request", "push", "schedule", "workflow_dispatch"):
                for name, context, _, _ in macos_cases(key, event):
                    upstream = context["github.repository_owner"] == "trycua"
                    case = (key, event, name)
                    if key in UPSTREAM_ONLY_MATRICES:
                        self.assertEqual(job["runs-on"], "${{ matrix.os }}", case)
                        expected = ["ubuntu-latest", "macos-26"] if upstream else ["ubuntu-latest"]
                        self.assertEqual(evaluate(job["strategy"]["matrix"]["os"], context), expected, case)
                    else:
                        self.assertEqual(bool(evaluate(job["if"], context)), upstream, case)
                        if key in UPSTREAM_ONLY_JOBS:
                            self.assertEqual(job["runs-on"], "macos-26", case)

    def test_nightly_spaces_producer_is_upstream_only(self):
        # The nightly Spaces build calls cd-cua-spaces-build (hosted macOS) and
        # publishes on macos-26; a fork must never schedule it.
        workflow = yaml.safe_load((ROOT / ".github/workflows/nightly-cua-spaces.yml").read_text())
        for job_id in ("plan", "build", "publish"):
            condition = workflow["jobs"][job_id]["if"]
            for owner in ("trycua", "cresslank"):
                for event in ("schedule", "workflow_dispatch"):
                    context = {
                        **expressions["workflow_context"](owner, event),
                        "github.ref": "refs/heads/main",
                        "needs.plan.outputs.skip": "false",
                    }
                    self.assertEqual(bool(evaluate(condition, context)), owner == "trycua",
                                     (job_id, owner, event))

    def test_release_publish_signing_and_pinned_lume_are_untouched(self):
        for path in (ROOT / ".github/workflows").glob("*.yml"):
            if (path.stem.startswith("cd-") or
                any(x in path.stem for x in ("publish", "release", "signatures")) or
                path.stem in ("docs-nightly", "ci-swift-lume", "ci-installers")):
                self.assertNotIn("FORK_MACOS_RUNNER", path.read_text(), path.name)

    def test_dynamic_docs_matrix_is_covered(self):
        self.assertIn("ci-check-docs/native-reference", JOBS)


if __name__ == "__main__":
    unittest.main()
