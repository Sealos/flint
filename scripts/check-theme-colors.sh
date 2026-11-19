#!/usr/bin/env bash
# Views take every color from theme tokens (ARCHITECTURE.md, "Theme tokens only"). This fails on a
# color written into UI code: rgb()/rgba()/hsla()/hsl() calls, Hsla/Rgba struct literals,
# 0xRRGGBB literals, "#rrggbb" strings, GPUI's named colors (red(), black(), opaque_grey(..), ...)
# and gpui-component's palette (ColorName::...). It also flags such text in comments; reword the
# comment. The theme module (theme.rs, accent.rs and themes/) is where colors live, so it is exempt.
set -euo pipefail
cd "$(dirname "$0")/.."
UI=crates/flint-app/src/ui
EXEMPT='^crates/flint-app/src/ui/(theme\.rs|accent\.rs|themes/)'
pattern='\b(rgba?|hsla?)\(|(=|\(|,|return)[[:space:]]*([a-z_]+::)*(Hsla|Rgba)[[:space:]]*\{|\b0x[0-9a-fA-F]{6}([0-9a-fA-F]{2})?\b|"#[0-9a-fA-F]{3,8}"|\b(red|green|blue|yellow|black|white|transparent_black|transparent_white)\(\)|\bopaque_grey\(|\bColorName::'
found=$(grep -rnE "$pattern" "$UI" --include='*.rs' | grep -vE "$EXEMPT" || true)
if [ -n "$found" ]; then
  echo "error: hard-coded colors in UI code; use a theme token (cx.theme()) instead:"
  echo "$found"
  exit 1
fi
