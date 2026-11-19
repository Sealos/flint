//! The ring buffer a log session fills, and the immutable views of it ([`LogLines`]) that
//! snapshots carry.
//!
//! Lines are stored in sealed chunks of [`CHUNK_LINES`] shared through `Arc`, plus one open tail
//! chunk. A view clones the list of chunk pointers and copies only the tail (at most
//! `CHUNK_LINES - 1` cheap line clones), so publishing a 100k-line buffer ten times a second
//! costs about the same as publishing a small one. Eviction is exact in line count (a view never
//! shows more than the limit); memory is freed a whole chunk at a time.

use std::collections::VecDeque;
use std::sync::Arc;

use super::line::{LineId, LogLine};

/// Lines per sealed chunk.
pub const CHUNK_LINES: usize = 1024;

/// The lines of a session, oldest first, bounded by a line limit.
#[derive(Debug)]
pub struct LogBuffer {
    limit: usize,
    /// Sealed chunks, each exactly [`CHUNK_LINES`] long.
    chunks: VecDeque<Arc<[LogLine]>>,
    /// Id of the first line of `chunks[0]` (or of `tail` when there are no chunks).
    base: LineId,
    tail: Vec<LogLine>,
    /// Oldest line still kept. Lines between `base` and `first` are evicted but not freed yet.
    first: LineId,
    /// Id the next line gets.
    next: LineId,
}

impl LogBuffer {
    /// A buffer keeping at most `limit` lines (at least one).
    pub fn new(limit: usize) -> Self {
        LogBuffer {
            limit: limit.max(1),
            chunks: VecDeque::new(),
            base: 0,
            tail: Vec::new(),
            first: 0,
            next: 0,
        }
    }

    /// Appends a line, evicting the oldest one if the buffer is full. Returns the line's id.
    pub fn push(&mut self, line: LogLine) -> LineId {
        let id = self.next;
        self.tail.push(line);
        self.next += 1;
        if self.tail.len() == CHUNK_LINES {
            let sealed: Arc<[LogLine]> = std::mem::take(&mut self.tail).into();
            self.chunks.push_back(sealed);
        }
        if self.next - self.first > self.limit as u64 {
            self.first = self.next - self.limit as u64;
            while self.base + CHUNK_LINES as u64 <= self.first && !self.chunks.is_empty() {
                self.chunks.pop_front();
                self.base += CHUNK_LINES as u64;
            }
        }
        id
    }

    /// The id of the newest line, if any line was ever pushed.
    pub fn last_id(&self) -> Option<LineId> {
        self.next.checked_sub(1)
    }

    /// Lines currently kept.
    pub fn len(&self) -> usize {
        (self.next - self.first) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.next == self.first
    }

    /// An immutable view of the buffer as it is now.
    pub fn view(&self) -> LogLines {
        LogLines {
            chunks: self.chunks.iter().cloned().collect(),
            tail: Arc::from(self.tail.as_slice()),
            base: self.base,
            first: self.first,
            end: self.next,
        }
    }
}

/// An immutable view of a session's lines, as of one snapshot. Lines are addressed by
/// [`LineId`]: `first_id()..end_id()` are present. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct LogLines {
    chunks: Arc<[Arc<[LogLine]>]>,
    tail: Arc<[LogLine]>,
    base: LineId,
    first: LineId,
    end: LineId,
}

impl LogLines {
    /// The oldest line present.
    pub fn first_id(&self) -> LineId {
        self.first
    }

    /// One past the newest line present. Lines with ids from the previous snapshot's `end_id()`
    /// up to this one are new.
    pub fn end_id(&self) -> LineId {
        self.end
    }

    pub fn len(&self) -> usize {
        (self.end - self.first) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.end == self.first
    }

    pub fn contains(&self, id: LineId) -> bool {
        (self.first..self.end).contains(&id)
    }

    /// The line with this id, unless it was evicted or has not arrived.
    pub fn get(&self, id: LineId) -> Option<&LogLine> {
        if !self.contains(id) {
            return None;
        }
        let offset = (id - self.base) as usize;
        let (chunk, index) = (offset / CHUNK_LINES, offset % CHUNK_LINES);
        match self.chunks.get(chunk) {
            Some(lines) => lines.get(index),
            None => self.tail.get(offset - self.chunks.len() * CHUNK_LINES),
        }
    }

    /// The `index`-th line present (0 is the oldest), for virtualized lists.
    pub fn at(&self, index: usize) -> Option<(LineId, &LogLine)> {
        let id = self.first.checked_add(index as u64)?;
        self.get(id).map(|line| (id, line))
    }

    /// The lines with ids in `from..to` that are present, oldest first.
    pub fn range(&self, from: LineId, to: LineId) -> impl Iterator<Item = (LineId, &LogLine)> {
        let (from, to) = (from.max(self.first), to.min(self.end));
        (from..to.max(from)).filter_map(|id| self.get(id).map(|line| (id, line)))
    }

    /// Every line present, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = (LineId, &LogLine)> {
        self.range(self.first, self.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(n: u64) -> LogLine {
        LogLine::parse(&format!("line {n}"), false)
    }

    fn texts(lines: &LogLines) -> Vec<String> {
        lines.iter().map(|(_, l)| l.text.to_string()).collect()
    }

    // covers UP-40
    #[test]
    fn keeps_at_most_the_limit_and_ids_survive_eviction() {
        let mut buffer = LogBuffer::new(3);
        for n in 0..5 {
            assert_eq!(buffer.push(line(n)), n);
        }
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.last_id(), Some(4));
        let view = buffer.view();
        assert_eq!((view.first_id(), view.end_id(), view.len()), (2, 5, 3));
        assert_eq!(texts(&view), ["line 2", "line 3", "line 4"]);
        assert!(view.get(1).is_none(), "evicted");
        assert!(view.get(5).is_none(), "not arrived");
        assert_eq!(view.at(0).map(|(id, _)| id), Some(2));
        assert_eq!(
            view.range(0, 4).map(|(id, _)| id).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(view.range(4, 2).count(), 0);
    }

    // covers UP-40
    #[test]
    fn whole_chunks_are_freed_and_views_stay_frozen() {
        let limit = CHUNK_LINES + 10;
        let mut buffer = LogBuffer::new(limit);
        for n in 0..(3 * CHUNK_LINES as u64 + 5) {
            buffer.push(line(n));
        }
        // Memory: at most the limit plus one chunk.
        let kept = buffer.chunks.len() * CHUNK_LINES + buffer.tail.len();
        assert!(kept <= limit + CHUNK_LINES, "{kept}");
        let view = buffer.view();
        assert_eq!(view.len(), limit);
        for (i, (id, l)) in view.iter().enumerate() {
            assert_eq!(id, view.first_id() + i as u64);
            assert_eq!(&*l.text, format!("line {id}"));
        }
        // A view does not change when the buffer moves on.
        let before = texts(&view);
        for n in 0..(2 * CHUNK_LINES as u64) {
            buffer.push(line(10_000 + n));
        }
        assert_eq!(texts(&view), before);
        assert_eq!(buffer.view().len(), limit);
    }

    #[test]
    fn an_empty_buffer_has_an_empty_view() {
        let buffer = LogBuffer::new(0);
        assert!(buffer.is_empty());
        assert_eq!(buffer.last_id(), None);
        let view = buffer.view();
        assert!(view.is_empty());
        assert!(view.at(0).is_none());
        assert!(LogLines::default().iter().next().is_none());
    }
}
