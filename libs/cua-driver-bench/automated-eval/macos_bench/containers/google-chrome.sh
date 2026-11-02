#!/usr/bin/env sh
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
# No host profile and no host GUI: the outer container is the sandbox boundary.
exec /usr/bin/google-chrome --no-sandbox --ozone-platform=x11 "$@"
