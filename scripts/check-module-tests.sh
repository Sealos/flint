#!/usr/bin/env bash
# Every source module must have unit tests. A module passes when it:
#   - contains #[cfg(test)] / #[test] / a #[gpui_kit::test], or
#   - names the file holding its tests with a `// module-tests: <path relative to the module>` line, or
#   - is pure wiring (lib.rs, main.rs, mod.rs) or a test file itself (*tests.rs), or
#   - is listed in scripts/module-tests-allowlist.txt (existing debt; the list may only shrink).
set -euo pipefail
cd "$(dirname "$0")/.."
ALLOW=scripts/module-tests-allowlist.txt
fail=0
while IFS= read -r f; do
  base=$(basename "$f")
  case "$base" in lib.rs | main.rs | mod.rs | *tests.rs) continue ;; esac
  if grep -qE '#\[(cfg\(test\)|test|gpui_kit::test)\]' "$f"; then continue; fi
  if target=$(grep -oE '// module-tests: *[^ ]+' "$f" | head -1 | sed 's#// module-tests: *##'); [ -n "$target" ]; then
    if [ -f "$(dirname "$f")/$target" ]; then continue; fi
    echo "error: $f points to missing test file $target"; fail=1; continue
  fi
  if grep -qxF "$f" "$ALLOW"; then continue; fi
  echo "error: $f has no unit tests (add #[cfg(test)] mod tests, or // module-tests: <file>)"
  fail=1
done < <(find crates -path '*/src/*.rs' | sort)
# Keep the allowlist honest: entries must still exist and still lack tests.
while IFS= read -r f; do
  case "$f" in '' | '#'*) continue ;; esac
  if [ ! -f "$f" ]; then echo "error: $ALLOW lists $f, which no longer exists; remove it"; fail=1
  elif grep -qE '#\[(cfg\(test\)|test|gpui_kit::test)\]|// module-tests:' "$f"; then echo "error: $f now has tests; remove it from $ALLOW"; fail=1; fi
done < "$ALLOW"
exit $fail
