//! The ring buffer a log session fills, and the immutable views of it ([`LogLines`]) that
//! snapshots carry.
//!
//! Lines are stored in sealed blocks shared through `Arc`, plus one open tail block. A block is
//! sealed when it reaches [`CHUNK_LINES`] lines or its byte share of the budget, whichever comes
//! first. A view clones the list of block pointers and copies only the tail (cheap line clones),
//! so publishing a 100k-line buffer ten times a second costs about the same as a small one.
//!
//! Two limits apply, and the buffer evicts when either is exceeded:
//! - **Lines:** exact. A view never shows more than the line limit.
//! - **Bytes:** the memory the lines take ([`LogLine::heap_bytes`]). When the stored blocks go
//!   over the budget, whole blocks are dropped from the oldest end, so memory stays within the
//!   budget plus one block (a block holds at most 1/[`BLOCKS_PER_BUDGET`] of it).

use std::collections::VecDeque;
use std::sync::Arc;

use super::line::{LineId, LogLine};

/// Most lines per sealed block.
pub const CHUNK_LINES: usize = 1024;

/// A block is sealed once it holds this fraction of the byte budget, so dropping one block
/// frees a small, predictable share of memory.
pub const BLOCKS_PER_BUDGET: usize = 32;

/// The lines of a session, oldest first, bounded by a line limit and a byte budget.
#[derive(Debug)]
pub struct LogBuffer {
    limit: usize,
    budget: usize,
    /// Bytes at which the tail is sealed.
    block_bytes: usize,
    /// Sealed blocks with the id of their first line and their size in bytes.
    chunks: VecDeque<Block>,
    tail: Vec<LogLine>,
    /// Id of the first line in `tail`.
    tail_start: LineId,
    tail_bytes: usize,
    /// Bytes of all stored lines (sealed blocks and tail), evicted-but-not-freed included.
    bytes: usize,
    /// Oldest line still shown. Lines before it may still be stored until their block goes.
    first: LineId,
    /// Id the next line gets.
    next: LineId,
    /// Blocks dropped so far.
    dropped: u64,
}

#[derive(Debug)]
struct Block {
    start: LineId,
    bytes: usize,
    lines: Arc<[LogLine]>,
}

impl LogBuffer {
    /// A buffer keeping at most `limit` lines (at least one), with no byte budget to speak of.
    pub fn new(limit: usize) -> Self {
        Self::with_budget(limit, usize::MAX)
    }

    /// A buffer keeping at most `limit` lines (at least one) and about `budget` bytes. The
    /// budget is raised to at least what one line of [`MAX_LINE_BYTES`](super::MAX_LINE_BYTES) needs per block.
    pub fn with_budget(limit: usize, budget: usize) -> Self {
        let block_bytes = (budget / BLOCKS_PER_BUDGET).max(LogLine::max_heap_bytes());
        LogBuffer {
            limit: limit.max(1),
            budget: budget.max(block_bytes),
            block_bytes,
            chunks: VecDeque::new(),
            tail: Vec::new(),
            tail_start: 0,
            tail_bytes: 0,
            bytes: 0,
            first: 0,
            next: 0,
            dropped: 0,
        }
    }

    /// Appends a line, evicting the oldest lines if a limit is exceeded. Returns the line's id.
    pub fn push(&mut self, line: LogLine) -> LineId {
        let id = self.next;
        let size = line.heap_bytes();
        self.tail.push(line);
        self.tail_bytes += size;
        self.bytes += size;
        self.next += 1;
        if self.tail.len() >= CHUNK_LINES || self.tail_bytes >= self.block_bytes {
            self.seal();
        }
        if self.next - self.first > self.limit as u64 {
            self.first = self.next - self.limit as u64;
        }
        // Drop whole blocks that are fully evicted by the line limit, or the oldest blocks while
        // over the byte budget.
        while let Some(block) = self.chunks.front() {
            let end = block.start + block.lines.len() as u64;
            if end > self.first && self.bytes <= self.budget {
                break;
            }
            self.bytes -= block.bytes;
            self.first = self.first.max(end);
            self.chunks.pop_front();
            self.dropped += 1;
        }
        id
    }

    fn seal(&mut self) {
        let lines: Arc<[LogLine]> = std::mem::take(&mut self.tail).into();
        let start = self.tail_start;
        self.tail_start += lines.len() as u64;
        self.chunks.push_back(Block {
            start,
            bytes: std::mem::take(&mut self.tail_bytes),
            lines,
        });
    }

    /// The id of the newest line, if any line was ever pushed.
    pub fn last_id(&self) -> Option<LineId> {
        self.next.checked_sub(1)
    }

    /// The oldest line shown.
    pub fn first_id(&self) -> LineId {
        self.first
    }

    /// Lines currently shown.
    pub fn len(&self) -> usize {
        (self.next - self.first) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.next == self.first
    }

    /// Drops whole blocks from the oldest end until the stored lines take at most `bytes` (the
    /// open tail block stays), for the shared budget ([`super::LogBudget`]). Returns whether any
    /// line was dropped.
    pub fn trim_to(&mut self, bytes: usize) -> bool {
        let mut trimmed = false;
        while self.bytes > bytes
            && let Some(block) = self.chunks.pop_front()
        {
            self.bytes -= block.bytes;
            self.first = self.first.max(block.start + block.lines.len() as u64);
            self.dropped += 1;
            trimmed = true;
        }
        trimmed
    }

    /// How many blocks were dropped so far; changes exactly when memory was released.
    pub fn dropped_blocks(&self) -> u64 {
        self.dropped
    }

    /// Bytes [`LogBuffer::trim_to`] can free: the sealed blocks.
    pub fn trimmable_bytes(&self) -> usize {
        self.bytes - self.tail_bytes
    }

    /// Bytes the stored lines take, by [`LogLine::heap_bytes`].
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// An immutable view of the buffer as it is now.
    pub fn view(&self) -> LogLines {
        LogLines {
            starts: self.chunks.iter().map(|b| b.start).collect(),
            chunks: self.chunks.iter().map(|b| b.lines.clone()).collect(),
            tail: Arc::from(self.tail.as_slice()),
            tail_start: self.tail_start,
            first: self.first,
            end: self.next,
        }
    }
}

/// An immutable view of a session's lines, as of one snapshot. Lines are addressed by
/// [`LineId`]: `first_id()..end_id()` are present. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct LogLines {
    /// The id of the first line of each block in `chunks`.
    starts: Arc<[LineId]>,
    chunks: Arc<[Arc<[LogLine]>]>,
    tail: Arc<[LogLine]>,
    tail_start: LineId,
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
        if id >= self.tail_start {
            return self.tail.get((id - self.tail_start) as usize);
        }
        let block = self.starts.partition_point(|s| *s <= id).checked_sub(1)?;
        self.chunks[block].get((id - self.starts[block]) as usize)
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

    /// The same view without lines before `first`, releasing the blocks that held only those.
    /// A paused session trims its frozen view to the live buffer's oldest line this way, so the
    /// pause never keeps evicted blocks alive.
    pub fn trimmed(&self, first: LineId) -> LogLines {
        if first <= self.first {
            return self.clone();
        }
        let first = first.min(self.end);
        // Keep the block holding `first` and everything after it.
        let keep_from = self
            .starts
            .partition_point(|s| *s <= first)
            .saturating_sub(1);
        let keep_from = if self
            .starts
            .get(keep_from)
            .zip(self.chunks.get(keep_from))
            .is_some_and(|(s, c)| s + c.len() as u64 <= first)
        {
            keep_from + 1
        } else {
            keep_from
        };
        // A tail none of whose lines remain is let go too: a paused view must not keep a block
        // the buffer has dropped (it keeps `end`, for the held-back count).
        let tail = if first >= self.tail_start + self.tail.len() as u64 {
            Arc::from(Vec::new())
        } else {
            self.tail.clone()
        };
        LogLines {
            starts: self.starts[keep_from..].into(),
            chunks: self.chunks[keep_from..].into(),
            tail,
            tail_start: self.tail_start,
            first,
            end: self.end,
        }
    }

    /// Sealed blocks this view holds (for tests and memory accounting).
    pub fn blocks(&self) -> usize {
        self.chunks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::MAX_LINE_BYTES;

    fn line(n: u64) -> LogLine {
        LogLine::parse(&format!("line {n}"), false)
    }

    fn texts(lines: &LogLines) -> Vec<String> {
        lines.iter().map(|(_, l)| l.text.to_string()).collect()
    }

    // covers UP-40, UP-42: a paused view whose open tail block was later sealed and dropped
    // lets go of it too, keeping its end for the held-back count (#254).
    #[test]
    fn a_trimmed_view_lets_go_of_a_tail_with_no_lines_left() {
        let mut buffer = LogBuffer::new(10_000);
        for n in 0..10 {
            buffer.push(line(n));
        }
        // Paused here: the view holds the open tail, lines 0..10.
        let paused = buffer.view();
        assert_eq!(paused.tail.len(), 10);
        // The tail fills, is sealed, and the buffer drops it.
        for n in 10..(CHUNK_LINES as u64 * 2) {
            buffer.push(line(n));
        }
        assert!(buffer.trim_to(0));
        let trimmed = paused.trimmed(buffer.first_id());
        assert!(trimmed.tail.is_empty(), "the frozen tail is let go");
        assert!(trimmed.is_empty());
        assert_eq!(
            trimmed.end_id(),
            10,
            "the end stays, for the held-back count"
        );
        // A tail with lines left is kept.
        let partial = buffer.view().trimmed(buffer.first_id());
        assert_eq!(partial.tail.len(), buffer.view().tail.len());
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
        let kept: usize =
            buffer.chunks.iter().map(|b| b.lines.len()).sum::<usize>() + buffer.tail.len();
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

    // covers UP-40
    #[test]
    fn the_byte_budget_evicts_whole_blocks_of_long_lines() {
        let budget = 4 * 1024 * 1024;
        let mut buffer = LogBuffer::with_budget(100_000, budget);
        let long = "x".repeat(MAX_LINE_BYTES);
        for n in 0..1000u64 {
            buffer.push(LogLine::parse(&format!("{n} {long}"), false));
            assert!(
                buffer.bytes() <= budget + buffer.block_bytes,
                "{} bytes after line {n}",
                buffer.bytes()
            );
        }
        // Far fewer than the line limit: the budget decided.
        let view = buffer.view();
        assert!(view.len() < 100, "{}", view.len());
        assert!(view.len() > 30, "{}", view.len());
        assert_eq!(view.end_id(), 1000);
        // Lines are evicted from the oldest end only, ids intact.
        for (id, l) in view.iter() {
            assert!(l.text.starts_with(&format!("{id} ")));
        }
        // Each block holds about 1/32 of the budget, so one long line can seal a block.
        assert!(buffer.block_bytes >= LogLine::max_heap_bytes());
    }

    // covers UP-40
    #[test]
    fn short_lines_hit_the_line_limit_before_the_budget() {
        let mut buffer = LogBuffer::with_budget(500, 64 * 1024 * 1024);
        for n in 0..5000 {
            buffer.push(line(n));
        }
        assert_eq!(buffer.len(), 500);
        assert!(buffer.bytes() < 1024 * 1024);
    }

    // covers UP-40
    #[test]
    fn a_tiny_budget_still_keeps_the_newest_line() {
        let mut buffer = LogBuffer::with_budget(10, 1);
        let long = "y".repeat(MAX_LINE_BYTES);
        for _ in 0..5 {
            buffer.push(LogLine::parse(&long, true));
        }
        let view = buffer.view();
        assert!(!view.is_empty());
        assert_eq!(view.end_id(), 5);
        assert!(buffer.bytes() <= 2 * LogLine::max_heap_bytes());
    }

    // covers UP-40
    #[test]
    fn trimming_a_view_releases_blocks_before_the_new_first_line() {
        let mut buffer = LogBuffer::new(10 * CHUNK_LINES);
        for n in 0..(3 * CHUNK_LINES as u64 + 7) {
            buffer.push(line(n));
        }
        let view = buffer.view();
        assert_eq!(view.blocks(), 3);
        let at = CHUNK_LINES as u64 + 5;
        let trimmed = view.trimmed(at);
        assert_eq!(trimmed.blocks(), 2);
        assert_eq!(trimmed.first_id(), at);
        assert_eq!(trimmed.end_id(), view.end_id());
        assert_eq!(&*trimmed.get(at).unwrap().text, format!("line {at}"));
        assert!(trimmed.get(at - 1).is_none());
        // On a block boundary the whole earlier block goes.
        assert_eq!(view.trimmed(2 * CHUNK_LINES as u64).blocks(), 1);
        // Past the end: empty, nothing kept.
        let gone = view.trimmed(u64::MAX);
        assert!(gone.is_empty());
        assert_eq!(gone.blocks(), 0);
        assert_eq!(view.trimmed(0).len(), view.len());
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
