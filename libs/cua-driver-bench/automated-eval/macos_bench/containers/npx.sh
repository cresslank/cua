#!/usr/bin/env sh
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
# Docker's no-new-privileges/default seccomp blocks Chromium's inner sandbox.
# Scope this exception to Electron, inside the disposable outer container only.
if [ "${1:-}" = electron ]; then
  shift
  exec /usr/bin/npx --no-install electron "$@" --no-sandbox
fi
exec /usr/bin/npx "$@"
