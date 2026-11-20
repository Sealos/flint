//! Highlights of a regular-expression search (#264, UP-48), found off the UI thread. A regex
//! can take a while over a long line (up to 64 KiB), so the rows drawn ask for their matches
//! and draw plain until they land: the whole line is matched there, so anchors mean what they do
//! in the search, and the ranges are clipped to the text drawn later. Plain
//! text queries are matched as the row is drawn, which is fast.
// module-tests: log_tests.rs

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use flint_core::LineId;
use gpui_kit::*;

use super::LogView;

/// How many lines' highlights are kept; past this the cache starts over (a screen shows far
/// fewer).
const KEEP: usize = 4096;

/// The matches found in lines' text, for the current matcher.
#[derive(Default)]
pub(super) struct Highlights {
    found: HashMap<LineId, Arc<[Range<usize>]>>,
    /// Lines drawn without their matches, to find next.
    wanted: Vec<LineId>,
    /// A search for `wanted` runs.
    finding: bool,
    _find: Option<Task<()>>,
}

impl Highlights {
    /// Forgets every match: the matcher changed.
    pub(super) fn clear(&mut self) {
        *self = Highlights::default();
    }

    /// The matches found in line `id`'s text, if they were.
    #[cfg(test)]
    pub(super) fn found(&self, id: LineId) -> Option<&[Range<usize>]> {
        self.found.get(&id).map(|found| &found[..])
    }
}

impl LogView {
    /// The matches in line `id`'s text to draw now: `None` to match it as it is drawn (a plain
    /// query), else those found so far (none yet: asked for, and drawn when they come).
    pub(super) fn text_matches(
        &mut self,
        id: LineId,
        cx: &mut Context<Self>,
    ) -> Option<Arc<[Range<usize>]>> {
        if !self.matcher.is_regex() {
            return None;
        }
        if let Some(found) = self.highlights.found.get(&id) {
            return Some(found.clone());
        }
        self.highlights.wanted.push(id);
        if !self.highlights.finding {
            self.highlights.finding = true;
            // Spawned tasks start after this frame's rows: all of them are asked for at once.
            self.highlights._find = Some(cx.spawn(async move |this, cx| {
                let Ok((mut ids, lines, matcher, generation)) = this.update(cx, |view, _| {
                    (
                        std::mem::take(&mut view.highlights.wanted),
                        view.snapshot.lines.clone(),
                        view.matcher.clone(),
                        view.search_generation,
                    )
                }) else {
                    return;
                };
                let found: Vec<(LineId, Arc<[Range<usize>]>)> = cx
                    .background_spawn(async move {
                        ids.sort_unstable();
                        ids.dedup();
                        ids.into_iter()
                            .filter_map(|id| {
                                let line = lines.get(id)?;
                                Some((id, matcher.match_ranges(&line.text).into()))
                            })
                            .collect()
                    })
                    .await;
                let _ = this.update(cx, |view, cx| {
                    view.highlights.finding = false;
                    if view.search_generation == generation {
                        if view.highlights.found.len() + found.len() > KEEP {
                            view.highlights.found.clear();
                        }
                        view.highlights.found.extend(found);
                    }
                    cx.notify();
                });
            }));
        }
        Some(Arc::from([]))
    }
}
