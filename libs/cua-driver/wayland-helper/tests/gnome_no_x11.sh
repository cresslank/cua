#!/usr/bin/env bash
set -euo pipefail

unset DISPLAY
exec "$@" --no-x11
