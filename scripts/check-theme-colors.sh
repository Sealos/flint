#!/usr/bin/env bash
# Views take every color from theme tokens (ARCHITECTURE.md, "Theme tokens only"). This fails on a
# color written into UI code: rgb()/rgba()/hsla()/hsl() calls, Hsla/Rgba struct literals,
# 0xRRGGBB literals (underscores allowed), "#rrggbb" strings, GPUI's named colors (red(), black(),
# opaque_grey(..), ...) and gpui-component's palette (ColorName::...). It also flags such text in
# comments; reword the comment. The theme module (theme.rs, accent.rs and themes/) is where colors
# live, so it is exempt. The pattern checks itself against known samples first.
set -euo pipefail
cd "$(dirname "$0")/.."
UI=crates/flint-app/src/ui
EXEMPT='^crates/flint-app/src/ui/(theme\.rs|accent\.rs|themes/)'
pattern='\b(rgba?|hsla?)\(|(^|=>|=|\(|,|\{|return)[[:space:]]*([a-z_]+::)*(Hsla|Rgba)[[:space:]]*\{|\b0x[0-9a-fA-F](_?[0-9a-fA-F]){5}(_?[0-9a-fA-F]){0,2}\b|"#[0-9a-fA-F]{3,8}"|\b(red|green|blue|yellow|black|white|transparent_black|transparent_white)\(\)|\bopaque_grey\(|\bColorName::'

must_match=(
  '.bg(rgb(0xff0000))'
  'let c = Hsla { h: 0., s: 1., l: 0.5, a: 1. };'
  '    Hsla {'
  '    gpui::Rgba {'
  'Some(x) => Hsla { h: 0., s: 0., l: 0., a: 1. },'
  'const RED: u32 = 0xff_00_00;'
  'const RED: u32 = 0xff0000ff;'
  '.text_color("#c2410c")'
  '.bg(black())'
  '.bg(opaque_grey(0.5, 1.))'
  'ColorName::Orange'
)
must_not_match=(
  '.bg(theme.table_active)'
  'let mask = 0x1000;'
  'fn hsla_to_text(color: Hsla) -> String {'
  'fn hex(color: &str) -> gpui_kit::Hsla {'
  '.text_color(theme.foreground)'
)
for sample in "${must_match[@]}"; do
  grep -qE "$pattern" <<<"$sample" || { echo "error: the color pattern misses: $sample"; exit 1; }
done
for sample in "${must_not_match[@]}"; do
  if grep -qE "$pattern" <<<"$sample"; then echo "error: the color pattern wrongly flags: $sample"; exit 1; fi
done

found=$(grep -rnE "$pattern" "$UI" --include='*.rs' | grep -vE "$EXEMPT" || true)
if [ -n "$found" ]; then
  echo "error: hard-coded colors in UI code; use a theme token (cx.theme()) instead:"
  echo "$found"
  exit 1
fi
