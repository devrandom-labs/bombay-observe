#!/usr/bin/env bash
set -euo pipefail
root=research/observepass-autoresearch
tests=$(rg -g '*.rs' -c '#\[(tokio::)?test' "$root" 2>/dev/null | awk -F: '{n+=$2} END{print n+0}'); properties=$(rg -g '*.rs' -c 'proptest!|quickcheck' "$root" 2>/dev/null | awk -F: '{n+=$2} END{print n+0}'); fuzz=$(find "$root" -path '*/fuzz_targets/*.rs' -type f 2>/dev/null | wc -l | tr -d ' '); findings=$(rg -c '^## FINDING-' "$root/RESEARCH-REPORT.md" 2>/dev/null | awk -F: '{n+=$2} END{print n+0}')
echo "METRIC score=$((tests + 5*properties + 10*fuzz + 25*findings)) unit=adversarial_evidence"; echo "METRIC tests=$tests"; echo "METRIC properties=$properties"; echo "METRIC fuzz_targets=$fuzz"; echo "METRIC findings=$findings"
