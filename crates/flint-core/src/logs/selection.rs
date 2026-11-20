//! A selection of consecutive log lines, by [`LineId`].
//!
//! Ids never change, so a selection stays on the same lines while new lines arrive. When the ring
//! buffer evicts the start of a selection, [`LineSelection::clamp`] shrinks it to the lines still
//! present (or drops it when all are gone), and [`LineSelection::is_cut`] says so, so the UI can
//! tell the user. Copy uses [`LineSelection::text`]; AI log analysis (#56) sends the same lines.

use super::buffer::LogLines;
use super::line::{LineFormat, LineId, LogLine};

/// A range of lines from `anchor` (where the selection started) to `head` (where it was
/// extended to, by mouse or Shift and the arrow keys). Either may be the larger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineSelection {
    pub anchor: LineId,
    pub head: LineId,
}

impl LineSelection {
    /// One line.
    pub fn line(id: LineId) -> Self {
        LineSelection {
            anchor: id,
            head: id,
        }
    }

    /// From `anchor` to `head`, inclusive.
    pub fn new(anchor: LineId, head: LineId) -> Self {
        LineSelection { anchor, head }
    }

    /// Keeps the anchor and moves the head (Shift-click).
    pub fn extend_to(self, head: LineId) -> Self {
        LineSelection { head, ..self }
    }

    /// Moves the head by `delta` lines (Shift and the arrow keys), staying on lines present in
    /// `lines`.
    pub fn move_head(self, delta: i64, lines: &LogLines) -> Self {
        let Some(last) = lines.end_id().checked_sub(1).filter(|_| !lines.is_empty()) else {
            return self;
        };
        let head = self
            .head
            .saturating_add_signed(delta)
            .clamp(lines.first_id(), last);
        self.extend_to(head)
    }

    /// The first selected line.
    pub fn start(&self) -> LineId {
        self.anchor.min(self.head)
    }

    /// The last selected line (inclusive).
    pub fn end(&self) -> LineId {
        self.anchor.max(self.head)
    }

    /// Lines selected, including any no longer present.
    pub fn len(&self) -> u64 {
        self.end() - self.start() + 1
    }

    /// A selection always holds at least one line.
    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn contains(&self, id: LineId) -> bool {
        (self.start()..=self.end()).contains(&id)
    }

    /// How many selected lines are in `lines`, without visiting them: what the selection hint
    /// and the copy note count, every frame if need be.
    pub fn present_len(&self, lines: &LogLines) -> u64 {
        let Some(last) = lines.end_id().checked_sub(1).filter(|_| !lines.is_empty()) else {
            return 0;
        };
        let start = self.start().max(lines.first_id());
        let end = self.end().min(last);
        if start > end { 0 } else { end - start + 1 }
    }

    /// Whether some selected lines are no longer (or not yet) in `lines`.
    pub fn is_cut(&self, lines: &LogLines) -> bool {
        !(lines.contains(self.start()) && lines.contains(self.end()))
    }

    /// The part of the selection still present in `lines`, keeping its direction; `None` when
    /// none of it is.
    pub fn clamp(self, lines: &LogLines) -> Option<Self> {
        let last = lines.end_id().checked_sub(1)?;
        if lines.is_empty() || self.end() < lines.first_id() || self.start() > last {
            return None;
        }
        let fit = |id: LineId| id.clamp(lines.first_id(), last);
        Some(LineSelection {
            anchor: fit(self.anchor),
            head: fit(self.head),
        })
    }

    /// The selected lines present in `lines`, oldest first.
    pub fn lines<'a>(&self, lines: &'a LogLines) -> impl Iterator<Item = (LineId, &'a LogLine)> {
        lines.range(self.start(), self.end().saturating_add(1))
    }

    /// The selected lines as shown, one per line, without redaction: what copy puts on the
    /// clipboard. `timestamps` matches the view's timestamp toggle; lines of a workload session
    /// keep their `[pod/container]` prefix ([`LineSelection::text_as`] can leave it out).
    pub fn text(&self, lines: &LogLines, timestamps: bool) -> String {
        self.text_as(
            lines,
            LineFormat {
                timestamps,
                source: true,
            },
        )
    }

    /// The selected lines as [`LogLine::display_as`] shows them with `format`, one per line,
    /// without redaction.
    pub fn text_as(&self, lines: &LogLines, format: LineFormat) -> String {
        let mut out = String::new();
        for (i, (_, line)) in self.lines(lines).enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&line.display_as(format));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::buffer::LogBuffer;

    fn buffer(limit: usize, count: u64) -> LogBuffer {
        let mut buffer = LogBuffer::new(limit);
        for n in 0..count {
            buffer.push(LogLine::parse(
                &format!("2024-05-01T12:00:0{}Z line {n}", n % 10),
                false,
            ));
        }
        buffer
    }

    // covers UP-40
    #[test]
    fn selects_and_copies_in_either_direction() {
        let lines = buffer(10, 5).view();
        let down = LineSelection::line(1).extend_to(3);
        let up = LineSelection::new(3, 1);
        assert_eq!((down.start(), down.end(), down.len()), (1, 3, 3));
        assert_eq!(down.text(&lines, false), up.text(&lines, false));
        assert_eq!(down.text(&lines, false), "line 1\nline 2\nline 3");
        assert_eq!(
            LineSelection::line(0).text(&lines, true),
            "2024-05-01T12:00:00Z line 0"
        );
        assert!(down.contains(2) && !down.contains(4));
        assert!(!down.is_empty());
    }

    // covers UP-40
    #[test]
    fn shift_arrows_stay_on_present_lines() {
        let lines = buffer(10, 5).view();
        let sel = LineSelection::line(3);
        assert_eq!(sel.move_head(1, &lines), LineSelection::new(3, 4));
        assert_eq!(sel.move_head(5, &lines), LineSelection::new(3, 4));
        assert_eq!(sel.move_head(-10, &lines), LineSelection::new(3, 0));
        let empty = LogLines::default();
        assert_eq!(sel.move_head(1, &empty), sel);
    }

    // covers UP-40
    #[test]
    fn eviction_shrinks_then_drops_the_selection() {
        let mut buf = buffer(4, 4);
        let sel = LineSelection::new(3, 1);
        assert!(!sel.is_cut(&buf.view()));
        assert_eq!(sel.clamp(&buf.view()), Some(sel));

        // Lines 0 and 1 are evicted: the selection keeps 2..=3 and its direction.
        buf.push(LogLine::parse("line 4", false));
        buf.push(LogLine::parse("line 5", false));
        let view = buf.view();
        assert!(sel.is_cut(&view));
        assert_eq!(sel.clamp(&view), Some(LineSelection::new(3, 2)));
        assert_eq!(sel.text(&view, false), "line 2\nline 3");
        assert_eq!(sel.present_len(&view), 2);

        for n in 6..10 {
            buf.push(LogLine::parse(&format!("line {n}"), false));
        }
        assert_eq!(sel.clamp(&buf.view()), None);
        assert_eq!(sel.text(&buf.view(), false), "");
        assert_eq!(sel.present_len(&buf.view()), 0);
        assert_eq!(LineSelection::line(0).clamp(&LogLines::default()), None);
    }

    // covers UP-40: the count matches the lines visited, cut at either end or not.
    #[test]
    fn present_len_counts_without_visiting_the_lines() {
        let mut buf = buffer(6, 6);
        for n in 6..9 {
            buf.push(LogLine::parse(&format!("line {n}"), false));
        }
        // Lines 3..=8 are present.
        let view = buf.view();
        for (a, h) in [
            (0, 2),
            (0, 4),
            (4, 6),
            (5, 12),
            (9, 12),
            (3, 8),
            (0, 20),
            (7, 7),
        ] {
            let sel = LineSelection::new(h, a);
            assert_eq!(
                sel.present_len(&view),
                sel.lines(&view).count() as u64,
                "{a}..={h}"
            );
        }
        assert_eq!(LineSelection::line(0).present_len(&LogLines::default()), 0);
    }

    // covers UP-45
    #[test]
    fn copy_keeps_or_drops_the_source_prefix() {
        let mut buf = LogBuffer::new(10);
        let source = std::sync::Arc::new(super::super::LineSource::new("web-1", "app"));
        for text in ["2024-05-01T12:00:00Z a", "2024-05-01T12:00:01Z b"] {
            let mut line = LogLine::parse(text, false);
            line.source = Some(source.clone());
            buf.push(line);
        }
        let view = buf.view();
        let sel = LineSelection::new(0, 1);
        assert_eq!(sel.text(&view, false), "[web-1/app] a\n[web-1/app] b");
        let bare = LineFormat {
            timestamps: true,
            source: false,
        };
        assert_eq!(
            sel.text_as(&view, bare),
            "2024-05-01T12:00:00Z a\n2024-05-01T12:00:01Z b"
        );
    }
}
