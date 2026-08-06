#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
.auto/checks.sh
.auto/measure.sh
