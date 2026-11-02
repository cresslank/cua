#!/usr/bin/env bash
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
set -euo pipefail
if [[ $# -lt 1 || "$1" == --help ]]; then
  printf 'Usage: CDB_CONTAINER_IMAGE=image [CDB_CONTAINER_NETWORK=none|bridge] bash %s EVIDENCE_DIR [-- COMMAND ARG...]\n' "$0"
  exit 2
fi
evidence=$(realpath -m -- "$1")
shift
[[ ${1:-} != -- ]] || shift
network=${CDB_CONTAINER_NETWORK:-none}
[[ "$network" == none || "$network" == bridge ]] || { printf 'Invalid network\n' >&2; exit 2; }
image=${CDB_CONTAINER_IMAGE:-cua-bench-linux:disposable}
mounts=()
# Only explicitly selected bench inputs. Never infer or mount a login profile.
if [[ -n ${CDB_CUA_ALLOWLIST:-} ]]; then
  scopes=$(realpath -e -- "$CDB_CUA_ALLOWLIST")
  [[ -f "$scopes" ]] || { printf 'Allowlist must be a file\n' >&2; exit 2; }
  mounts+=(--mount "type=bind,src=$scopes,dst=/opt/cua-container/authorized-scopes.json,readonly")
fi
if [[ -n ${CDB_CLAUDE_TOKEN_FILE:-} && -n ${CDB_ANTHROPIC_API_KEY_FILE:-} ]]; then
  printf 'Exactly one Claude credential mode is allowed; both credential files are set\n' >&2; exit 2
fi
if [[ -n ${CDB_CLAUDE_TOKEN_FILE:-} ]]; then
  [[ ! -L "$CDB_CLAUDE_TOKEN_FILE" ]] || { printf 'Token symlink refused\n' >&2; exit 2; }
  token=$(realpath -e -- "$CDB_CLAUDE_TOKEN_FILE")
  [[ -f "$token" && $(stat -c %u -- "$token") == "$UID" && $(stat -c %a -- "$token") == 600 ]] || {
    printf 'Token must be an owned regular 0600 file\n' >&2; exit 2;
  }
  mounts+=(--mount "type=bind,src=$token,dst=/opt/cua-container/claude-token,readonly")
fi
if [[ -n ${CDB_ANTHROPIC_API_KEY_FILE:-} ]]; then
  [[ ! -L "$CDB_ANTHROPIC_API_KEY_FILE" ]] || { printf 'API key symlink refused\n' >&2; exit 2; }
  key=$(realpath -e -- "$CDB_ANTHROPIC_API_KEY_FILE" 2>/dev/null) || {
    printf 'API key must be an owned regular 0600 file\n' >&2; exit 2;
  }
  [[ -f "$key" && $(stat -c %u -- "$key") == "$UID" && $(stat -c %a -- "$key") == 600 ]] || {
    printf 'API key must be an owned regular 0600 file\n' >&2; exit 2;
  }
  mounts+=(--mount "type=bind,src=$key,dst=/opt/cua-container/anthropic-api-key,readonly")
fi
# Resolve first; startup must never pull/update an image or reuse an allocation.
iid=$(docker image inspect --format '{{.Id}}' "$image")
[[ ! -e "$evidence" ]] || { printf 'Evidence path already exists: %s\n' "$evidence" >&2; exit 2; }
mkdir -m 700 -- "$evidence"
name="cua-bench-${UID}-$(date +%s)-${RANDOM}"
logger=
waiter=
cleanup() {
  local rc=$?
  trap - EXIT INT TERM
  # Address only this invocation's unique container, including startup failure.
  if docker container inspect "$name" >/dev/null 2>&1; then
    docker stop --time 30 "$name" >/dev/null 2>&1 || true
    docker rm -f "$name" >/dev/null 2>&1 || true
  fi
  if [[ -n "$logger" ]]; then wait "$logger" 2>/dev/null || true; fi
  if [[ -n "$waiter" ]]; then wait "$waiter" 2>/dev/null || true; fi
  if docker container inspect "$name" >/dev/null 2>&1; then
    printf 'Owned container survived cleanup: %s\n' "$name" >&2
    rc=1
  fi
  printf '%s\n' "$rc" > "$evidence/docker-exit"
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf 'image=%s\ncontainer=%s\n' "$iid" "$name" > "$evidence/container.txt"
# No host HOME, /tmp, sockets, devices, PID/IPC namespaces, GUI env or credentials.
# The writable home is intentionally exec: native regression fixtures and managed
# Python environments create executables there. The outer container stays fenced.
docker run --detach --name "$name" --hostname cua-bench-private \
  --cidfile "$evidence/container.id" \
  --network "$network" --ipc private --shm-size 1g \
  --cap-drop ALL --security-opt no-new-privileges --read-only \
  --user "$(id -u):$(id -g)" --pids-limit 2048 --memory 12g --cpus 6 \
  --tmpfs /tmp:rw,noexec,nosuid,nodev,mode=1777 \
  --tmpfs /run:rw,noexec,nosuid,nodev,mode=755 \
  --tmpfs "/home/bench:rw,exec,nosuid,nodev,mode=700,uid=$(id -u),gid=$(id -g)" \
  --mount "type=bind,src=$evidence,dst=/evidence" "${mounts[@]}" \
  --env "CDB_SESSION_TIMEOUT=${CDB_SESSION_TIMEOUT:-600}" \
  --env "CDB_SESSION_GRACE=${CDB_SESSION_GRACE:-10}" \
  "$iid" "$@" >/dev/null
cid=$(<"$evidence/container.id")
# Read back the exact daemon-owned target before considering launch successful.
docker container inspect "$cid" > "$evidence/container-inspect.json"
docker logs --follow "$cid" &
logger=$!
# A shell wait is signal-interruptible; a foreground attached docker client isn't.
docker wait "$cid" > "$evidence/container-status" &
waiter=$!
wait "$waiter"
waiter=
wait "$logger"
logger=
rc=$(<"$evidence/container-status")
[[ "$rc" =~ ^[0-9]+$ ]] || { printf 'Missing container exit status\n' >&2; exit 1; }
exit "$rc"
