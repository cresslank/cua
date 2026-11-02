#!/usr/bin/env bash
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
set -euo pipefail
umask 077
mkdir -p /home/bench/runtime /home/bench/tmp /evidence/work
chmod 700 /home/bench/runtime
optional=()
if [[ -f /opt/cua-container/claude-token ]]; then
  optional+=(CDB_CLAUDE_TOKEN_FILE=/opt/cua-container/claude-token)
fi
if [[ -f /opt/cua-container/anthropic-api-key ]]; then
  optional+=(CDB_ANTHROPIC_API_KEY_FILE=/opt/cua-container/anthropic-api-key)
fi
# Deliberately do not inherit even Docker/image env. A managed interpreter must
# launch as COMMAND inside this session; docker exec alone lacks its private bus.
exec env -i \
  HOME=/home/bench USER=bench LOGNAME=bench LANG=C.UTF-8 \
  PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin \
  TMPDIR=/home/bench/tmp XDG_RUNTIME_DIR=/home/bench/runtime \
  XDG_SESSION_TYPE=x11 GDK_BACKEND=x11 GTK_A11Y=always NO_AT_BRIDGE=0 \
  QT_ACCESSIBILITY=1 QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 \
  CUA_DRIVER_RS_TELEMETRY_ENABLED=false DO_NOT_TRACK=1 \
  CUA_DRIVER_BIN=/usr/local/bin/cua-driver-local CUA_SKILLS=/opt/cua-skills \
  CDB_BENCH_DISPOSABLE=1 CDB_BENCH_WORK=/evidence/work \
  CDB_TASKPACK=/opt/cua-bench/taskpack PYTHONDONTWRITEBYTECODE=1 \
  CDB_SESSION_TIMEOUT="${CDB_SESSION_TIMEOUT:-600}" \
  CDB_SESSION_GRACE="${CDB_SESSION_GRACE:-10}" "${optional[@]}" \
  /usr/bin/python3 /opt/cua-bench/automated-eval/macos_bench/launch/linux_session.py \
  --evidence /evidence/session -- "$@"
