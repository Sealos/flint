#!/usr/bin/env bash
# Every click handler in the app's UI must perform a registered action (docs/ARCHITECTURE.md,
# "Action registry"), so a click, a key, the command palette and automation take one path.
# A handler passes when its call, or one of the three lines after it, goes through `registry::`
# (`registry::on_click(..)`, `registry::perform(..)`). A handler that is not a control (it only
# stops a click from reaching the row under it) says so on the line before:
#   // not-an-action: <why>
set -euo pipefail
cd "$(dirname "$0")/.."
fail=0
while IFS= read -r f; do
  case "$(basename "$f")" in *tests.rs | registry.rs) continue ;; esac
  awk -v file="$f" '
    { line[NR] = $0 }
    END {
      bad = 0
      for (i = 1; i <= NR; i++) {
        if (line[i] !~ /\.on_(click|mouse_down|mouse_up)\(/) continue
        if (line[i] ~ /^[[:space:]]*\/\//) continue
        ok = 0
        for (j = i; j <= i + 3 && j <= NR; j++) if (line[j] ~ /registry::/) ok = 1
        if (i > 1 && line[i - 1] ~ /\/\/ not-an-action:/) ok = 1
        if (!ok) { printf "error: %s:%d: click handler does not perform a registered action\n", file, i; bad = 1 }
      }
      exit bad
    }' "$f" || fail=1
done < <(find crates/flint-app/src/ui -name '*.rs' | sort)
exit $fail
