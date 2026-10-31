"""Evaluate the expression subset exercised by fork workflow-contract tests.

These fixtures model explicit contexts, not GitHub-hosted execution. Unknown
context paths fail rather than silently making an admission test pass.
"""

import json
import re


TOKEN = re.compile(r"'(?:[^']|'')*'|!=|!|&&|\|\||\b(?:github|vars|needs|matrix)(?:\.[\w*-]+)+")


def evaluate(value, context):
    if not isinstance(value, str) or not value.startswith("${{"):
        return value
    match = re.fullmatch(r"\$\{\{\s*(.*?)\s*\}\}", value.strip())
    assert match, value
    expression = match.group(1)

    def translate(match):
        token = match.group()
        if token.startswith("'"):
            return repr(token[1:-1].replace("''", "'"))
        if token in ("!=", "!", "&&", "||"):
            return {"!=": "!=", "!": " not ", "&&": " and ", "||": " or "}[token]
        if token not in context:
            raise AssertionError(f"Unmodeled workflow context: {token}")
        return f"context[{token!r}]"

    return eval(TOKEN.sub(translate, expression).strip(), {"__builtins__": {}}, {
        "context": context,
        "fromJSON": json.loads,
        "contains": lambda collection, item: item in collection,
        "startsWith": lambda value, prefix: value.startswith(prefix),
        "cancelled": lambda: context.get("cancelled", False),
    })


def workflow_context(owner="trycua", event_name="pull_request"):
    repository = f"{owner}/cua"
    return {
        "github.repository_owner": owner,
        "github.repository": repository,
        "github.actor": owner,
        "github.triggering_actor": owner,
        "github.event_name": event_name,
        "github.event.pull_request.head.repo.full_name": repository,
        "github.event.pull_request.draft": False,
        "github.event.pull_request.labels.*.name": [],
        "github.head_ref": "feature/x",
        "vars.FORK_MACOS_CI_ENABLED": "",
        "vars.FORK_MACOS_RUNNER": "",
        "vars.FORK_MACOS_EXCLUDE": "",
        "needs.changes.outputs.run": "",
        "needs.changes.outputs.typescript": "",
        "needs.reuse.outputs.pr_subset": "",
        **{f"needs.{need}.result": "success" for need in ("build-sdk", "build-fixtures", "build-cli")},
    }


def macos_cases(job_key, event_name="pull_request"):
    """(name, context, admitted, self-hosted) for upstream and fork admission."""
    upstream = workflow_context(event_name=event_name)
    disabled = workflow_context("cresslank", event_name)
    enabled = {**disabled, "vars.FORK_MACOS_CI_ENABLED": "true",
               "vars.FORK_MACOS_RUNNER": '["self-hosted","macOS","ARM64"]'}
    return (
        ("upstream", upstream, True, False),
        ("upstream-outsider", {**upstream, "github.actor": "contributor",
                               "github.triggering_actor": "contributor",
                               "github.event.pull_request.head.repo.full_name": "contributor/cua"}, True, False),
        ("fork-disabled", disabled, False, False),
        ("fork-owner", enabled, True, True),
        ("fork-disabled-with-runner", {**enabled, "vars.FORK_MACOS_CI_ENABLED": "false"}, False, False),
        ("fork-no-runner", {**enabled, "vars.FORK_MACOS_RUNNER": ""}, False, False),
        ("fork-outsider", {**enabled, "github.actor": "contributor"}, False, False),
        ("fork-rerun-outsider", {**enabled, "github.triggering_actor": "contributor"}, False, False),
        ("fork-cross-repo", {**enabled, "github.event.pull_request.head.repo.full_name": "contributor/cua"},
         event_name != "pull_request", event_name != "pull_request"),
        ("fork-excluded", {**enabled, "vars.FORK_MACOS_EXCLUDE": json.dumps([job_key])}, False, False),
        ("fork-other-excluded", {**enabled, "vars.FORK_MACOS_EXCLUDE": '["other/job"]'}, True, True),
    )
