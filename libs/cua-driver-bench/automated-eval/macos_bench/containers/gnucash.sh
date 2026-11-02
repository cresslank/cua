#!/usr/bin/env sh
# SPDX-License-Identifier: FSL-1.1-MIT
# Copyright (c) 2026 Cua AI, Inc.
# Execute at the real installation prefix so relocatable resource lookup does
# not depend on this PATH shim's location.
export GSETTINGS_SCHEMA_DIR=/opt/gnucash/share/glib-2.0/schemas
exec /opt/gnucash/bin/gnucash "$@"
