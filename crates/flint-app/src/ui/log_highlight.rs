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

/// How many bytes of ranges are kept; past this the cache starts over (a screen's rows need far
/// fewer, ranges merged and clipped to what is drawn).
const KEEP_BYTES: usize = 4 << 20;

/// The matches found in lines' text, for the current matcher.
#[derive(Default)]
pub(super) struct Highlights {
    found: HashMap<LineId, Arc<[Range<usize>]>>,
    /// Bytes the ranges in `found` take.
    bytes: usize,
    /// Lines drawn without their matches, to find next.
    wanted: Vec<LineId>,
    /// A search for `wanted` runs.
    finding: bool,
    _find: Option<Task<()>>,
}

impl Highlights {
    /// Forgets every match: the matcher, or how much of a line is drawn, changed.
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
                let Ok((mut ids, lines, matcher, generation, cutoff)) =
                    this.update(cx, |view, _| {
                        (
                            std::mem::take(&mut view.highlights.wanted),
                            view.snapshot.lines.clone(),
                            view.matcher.clone(),
                            view.search_generation,
                            (!view.wrap).then_some(super::log_model::UNWRAPPED_MAX_BYTES),
                        )
                    })
                else {
                    return;
                };
                let found: Vec<(LineId, Arc<[Range<usize>]>)> = cx
                    .background_spawn(async move {
                        ids.sort_unstable();
                        ids.dedup();
                        ids.into_iter()
                            .filter_map(|id| {
                                let line = lines.get(id)?;
                                let found = matcher.match_ranges(&line.text);
                                Some((id, drawn_ranges(found, cutoff).into()))
                            })
                            .collect()
                    })
                    .await;
                let _ = this.update(cx, |view, cx| {
                    view.highlights.finding = false;
                    if view.search_generation == generation {
                        let size = |r: &Arc<[Range<usize>]>| std::mem::size_of_val(&r[..]);
                        let adding: usize = found.iter().map(|(_, r)| size(r)).sum();
                        // Over the budget: start over. What this frame's rows need is kept
                        // whatever it takes, so they are not asked for again and again.
                        if view.highlights.bytes + adding > KEEP_BYTES {
                            view.highlights.found.clear();
                            view.highlights.bytes = 0;
                        }
                        for (id, ranges) in found {
                            view.highlights.bytes += size(&ranges);
                            if let Some(old) = view.highlights.found.insert(id, ranges) {
                                view.highlights.bytes -= size(&old);
                            }
                        }
                    }
                    cx.notify();
                });
            }));
        }
        Some(Arc::from([]))
    }
}

/// `found` (sorted, not overlapping) as drawn: ranges that touch merged into one (a dense
/// pattern such as `.` is one range, not one per character), and only up to `cutoff` bytes,
/// where an unwrapped line is cut.
pub(super) fn drawn_ranges(found: Vec<Range<usize>>, cutoff: Option<usize>) -> Vec<Range<usize>> {
    let cutoff = cutoff.unwrap_or(usize::MAX);
    let mut drawn: Vec<Range<usize>> = Vec::new();
    for r in found {
        if r.start >= cutoff {
            break;
        }
        let r = r.start..r.end.min(cutoff);
        match drawn.last_mut() {
            Some(last) if last.end >= r.start => last.end = last.end.max(r.end),
            _ => drawn.push(r),
        }
    }
    drawn.shrink_to_fit();
    drawn
}
