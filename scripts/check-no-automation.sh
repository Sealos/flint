#!/usr/bin/env bash
# Fails if a release binary contains the test-only automation channel (docs/TESTING.md,
# "Automation channel"): its socket name, its log lines or its module path. Release binaries are
# stripped, so this looks at strings, not symbols. Usage: scripts/check-no-automation.sh <binary>
set -euo pipefail
binary=${1:?usage: $0 <binary>}
if [ ! -f "$binary" ]; then
  echo "error: $binary does not exist"
  exit 2
fi
if strings -a "$binary" | grep -E 'automation\.sock|automation channel|flint::ui::automation'; then
  echo "::error::$binary contains the automation channel"
  exit 1
fi
echo "$binary has no automation channel"
