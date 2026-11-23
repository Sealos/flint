//! What a log tab shows, without GPUI, so it can be unit tested: how a new snapshot changes the
//! list of lines, the text and highlights of one line (a short local timestamp, a short pod
//! prefix in the pod's colour), the session's state and errors in words, how much of the toolbar
//! fits, and the text that copy and save write.
//!
//! **Shown versus copied.** A line shows its timestamp as local `HH:MM:SS.mmm` and a workload
//! line its pod by the part of its name that tells it from its siblings. Copies and saves follow
//! what is shown (a part hidden is left out), but write each part in full: the server's RFC 3339
//! timestamp in UTC and `[pod/container]`, so a file stays unambiguous across days, time zones
//! and pods. Hovering over a shown timestamp or prefix shows the full value too.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use flint_core::logs::{ContainerWait, FailedHop, LineRanges, NETWORK_FAILURES};
use flint_core::{
    LineFormat, LineId, LineSelection, LineSource, LogLine, LogLines, LogState, LogSubject, PodSet,
    PodSetStatus, SearchQuery, WorkloadTarget,
};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use super::registry::SaveScope;

/// How the virtualized list must change to go from one snapshot's lines to the next: lines
/// leave at the oldest end (the ring buffer evicted them) and arrive at the newest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListChange {
    Unchanged,
    /// `evicted` items left the front and `appended` items arrived at the back.
    Splice {
        evicted: usize,
        appended: usize,
    },
    /// Start over with this many items (no lines in common, or ids went backwards).
    Reset(usize),
}

/// The change from lines `old` (`first_id..end_id`) to lines `new`. Ids never change, so
/// two snapshots of one session overlap on the ids both hold.
pub fn list_change(old: Range<LineId>, new: Range<LineId>) -> ListChange {
    let new_len = (new.end - new.start) as usize;
    if old == new {
        return ListChange::Unchanged;
    }
    if old.is_empty() || new.start < old.start || new.end < old.end || new.start >= old.end {
        return ListChange::Reset(new_len);
    }
    ListChange::Splice {
        evicted: (new.start - old.start) as usize,
        appended: (new.end - old.end) as usize,
    }
}

/// Lines longer than this are cut when lines are not wrapped: the rest would never be seen,
/// and shaping 64 KiB of text for every frame is not free.
pub const UNWRAPPED_MAX_BYTES: usize = 4096;

/// Characters of a shown timestamp: `HH:MM:SS.mmm`.
pub const TIME_WIDTH: usize = 12;

/// `ts` as shown: local wall-clock time to the millisecond, always [`TIME_WIDTH`] wide.
pub fn short_time(ts: Timestamp, tz: &TimeZone) -> String {
    let local = ts.to_zoned(tz.clone());
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        local.hour(),
        local.minute(),
        local.second(),
        local.millisecond()
    )
}

/// `ts` in full, for a tooltip: as the server wrote it (UTC), then in the shown time zone, named
/// by its abbreviation and offset.
pub fn full_time(ts: Timestamp, tz: &TimeZone) -> String {
    let local = ts.to_zoned(tz.clone());
    format!("{ts}\n{}", local.strftime("%Y-%m-%d %H:%M:%S%.f %:z %Z"))
}

/// The day (and UTC offset) a timestamp falls on in `tz`: a line whose day differs from the line
/// before shows a dated separator, so a tail that spans days, or a fall-back hour that repeats,
/// reads unambiguously.
pub fn day_of(ts: Timestamp, tz: &TimeZone) -> (jiff::civil::Date, i32) {
    let local = ts.to_zoned(tz.clone());
    (local.date(), local.offset().seconds())
}

/// The separator above the first line of a day: `Friday 20 November 2026 · CET (UTC+01:00)`.
pub fn day_label(ts: Timestamp, tz: &TimeZone) -> String {
    let local = ts.to_zoned(tz.clone());
    local.strftime("%A %-d %B %Y · %Z (UTC%:z)").to_string()
}

/// Earlier timestamped lines [`day_separator`] compares with, and how far back it looks for
/// them: lines of a workload's pods interleave slightly out of timestamp order, so the latest
/// of a few recent ones stands for "the day so far".
const DAY_RECENT: usize = 16;
const DAY_LOOKBACK: usize = 256;

/// The dated separator to draw above `line`, when timestamps are shown. `before` is the lines
/// before it, newest first. A separator marks where the day (or the UTC offset) moves on from
/// the latest of the recent timestamped lines: above the first timestamped line shown, and above
/// a line later than all of those whose day or offset differs. A line out of order (a pod's line
/// a few milliseconds older than its neighbours, across midnight) never starts a day, and lines
/// without a timestamp are skipped over.
pub fn day_separator<'a>(
    before: impl Iterator<Item = &'a LogLine>,
    line: &LogLine,
    tz: &TimeZone,
) -> Option<String> {
    let ts = line.timestamp?;
    let mut latest: Option<Timestamp> = None;
    let mut seen = 0;
    let mut exhausted = true;
    for (looked, prev) in before.enumerate() {
        if looked == DAY_LOOKBACK || seen == DAY_RECENT {
            exhausted = false;
            break;
        }
        if let Some(t) = prev.timestamp {
            seen += 1;
            latest = Some(latest.map_or(t, |l| l.max(t)));
        }
    }
    match latest {
        // Nothing timestamped before it: the first of the lines shown, or too far to tell.
        None => exhausted.then(|| day_label(ts, tz)),
        Some(latest) if ts > latest && day_of(ts, tz) != day_of(latest, tz) => {
            Some(day_label(ts, tz))
        }
        Some(_) => None,
    }
}

/// Who said why a previous instance's log is not available: the API server, when it found no
/// previous terminated container, else the kubelet (its one-line answer instead of the log).
pub fn previous_speaker(detail: &str) -> &'static str {
    if detail.contains("previous terminated container") {
        "The API server"
    } else {
        "The kubelet"
    }
}

/// The time zone timestamps show in, and its name for the toolbar: the system's, or UTC said as
/// such when the system's cannot be read.
pub fn system_zone() -> (TimeZone, String) {
    match TimeZone::try_system() {
        Ok(tz) => {
            let name = tz.iana_name().unwrap_or("the system time zone").to_owned();
            (tz, format!("local time ({name})"))
        }
        Err(_) => (
            TimeZone::UTC,
            "UTC (the system time zone could not be read)".to_owned(),
        ),
    }
}

/// The short pod prefixes of a workload session: each pod by the part of its name after the
/// prefix every pod shares (`cd27k` for `api-79db669d5-cd27k`), with the container only when the
/// pod has several, and a colour per pod (an index into the theme's prefix palette), assigned
/// in the order pods are first seen so the first few pods never share one. Pods are remembered
/// for the session, so a line of a pod that went keeps its label and colour.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceLabels {
    pods: BTreeMap<Box<str>, PodLabel>,
    /// Bytes at the start of every pod name the labels leave out.
    cut: usize,
    /// The widest label, in characters: shorter ones are padded so the text lines up.
    width: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PodLabel {
    containers: BTreeSet<Box<str>>,
    color: usize,
}

impl SourceLabels {
    /// Learns the pods and containers `status` reads. Whether any label changed.
    pub fn learn(&mut self, status: &PodSetStatus) -> bool {
        let mut changed = false;
        for stream in &status.streams {
            let source = &stream.source;
            // Most snapshots repeat what is known: look before allocating a key.
            if self
                .pods
                .get(source.pod())
                .is_some_and(|label| label.containers.contains(source.container()))
            {
                continue;
            }
            let next = self.pods.len();
            let pod = self.pods.entry(source.pod().into()).or_insert_with(|| {
                changed = true;
                PodLabel {
                    containers: BTreeSet::new(),
                    color: next,
                }
            });
            changed |= pod.containers.insert(source.container().into());
        }
        if changed {
            self.cut = shared_prefix(self.pods.keys().map(|p| &**p));
            let this = &*self;
            let width = this
                .pods
                .iter()
                .flat_map(|(pod, label)| {
                    label
                        .containers
                        .iter()
                        .map(move |c| this.label_of(pod, c, label).chars().count())
                })
                .max()
                .unwrap_or(0);
            self.width = width;
        }
        changed
    }

    fn label_of(&self, pod: &str, container: &str, label: &PodLabel) -> String {
        let short = pod.get(self.cut..).filter(|s| !s.is_empty()).unwrap_or(pod);
        if label.containers.len() > 1 {
            format!("{short}/{container}")
        } else {
            short.to_owned()
        }
    }

    /// `source` as its prefix shows it. A pod never seen in a status shows in full.
    pub fn short(&self, source: &LineSource) -> String {
        match self.pods.get(source.pod()) {
            Some(label) => self.label_of(source.pod(), source.container(), label),
            None => source.label().to_owned(),
        }
    }

    /// `source`'s colour: an index into the palette, to take modulo its length.
    pub fn color(&self, source: &LineSource) -> usize {
        match self.pods.get(source.pod()) {
            Some(label) => label.color,
            // Not seen in a status: stable by name.
            None => source.pod().bytes().fold(0usize, |h, b| {
                h.wrapping_mul(31).wrapping_add(usize::from(b))
            }),
        }
    }

    /// The widest label, in characters.
    pub fn width(&self) -> usize {
        self.width
    }
}

/// Bytes at the start of every name in `pods` that a short label leaves out: their common
/// prefix up to its last `-`, so a label never starts mid-word (`api-79db669d5-` of
/// `api-79db669d5-cd27k` and `api-79db669d5-6zn94`, also of a pod alone). `0` when that would
/// leave a name empty.
fn shared_prefix<'a>(mut pods: impl Iterator<Item = &'a str>) -> usize {
    let Some(first) = pods.next() else {
        return 0;
    };
    let mut names = vec![first];
    let mut common = first.len();
    for pod in pods {
        names.push(pod);
        common = first
            .bytes()
            .zip(pod.bytes())
            .take(common)
            .take_while(|(a, b)| a == b)
            .count();
    }
    let cut = first[..common].rfind('-').map_or(0, |at| at + 1);
    if names.iter().any(|n| n.len() <= cut) {
        0
    } else {
        cut
    }
}

/// How lines are shown now: which parts, the pod prefixes, and the time zone of timestamps.
#[derive(Debug, Clone, Copy)]
pub struct LineView<'a> {
    pub format: LineFormat,
    pub labels: &'a SourceLabels,
    pub tz: &'a TimeZone,
}

/// One line as drawn: its text, with the short `[pod]` prefix and the short timestamp in front
/// when shown, where those end, and the byte ranges of search matches in that text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownLine {
    pub text: String,
    /// The source prefix, its padding and the space after it; `0` when not shown (or a pod
    /// session).
    pub source: usize,
    /// Where the line's own text starts: after the source and the timestamp.
    pub prefix: usize,
    pub matches: Vec<Range<usize>>,
    /// The line was cut: at [`flint_core::logs::MAX_LINE_BYTES`] when it arrived, or here at
    /// `max_bytes`.
    pub cut: bool,
    /// The prefix's colour, as [`SourceLabels::color`].
    pub color: Option<usize>,
    /// The line may hold matches that are not highlighted.
    pub limited: bool,
}

/// `line` as shown with `view`: its search matches (in the prefix too, when the query searches
/// sources and the prefix is shown), and at most `max_bytes` of its text (on a character
/// boundary). A line without a timestamp gets blanks in its place, so the text lines up.
#[cfg(test)]
pub fn shown_line(
    line: &LogLine,
    view: LineView<'_>,
    query: &SearchQuery,
    max_bytes: Option<usize>,
) -> ShownLine {
    shown_line_with(line, view, max_bytes, |short| LineMatches {
        source: short.map_or_else(Vec::new, |short| source_matches(query, short, line)),
        body: query.match_ranges(&line.text),
        limited: false,
    })
}

/// What a search found in one line, before it is laid out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineMatches {
    /// In the short source label.
    pub source: Vec<Range<usize>>,
    /// In the whole message, so that `$` and `\b` see its real end when it is drawn clipped.
    pub body: Vec<Range<usize>>,
    /// A limit, or a scan still to come, may have left matches out of `body`.
    pub limited: bool,
}

/// Where `query` matches the short label `short` of `line`'s source. A match only in the part
/// of the name the label leaves out marks the whole label.
fn source_matches(query: &SearchQuery, short: &str, line: &LogLine) -> Vec<Range<usize>> {
    if !query.sources {
        return Vec::new();
    }
    let found = query.match_ranges(short);
    if !found.is_empty() {
        return found;
    }
    match &line.source {
        Some(source) if !query.match_ranges(source.label()).is_empty() => {
            std::iter::once(0..short.len()).collect()
        }
        _ => Vec::new(),
    }
}

/// Most time a frame spends finding highlights, on top of the one search that crosses it.
const HIGHLIGHT_FRAME_TIME: std::time::Duration = std::time::Duration::from_millis(4);
/// Most lines whose highlights are kept: several screens of rows.
const HIGHLIGHT_CACHE_LINES: usize = 1024;
/// Most long lines one background highlight scan covers: about a screen of rows.
const WHOLE_LINE_BATCH: usize = 64;

#[derive(Debug, Default)]
struct CachedMatches {
    /// With the short label they were found in: labels shorten as more pods are learned.
    source: Option<(String, Vec<Range<usize>>)>,
    body: Option<Vec<Range<usize>>>,
    /// Its whole-line matches were asked for (or have landed), so it is not asked for again.
    whole: bool,
    /// Its whole-line matches have landed.
    landed: bool,
    /// A limit stopped the search that found `body`.
    limited: bool,
}

/// The search's matches in the lines drawn: found once per line and query, and for at most
/// [`HIGHLIGHT_FRAME_TIME`] per frame. A row past that is drawn plain and asks for another frame.
/// A regex tests only the start of a line here; a longer line drawn whole is listed for a
/// background scan ([`LineHighlights::take_wanted`]).
#[derive(Debug)]
pub struct LineHighlights {
    query: SearchQuery,
    /// How much of a line is drawn: what was found depends on it.
    drawn: Option<usize>,
    lines: std::collections::HashMap<flint_core::LineId, CachedMatches>,
    budget: std::time::Duration,
    spent: std::time::Duration,
    /// A row went without its highlights this frame.
    deferred: bool,
    /// The next frame has been asked for.
    redraw_asked: bool,
    /// Long lines drawn whose whole-line matches are still to be found.
    wanted: Vec<flint_core::LineId>,
    /// A line joined `wanted` since a frame was last asked for.
    wanted_new: bool,
}

impl Default for LineHighlights {
    fn default() -> Self {
        LineHighlights {
            query: SearchQuery::default(),
            drawn: None,
            lines: Default::default(),
            budget: HIGHLIGHT_FRAME_TIME,
            spent: std::time::Duration::ZERO,
            deferred: false,
            redraw_asked: false,
            wanted: Vec::new(),
            wanted_new: false,
        }
    }
}

/// The start of `text` that can hold a plain match reaching the first `max` bytes: one match
/// covers at most four bytes for each char of the query, however its case folds.
fn drawn_window<'a>(text: &'a str, query: &SearchQuery, max: Option<usize>) -> &'a str {
    let Some(max) = max.filter(|_| !query.regex) else {
        return text;
    };
    let mut end = max
        .saturating_add(query.text.chars().count().saturating_mul(4))
        .min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl LineHighlights {
    pub fn begin_frame(&mut self) {
        self.spent = std::time::Duration::ZERO;
        self.deferred = false;
        self.redraw_asked = false;
    }

    /// Whether to ask for another frame: once per frame, when a row went without highlights or
    /// a long line was newly listed for the background scan.
    pub fn take_redraw(&mut self) -> bool {
        let ask = (self.deferred || self.wanted_new) && !self.redraw_asked;
        self.redraw_asked |= ask;
        self.wanted_new &= !ask;
        ask
    }

    /// The long lines to scan whole, at most [`WHOLE_LINE_BATCH`]; each is handed out once.
    pub fn take_wanted(&mut self) -> Vec<flint_core::LineId> {
        let wanted = std::mem::take(&mut self.wanted);
        for id in &wanted {
            if let Some(entry) = self.lines.get_mut(id) {
                entry.whole = true;
            }
        }
        wanted
    }

    /// Keeps what a background scan found for `query`, unless what is drawn has changed since.
    pub fn store_whole(
        &mut self,
        query: &SearchQuery,
        found: Vec<(flint_core::LineId, flint_core::logs::LineRanges)>,
    ) {
        if self.query != *query || self.drawn.is_some() {
            return;
        }
        for (id, found) in found {
            if let Some(entry) = self.lines.get_mut(&id) {
                entry.body = Some(found.ranges);
                entry.limited = !found.complete;
                entry.whole = true;
                entry.landed = true;
            }
        }
    }

    /// Forgets every line's matches and what was asked of the background scan.
    pub fn forget(&mut self) {
        self.lines.clear();
        self.wanted.clear();
        self.wanted_new = false;
    }

    /// What `query` found in line `id`, as far as this frame's budget reaches. `max` is how many
    /// bytes of the message are drawn (`None`: all of them).
    pub fn matches(
        &mut self,
        id: flint_core::LineId,
        line: &LogLine,
        query: &SearchQuery,
        short: Option<&str>,
        max: Option<usize>,
    ) -> LineMatches {
        if query.text.is_empty() {
            return LineMatches::default();
        }
        if self.query != *query || self.drawn != max {
            self.query = query.clone();
            self.drawn = max;
            self.lines.clear();
            self.wanted.clear();
        }
        if self.lines.len() >= HIGHLIGHT_CACHE_LINES && !self.lines.contains_key(&id) {
            self.lines.clear();
            self.wanted.clear();
        }
        let Self {
            lines,
            budget,
            spent,
            deferred,
            wanted,
            wanted_new,
            ..
        } = self;
        let entry = lines.entry(id).or_default();
        let drawn_past_cut =
            query.regex && max.is_none() && line.text.len() > flint_core::logs::HIGHLIGHT_BYTES;
        if drawn_past_cut
            && !entry.whole
            && wanted.len() < WHOLE_LINE_BATCH
            && !wanted.contains(&id)
        {
            wanted.push(id);
            *wanted_new = true;
        }
        let mut find = |search: &dyn Fn() -> LineRanges| {
            if *spent >= *budget {
                *deferred = true;
                return None;
            }
            let started = std::time::Instant::now();
            let found = search();
            *spent += started.elapsed();
            Some(found)
        };
        if entry.body.is_none()
            && let Some(found) =
                find(&|| query.highlight_ranges(drawn_window(&line.text, query, max)))
        {
            entry.limited = !found.complete;
            entry.body = Some(found.ranges);
        }
        let source = match short.filter(|_| query.sources) {
            Some(short) => {
                if entry.source.as_ref().is_none_or(|(s, _)| s != short) {
                    entry.source = find(&|| LineRanges {
                        ranges: source_matches(query, short, line),
                        complete: true,
                    })
                    .map(|found| (short.to_owned(), found.ranges));
                }
                entry
                    .source
                    .as_ref()
                    .map_or_else(Vec::new, |(_, found)| found.clone())
            }
            None => Vec::new(),
        };
        LineMatches {
            source,
            limited: entry.limited || entry.body.is_none() || (drawn_past_cut && !entry.landed),
            body: entry.body.clone().unwrap_or_default(),
        }
    }
}

/// `shown_line` with the matches from `find`, which is given the short source label when it is
/// shown. Matches in the message are clipped to the part of it drawn.
pub fn shown_line_with(
    line: &LogLine,
    view: LineView<'_>,
    max_bytes: Option<usize>,
    find: impl FnOnce(Option<&str>) -> LineMatches,
) -> ShownLine {
    let shown_source = line.source.as_ref().filter(|_| view.format.source);
    let short = shown_source.map(|source| view.labels.short(source));
    let found = find(short.as_deref());
    let mut text = String::new();
    let mut matches = Vec::new();
    let mut color = None;
    if let (Some(source), Some(short)) = (shown_source, &short) {
        text.push('[');
        matches.extend(found.source.iter().map(|r| r.start + 1..r.end + 1));
        text.push_str(short);
        text.push(']');
        let pad = view.labels.width().saturating_sub(short.chars().count());
        text.extend(std::iter::repeat_n(' ', pad + 1));
        color = Some(view.labels.color(source));
    }
    let source = text.len();
    if view.format.timestamps {
        match line.timestamp {
            Some(ts) => text.push_str(&short_time(ts, view.tz)),
            None => text.extend(std::iter::repeat_n(' ', TIME_WIDTH)),
        }
        text.push(' ');
    }
    let prefix = text.len();
    let mut body: &str = &line.text;
    let mut cut = line.truncated;
    if let Some(max) = max_bytes
        && body.len() > max
    {
        let mut end = max;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body = &body[..end];
        cut = true;
    }
    text.push_str(body);
    let limited = found.limited;
    matches.extend(
        found
            .body
            .into_iter()
            .filter(|r| r.start < body.len())
            .map(|r| r.start + prefix..r.end.min(body.len()) + prefix),
    );
    ShownLine {
        text,
        source,
        prefix,
        matches,
        cut,
        color,
        limited,
    }
}

/// What a styled run of a shown line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Source,
    Timestamp,
    Match,
}

/// The styled runs `parts` (of [`line_parts`]) inside `segment` (the prefix, the timestamp or the
/// text), with ranges relative to the segment: what one of the line's three drawn parts highlights.
pub fn parts_in(
    parts: &[(Range<usize>, Part)],
    segment: Range<usize>,
) -> Vec<(Range<usize>, Part)> {
    parts
        .iter()
        .cloned()
        .filter_map(|(range, part)| {
            let start = range.start.max(segment.start);
            let end = range.end.min(segment.end);
            (start < end).then(|| (start - segment.start..end - segment.start, part))
        })
        .collect()
}

/// The styled runs of `shown`, in order and not overlapping: the source prefix and the timestamp,
/// split around any matches inside them, then every match. The rest is plain text.
pub fn line_parts(shown: &ShownLine) -> Vec<(Range<usize>, Part)> {
    let mut out = Vec::with_capacity(shown.matches.len() + 2);
    let mut matches = shown.matches.iter().peekable();
    for (range, part) in [
        (0..shown.source, Part::Source),
        (shown.source..shown.prefix, Part::Timestamp),
    ] {
        let mut at = range.start;
        while let Some(m) = matches.next_if(|m| m.start < range.end) {
            if at < m.start {
                out.push((at..m.start, part));
            }
            out.push((m.clone(), Part::Match));
            at = m.end;
        }
        if at < range.end {
            out.push((at..range.end, part));
        }
    }
    out.extend(matches.map(|m| (m.clone(), Part::Match)));
    out
}

/// The tone a state is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Muted,
    Live,
    Warning,
    Danger,
}

/// An error in plain words: a few for the status line, a sentence for the banner, and the
/// server's own text as the detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explained {
    /// Lower case, for "Ended · no permission".
    pub short: &'static str,
    pub headline: String,
    pub detail: Option<String>,
}

/// Says what a log session's `error` means. `hop`: the part that failed, as the session read it
/// from the full error. `pod`: the session reads one pod (else a workload's pods).
pub fn explain(error: &str, hop: FailedHop, pod: bool) -> Explained {
    let lower = error.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    let (short, headline) = if lower.starts_with("forbidden") || has(&["denied access (403"]) {
        (
            "no permission",
            if pod {
                "You don't have permission to read this pod's logs."
            } else {
                "You don't have permission to read these pods' logs."
            },
        )
    } else if lower.starts_with("unauthorized") || has(&["rejected the credentials"]) {
        (
            "credentials rejected",
            "The API server rejected your credentials.",
        )
    } else if hop == FailedHop::Node {
        // The API server answered, but could not reach the kubelet it proxies the log from.
        (
            "can't reach the node",
            if pod {
                "Can't reach the node running this pod."
            } else {
                "Can't reach the node running one of these pods."
            },
        )
    } else if has(NETWORK_FAILURES) {
        ("can't reach the API server", "Can't reach the API server.")
    } else if has(&["previous terminated container"]) {
        ("not available", PREVIOUS_NOT_AVAILABLE)
    } else if lower.starts_with("notfound") || has(&["not found"]) {
        (
            "not found",
            if pod {
                "The pod or container was not found."
            } else {
                "The workload was not found."
            },
        )
    } else {
        ("error", "Couldn't read the logs.")
    };
    Explained {
        short,
        headline: headline.to_owned(),
        detail: Some(error.to_owned()),
    }
}

/// What the status line says: a few words in a tone, and the full story for its tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusText {
    pub tone: Tone,
    pub text: String,
    pub tooltip: Option<String>,
}

/// The session's state in a few words, and the tone to draw it in. Errors are said by
/// [`explain`]: a few words here, the rest in the tooltip and the banner.
pub fn state_text(state: &LogState, pod: bool) -> StatusText {
    let status = |tone, text: &str| StatusText {
        tone,
        text: text.to_owned(),
        tooltip: None,
    };
    let explained = |e: &str, hop: FailedHop| {
        let ex = explain(e, hop, pod);
        let tooltip = match &ex.detail {
            Some(detail) => format!("{}\n{detail}", ex.headline),
            None => ex.headline.clone(),
        };
        (ex, tooltip)
    };
    match state {
        LogState::Connecting => status(Tone::Muted, "Connecting…"),
        LogState::Streaming => status(Tone::Live, "Streaming"),
        LogState::Reconnecting { back_off: true, error, .. } => StatusText {
            tone: Tone::Warning,
            text: BACK_OFF.to_owned(),
            tooltip: Some(match error {
                Some(e) => format!("The container is in CrashLoopBackOff: the kubelet restarts it after a delay that grows with each crash.\n{e}"),
                None => "The container is in CrashLoopBackOff: the kubelet restarts it after a delay that grows with each crash.".to_owned(),
            }),
        },
        LogState::Waiting { wait } => StatusText {
            tone: Tone::Warning,
            text: wait_text(wait),
            tooltip: Some(format!("{}\n{WAITING_DETAIL}", wait_text(wait))),
        },
        LogState::Reconnecting { error, hop, attempt, .. } => match error {
            Some(e) => {
                let (ex, tooltip) = explained(e, *hop);
                StatusText {
                    tone: Tone::Warning,
                    text: match ex.short {
                        "error" => format!("Reconnecting (attempt {attempt}) · {}", gist(e)),
                        short => format!("Reconnecting (attempt {attempt}) · {short}"),
                    },
                    tooltip: Some(tooltip),
                }
            }
            None => status(Tone::Warning, &format!("Reconnecting (attempt {attempt})")),
        },
        LogState::Ended { .. } if state.previous_not_available().is_some() => {
            let kubelet = state.previous_not_available().unwrap_or_default();
            StatusText {
                tone: Tone::Warning,
                text: "Ended · not available".to_owned(),
                tooltip: Some(format!("{PREVIOUS_NOT_AVAILABLE}\n{kubelet}")),
            }
        }
        LogState::Ended { error: Some(e), hop, .. } => {
            let (ex, tooltip) = explained(e, *hop);
            StatusText {
                tone: Tone::Danger,
                text: format!("Ended · {}", ex.short),
                tooltip: Some(tooltip),
            }
        }
        // An ordinary end, not a failure: the pod went after everything it logged was read.
        LogState::Ended {
            pod_deleted: true, ..
        } => status(Tone::Muted, "Ended · pod deleted"),
        LogState::Ended { .. } => status(Tone::Muted, "Ended"),
    }
}

/// Most characters of an unrecognised error the status line shows.
const GIST_CHARS: usize = 40;

/// An error the status line cannot say in plain words, cut short: its first line, at most
/// [`GIST_CHARS`] characters (the whole is in the tooltip).
pub fn gist(error: &str) -> String {
    let line = error.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= GIST_CHARS {
        line.to_owned()
    } else {
        let cut: String = line.chars().take(GIST_CHARS - 1).collect();
        format!("{}…", cut.trim_end())
    }
}

/// What a tab says while its container has not started.
pub fn wait_text(wait: &ContainerWait) -> String {
    match wait {
        ContainerWait::Unscheduled => "Pod isn't scheduled yet".to_owned(),
        ContainerWait::NotStarted(Some(reason)) => format!("Container hasn't started: {reason}"),
        ContainerWait::NotStarted(None) => "Container hasn't started".to_owned(),
    }
}

/// The rest of what a tab says while its container has not started.
pub const WAITING_DETAIL: &str = "Flint keeps checking. Its logs appear here once it starts.";

/// What a crash-looping container's tab says while the kubelet waits to restart it.
pub const BACK_OFF: &str = "Waiting for the container to restart (back-off)";

/// What a previous-instance tab says when that log could not be read: the container may still
/// be restarting, or may have been removed (garbage-collected, or its node restarted).
pub const PREVIOUS_NOT_AVAILABLE: &str =
    "The previous instance's logs aren't available (the container may have been removed).";

/// Whether the session is over: nothing more will come, so pausing means nothing.
pub fn has_ended(state: &LogState) -> bool {
    matches!(state, LogState::Ended { .. })
}

/// How much of the toolbar fits a log tab `width` wide (unknown before the first layout). The
/// toolbar is one row at any width: controls give way rather than wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolbarFit {
    /// The view toggles and Save are drawn as buttons; else they are in the "⋯" menu.
    pub toggles: bool,
    /// The Jump button says "Jump to newest" beside its arrow.
    pub jump_label: bool,
    /// The line count and a workload's stream counts are shown.
    pub counts: bool,
    /// The search's "3 of 17" is shown beside its arrows.
    pub match_count: bool,
}

/// Widths below which the toolbar gives up a part (see [`toolbar_fit`]), chosen so the parts
/// left fit with the status and search at their narrowest.
pub const FIT_FULL: f32 = 980.;
pub const FIT_COUNTS: f32 = 640.;
pub const FIT_MATCH_COUNT: f32 = 480.;

/// What of the toolbar fits `width`: everything from [`FIT_FULL`]; below, the toggles and Save
/// go into the "⋯" menu and Jump loses its words; below [`FIT_COUNTS`], the counts go (they stay
/// in the status's tooltip); below [`FIT_MATCH_COUNT`], the match count goes too.
pub fn toolbar_fit(width: Option<f32>) -> ToolbarFit {
    let width = width.unwrap_or(f32::INFINITY);
    ToolbarFit {
        toggles: width >= FIT_FULL,
        jump_label: width >= FIT_FULL,
        counts: width >= FIT_COUNTS,
        match_count: width >= FIT_MATCH_COUNT,
    }
}

/// What a tab says once the shared log memory budget made it drop lines: so a short history
/// is not taken for a quiet pod.
pub const TRIMMED_TEXT: &str = "Older lines dropped to save memory";
/// The same, in full, in the status's tooltip.
pub const TRIMMED_TOOLTIP: &str = "Older lines were dropped to keep all log tabs within \
                                   log_total_mib; the tab shown is trimmed last";

/// What an ended tab says once the shared budget dropped every line it had.
pub const ALL_LINES_DROPPED: &str = "All lines dropped to save memory.";

/// Whether `snapshot` is an ended tab whose lines the budget dropped, all of them. A paused tab
/// whose frozen view emptied while newer lines wait is not: they show when it resumes.
pub fn all_lines_dropped(snapshot: &flint_core::LogSnapshot) -> bool {
    matches!(snapshot.state, flint_core::LogState::Ended { .. })
        && snapshot.trimmed
        && snapshot.lines.is_empty()
        && snapshot.held_back == 0
}

/// An empty ended tab's `text` and `detail`, its lines having all been dropped. A failure keeps
/// its explanation, with the notice under it; otherwise the notice is the headline, and what
/// else the tab says (the pod was deleted) stays.
pub fn with_lines_dropped(
    text: String,
    detail: Option<String>,
    failed: bool,
) -> (String, Option<String>) {
    if failed {
        let detail = match detail {
            Some(detail) => format!("{detail}\n{ALL_LINES_DROPPED}"),
            None => ALL_LINES_DROPPED.to_owned(),
        };
        return (text, Some(detail));
    }
    let detail = match detail {
        Some(detail) => format!("{detail} {TRIMMED_TOOLTIP}"),
        None => TRIMMED_TOOLTIP.to_owned(),
    };
    (ALL_LINES_DROPPED.to_owned(), Some(detail))
}

/// What the pause indicator says: why it is paused, and how many lines wait.
pub fn pause_text(held_back: u64, by_selection: bool) -> String {
    let why = if by_selection {
        "Paused while selecting"
    } else {
        "Paused"
    };
    match held_back {
        0 => why.to_owned(),
        1 => format!("{why} · 1 new line"),
        n => format!("{why} · {} new lines", thousands(n)),
    }
}

/// "1 line", "12,345 lines".
pub fn line_count(n: u64) -> String {
    match n {
        1 => "1 line".to_owned(),
        n => format!("{} lines", thousands(n)),
    }
}

/// `n` with thousands separators.
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The context sizes the toolbar offers (any up to 100 from the palette).
pub const CONTEXT_CHOICES: [usize; 6] = [0, 1, 2, 3, 5, 10];
/// The same, as the "⋯" menu lists them.
const CONTEXT_LABELS: [&str; 6] = [
    "No context",
    "1 line",
    "2 lines",
    "3 lines",
    "5 lines",
    "10 lines",
];

/// What a filtered tab with no rows says: which toggles filter it.
pub fn filter_empty_text(spec: &flint_core::FilterSpec) -> String {
    match (spec.query.is_some(), spec.errors_only) {
        (true, true) => "No warnings or errors match the search".to_owned(),
        (true, false) => "No lines match the search".to_owned(),
        (false, true) => "No warnings or errors".to_owned(),
        (false, false) => "No lines".to_owned(),
    }
}

/// The toolbar's count: "1,234 lines", or "12 of 1,234 lines" while filtered.
pub fn shown_count(shown: Option<usize>, total: usize) -> String {
    match shown {
        None => line_count(total as u64),
        Some(shown) => format!(
            "{} of {}",
            thousands(shown as u64),
            line_count(total as u64)
        ),
    }
}

/// "3 of 17" when `current` is a match, "17 matches" otherwise, "No matches" for none.
#[cfg(test)]
pub fn match_text(search: &flint_core::LogSearch, current: Option<LineId>) -> String {
    let at = current.and_then(|id| search.matches().binary_search(&id).ok());
    match_count_text(search.count(), at)
}

/// "3 of 17" when the current match is the `at`-th (from 0) of `count`, "17 matches" otherwise.
pub fn match_count_text(count: usize, at: Option<usize>) -> String {
    match (count, at) {
        (0, _) => "No matches".to_owned(),
        (n, Some(i)) => format!("{} of {}", thousands(i as u64 + 1), thousands(n as u64)),
        (1, None) => "1 match".to_owned(),
        (n, None) => format!("{} matches", thousands(n as u64)),
    }
}

/// A view toggle a log tab's "⋯" menu offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    Timestamps,
    Sources,
    Wrap,
    ErrorsOnly,
    Regex,
    MatchingOnly,
    /// Lines of context around each row a filter shows.
    Context(usize),
}

/// A row of a log tab's toolbar menu (Save, or "⋯" in a narrow tab).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuRow {
    /// A note, not an entry.
    Label(&'static str),
    Separator,
    Toggle {
        label: &'static str,
        on: bool,
        toggle: Toggle,
    },
    Save {
        label: &'static str,
        scope: SaveScope,
        enabled: bool,
    },
}

/// What a log tab's toolbar menus depend on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuFacts {
    pub timestamps: bool,
    pub workload: bool,
    pub sources: bool,
    pub wrap: bool,
    pub has_lines: bool,
    pub has_matches: bool,
    pub has_selection: bool,
    /// Only warnings and errors are shown.
    pub errors_only: bool,
    /// The search is read as a regular expression.
    pub regex: bool,
    /// Only the lines matching the search are shown.
    pub matching_only: bool,
    /// Lines of context, while a filter is on.
    pub context: Option<usize>,
}

/// The Save menu: how lines are written, then a Save entry per scope.
pub fn save_menu(f: &MenuFacts) -> Vec<MenuRow> {
    let save = |label, scope, enabled| MenuRow::Save {
        label,
        scope,
        enabled,
    };
    vec![
        MenuRow::Label(if f.timestamps {
            "As shown, with full timestamps. Not redacted."
        } else {
            "As shown, without timestamps. Not redacted."
        }),
        save("Save All Lines…", SaveScope::All, true),
        save("Save Matching Lines…", SaveScope::Matches, f.has_matches),
        save(
            "Save Selected Lines…",
            SaveScope::Selection,
            f.has_selection,
        ),
    ]
}

/// What a toolbar menu offers: its entries, without its note and separators.
pub fn menu_entries(rows: &[MenuRow]) -> Vec<super::menu_record::Entry> {
    use super::menu_record::Entry;
    rows.iter()
        .filter_map(|row| match row {
            MenuRow::Label(_) | MenuRow::Separator => None,
            MenuRow::Toggle { label, on, .. } => Some(Entry::toggle(*label, *on)),
            MenuRow::Save { label, enabled, .. } => Some(Entry::new(*label, *enabled)),
        })
        .collect()
}

/// The "⋯" menu of a narrow tab: the view toggles, then Save's entries once there are lines.
pub fn more_menu(f: &MenuFacts) -> Vec<MenuRow> {
    let toggle = |label, on, toggle| MenuRow::Toggle { label, on, toggle };
    let mut rows = vec![toggle("Timestamps", f.timestamps, Toggle::Timestamps)];
    if f.workload {
        rows.push(toggle("Pod Prefix", f.sources, Toggle::Sources));
    }
    rows.push(toggle("Wrap Lines", f.wrap, Toggle::Wrap));
    rows.push(toggle(
        "Errors and Warnings Only",
        f.errors_only,
        Toggle::ErrorsOnly,
    ));
    // The search's toggles, which a narrow toolbar leaves out.
    rows.push(MenuRow::Separator);
    rows.push(toggle("Regex Search", f.regex, Toggle::Regex));
    rows.push(toggle(
        "Matching Lines Only",
        f.matching_only,
        Toggle::MatchingOnly,
    ));
    if let Some(context) = f.context {
        rows.push(MenuRow::Label("Lines around each:"));
        for (lines, label) in CONTEXT_CHOICES.into_iter().zip(CONTEXT_LABELS) {
            rows.push(toggle(label, lines == context, Toggle::Context(lines)));
        }
    }
    rows.push(MenuRow::Separator);
    if f.has_lines {
        rows.extend(save_menu(f));
    }
    rows
}

/// How tall the "⋯" menu may be in a window `viewport` tall: it scrolls past that.
pub fn menu_max_height(viewport: gpui_kit::Pixels) -> gpui_kit::Pixels {
    (viewport - gpui_kit::px(48.)).max(gpui_kit::px(120.))
}

/// A workload session's streams in a few words: `3 pods · 4 live · 1 failed · 2 not read`.
/// Counts that are zero are left out, except the pods and live streams.
pub fn streams_text(status: &PodSetStatus) -> String {
    let pods = match status.pods {
        1 => "1 pod".to_owned(),
        n => format!("{} pods", thousands(n as u64)),
    };
    let mut parts = vec![pods, format!("{} live", thousands(status.live() as u64))];
    for (n, what) in [
        (status.connecting(), "connecting"),
        (status.waiting, "waiting to start"),
        (status.failed(), "failed"),
        (status.capped, "not read"),
    ] {
        if n > 0 {
            parts.push(format!("{} {what}", thousands(n as u64)));
        }
    }
    parts.join(" · ")
}

/// What a session reads, as a tab names it: `web-1`, `deployment/web`, `-l app=web`.
fn subject_name(subject: &LogSubject) -> (String, Option<&str>) {
    match subject {
        LogSubject::Pod(target) => (target.pod.clone(), target.container.as_deref()),
        LogSubject::Workload(target) => (
            match &target.pods {
                PodSet::Workload { kind, name } => {
                    format!("{}/{name}", kind.name().to_lowercase())
                }
                PodSet::Selector(selector) => format!("-l {selector}"),
            },
            target.container.as_deref(),
        ),
    }
}

/// A tab's title: the pod (or workload), the container when named, and "previous" for a
/// terminated instance.
pub fn tab_title(subject: &LogSubject, previous: bool) -> String {
    let (name, container) = match subject {
        // `web (Deployment)`: the name first, as for a pod; the kind after it.
        LogSubject::Workload(WorkloadTarget {
            pods: PodSet::Workload { kind, name },
            container,
            ..
        }) => (format!("{name} ({})", kind.name()), container.as_deref()),
        _ => subject_name(subject),
    };
    let mut title = match container {
        Some(container) => format!("{name} · {container}"),
        None => name,
    };
    if previous {
        title.push_str(" (previous)");
    }
    title
}

/// The file name a save dialog suggests: `web-1_app.log`, `deployment_web.log`,
/// `web-1_app_previous.log`. Only characters safe in a file name are kept.
pub fn suggested_file_name(subject: &LogSubject, previous: bool) -> String {
    let (name, container) = subject_name(subject);
    let mut stem = name.trim_start_matches("-l ").to_owned();
    if let Some(container) = container {
        stem.push('_');
        stem.push_str(container);
    }
    if previous {
        stem.push_str("_previous");
    }
    let stem: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{stem}.log")
}

/// How many lines [`save_text`] writes for `scope`, without building the text: cheap enough
/// for the UI thread.
pub fn save_count(
    lines: &LogLines,
    scope: SaveScope,
    matches: &[LineId],
    selection: Option<LineSelection>,
) -> usize {
    match scope {
        SaveScope::All => lines.len(),
        SaveScope::Matches => matches.iter().filter(|id| lines.contains(**id)).count(),
        SaveScope::Selection => selection.map_or(0, |s| s.present_len(lines) as usize),
    }
}

/// The most a save hands to the file at once, and so holds formatted in memory.
const SAVE_WRITE_BYTES: usize = 64 * 1024;

/// Writes the lines [`save_text`] would return to `out`, and returns how many there were.
pub fn write_saved(
    out: impl std::io::Write,
    lines: &LogLines,
    scope: SaveScope,
    matches: &[LineId],
    selection: Option<LineSelection>,
    format: LineFormat,
) -> std::io::Result<usize> {
    use std::io::Write as _;
    let mut out = std::io::BufWriter::with_capacity(SAVE_WRITE_BYTES, out);
    let count = for_each_saved(lines, scope, matches, selection, |line| {
        out.write_all(line.display_as(format).as_bytes())?;
        out.write_all(b"\n")
    })?;
    out.flush()?;
    Ok(count)
}

/// Calls `visit` with each line of `scope`, stopping at its first error, and returns how many
/// lines it took.
fn for_each_saved(
    lines: &LogLines,
    scope: SaveScope,
    matches: &[LineId],
    selection: Option<LineSelection>,
    mut visit: impl FnMut(&LogLine) -> std::io::Result<()>,
) -> std::io::Result<usize> {
    let mut count = 0;
    let mut step = |line: &LogLine| {
        visit(line)?;
        count += 1;
        Ok::<(), std::io::Error>(())
    };
    match (scope, selection) {
        (SaveScope::All, _) => lines.iter().try_for_each(|(_, line)| step(line))?,
        (SaveScope::Matches, _) => matches
            .iter()
            .filter_map(|id| lines.get(*id))
            .try_for_each(&mut step)?,
        (SaveScope::Selection, Some(selection)) => selection
            .lines(lines)
            .try_for_each(|(_, line)| step(line))?,
        (SaveScope::Selection, None) => {}
    }
    Ok(count)
}

/// The lines to save, as shown with `format`, one per line and ending in a newline, and how
/// many there are. Not redacted: the file holds what the tab showed.
/// `None` when the scope has nothing (no search, no selection).
pub fn save_text(
    lines: &LogLines,
    scope: SaveScope,
    matches: &[LineId],
    selection: Option<LineSelection>,
    format: LineFormat,
) -> Option<(String, usize)> {
    let mut out = String::new();
    let count = for_each_saved(lines, scope, matches, selection, |line| {
        out.push_str(&line.display_as(format));
        out.push('\n');
        Ok(())
    })
    .ok()?;
    (count > 0).then_some((out, count))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flint_core::logs::LogBuffer;
    use flint_core::{LogTarget, StreamStatus, WorkloadKind, WorkloadTarget};

    use super::*;
    use flint_core::LogSearch;

    /// Timestamps on or off, no source prefix.
    fn ts(timestamps: bool) -> LineFormat {
        LineFormat {
            timestamps,
            source: false,
        }
    }

    /// `line` shown with `format`, no pod labels learned, timestamps in UTC.
    fn show(
        line: &LogLine,
        format: LineFormat,
        query: &SearchQuery,
        max: Option<usize>,
    ) -> ShownLine {
        let labels = SourceLabels::default();
        let view = LineView {
            format,
            labels: &labels,
            tz: &TimeZone::UTC,
        };
        shown_line(line, view, query, max)
    }

    fn lines(texts: &[&str]) -> LogLines {
        let mut buffer = LogBuffer::new(100);
        for t in texts {
            buffer.push(LogLine::parse(t, false));
        }
        buffer.view()
    }

    // covers UP-40: following appends and evicts without redrawing the whole list.
    #[test]
    fn snapshots_become_splices_of_the_list() {
        use ListChange::*;
        assert_eq!(list_change(0..0, 0..0), Unchanged);
        assert_eq!(list_change(0..0, 0..5), Reset(5));
        assert_eq!(
            list_change(0..5, 0..8),
            Splice {
                evicted: 0,
                appended: 3
            }
        );
        assert_eq!(
            list_change(0..5, 2..9),
            Splice {
                evicted: 2,
                appended: 4
            }
        );
        // Nothing in common (a long pause on a chatty pod), or ids went backwards.
        assert_eq!(list_change(0..5, 7..9), Reset(2));
        assert_eq!(list_change(3..5, 0..9), Reset(9));
        assert_eq!(list_change(3..9, 3..5), Reset(2));
        assert_eq!(list_change(5..5, 5..6), Reset(1));
    }

    // covers UP-40, UP-43: timestamps come and go without reloading; matches stay on the text.
    #[test]
    fn a_shown_line_offsets_matches_past_the_timestamp() {
        let line = LogLine::parse("2024-05-01T12:00:00Z GET /api error 500 ERROR", false);
        let query = SearchQuery::new("error");
        let plain = show(&line, ts(false), &query, None);
        assert_eq!(plain.text, "GET /api error 500 ERROR");
        assert_eq!(plain.prefix, 0);
        assert_eq!(plain.matches, [9..14, 19..24]);
        let stamped = show(&line, ts(true), &query, None);
        assert_eq!(&stamped.text[..stamped.prefix], "12:00:00.000 ");
        assert_eq!(&stamped.text[stamped.prefix..], plain.text);
        for (a, b) in stamped.matches.iter().zip(&plain.matches) {
            assert_eq!(&stamped.text[a.clone()], &plain.text[b.clone()]);
        }
        // A search never matches inside the timestamp.
        let in_ts = show(&line, ts(true), &SearchQuery::new("2024"), None);
        assert!(in_ts.matches.is_empty());
        assert!(!plain.cut);
    }

    // covers UP-45: a workload line shows its `[pod/container]` source, as a view toggle, and a
    // search over sources highlights it there.
    #[test]
    fn a_workload_line_shows_its_source_before_the_timestamp() {
        let mut line = LogLine::parse("2024-05-01T12:00:00Z error in web", false);
        line.source = Some(Arc::new(LineSource::new("web-1", "app")));
        let both = LineFormat {
            timestamps: true,
            source: true,
        };
        let query = SearchQuery {
            sources: true,
            ..SearchQuery::new("web")
        };
        let shown = show(&line, both, &query, None);
        // Copies write the timestamp in full; the line shows it short.
        assert_eq!(
            line.display_as(both),
            "[web-1/app] 2024-05-01T12:00:00Z error in web"
        );
        assert_eq!(shown.text, "[web-1/app] 12:00:00.000 error in web");
        assert_eq!(&shown.text[..shown.source], "[web-1/app] ");
        assert_eq!(&shown.text[shown.source..shown.prefix], "12:00:00.000 ");
        let found: Vec<&str> = shown
            .matches
            .iter()
            .map(|r| &shown.text[r.clone()])
            .collect();
        assert_eq!(found, ["web", "web"]);
        // Styled runs: the source split around its match, the timestamp, the match in the text.
        let body_match = shown.matches[1].clone();
        assert_eq!(
            line_parts(&shown),
            [
                (0..1, Part::Source),
                (1..4, Part::Match),
                (4..shown.source, Part::Source),
                (shown.source..shown.prefix, Part::Timestamp),
                (body_match, Part::Match),
            ]
        );
        // Hidden prefix: nothing of the source is shown or matched.
        let hidden = show(&line, ts(false), &query, None);
        assert_eq!((hidden.source, hidden.text.as_str()), (0, "error in web"));
        assert_eq!(hidden.matches.len(), 1);
    }

    // covers UP-45: the header counts a workload session's streams.
    #[test]
    fn stream_counts_leave_out_zeros() {
        let stream = |state| StreamStatus {
            source: Arc::new(LineSource::new("web-1", "app")),
            state,
        };
        let mut status = PodSetStatus {
            pods: 3,
            streams: vec![
                stream(LogState::Streaming),
                stream(LogState::Streaming),
                stream(LogState::Ended {
                    hop: Default::default(),
                    error: Some("x".into()),
                    pod_deleted: false,
                }),
            ],
            max_streams: 50,
            ..Default::default()
        };
        assert_eq!(streams_text(&status), "3 pods · 2 live · 1 failed");
        status.capped = 4;
        status.pods = 1;
        status.streams.truncate(1);
        assert_eq!(streams_text(&status), "1 pod · 1 live · 4 not read");
        assert_eq!(streams_text(&PodSetStatus::default()), "0 pods · 0 live");
    }

    #[test]
    fn long_lines_are_cut_on_a_character_boundary() {
        let line = LogLine::parse(&"é".repeat(10), false);
        let shown = show(&line, ts(false), &SearchQuery::default(), Some(5));
        assert_eq!(shown.text, "éé");
        assert!(shown.cut);
        assert!(shown.matches.is_empty());
        let truncated = LogLine::parse("x", true);
        assert!(show(&truncated, ts(false), &SearchQuery::default(), None).cut);
    }

    // covers UP-40: a narrow toolbar may cut a waiting tab's status, so its tooltip says all of it.
    #[test]
    fn a_waiting_tabs_tooltip_has_the_full_status() {
        for (wait, full) in [
            (ContainerWait::Unscheduled, "Pod isn't scheduled yet"),
            (
                ContainerWait::NotStarted(Some("ImagePullBackOff".into())),
                "Container hasn't started: ImagePullBackOff",
            ),
            (ContainerWait::NotStarted(None), "Container hasn't started"),
        ] {
            assert_eq!(
                state_text(&LogState::Waiting { wait }, true).tooltip,
                Some(format!("{full}\n{WAITING_DETAIL}"))
            );
        }
    }

    // covers UP-40: the session state is always shown, in a few words; errors in plain words.
    #[test]
    fn states_read_as_words() {
        let text = |state: LogState| {
            let s = state_text(&state, true);
            (s.tone, s.text)
        };
        assert_eq!(text(LogState::Connecting).0, Tone::Muted);
        assert_eq!(text(LogState::Streaming), (Tone::Live, "Streaming".into()));
        let reconnecting = |error: Option<&str>, back_off| LogState::Reconnecting {
            hop: Default::default(),
            error: error.map(str::to_owned),
            attempt: 3,
            back_off,
        };
        // An error without plain words keeps a short form in sight.
        assert_eq!(
            text(reconnecting(Some("EOF"), false)),
            (Tone::Warning, "Reconnecting (attempt 3) · EOF".into())
        );
        let long = "the log stream broke: an unexpected and rather long reason given by kube";
        assert_eq!(
            text(reconnecting(Some(long), false)).1,
            "Reconnecting (attempt 3) · the log stream broke: an unexpected and…"
        );
        let refused = "connection refused by https://127.0.0.1:6443; is the API server running, \
                       and is the port right?";
        assert_eq!(
            text(reconnecting(Some(refused), false)).1,
            "Reconnecting (attempt 3) · can't reach the API server"
        );
        assert!(
            state_text(&reconnecting(Some(refused), false), true)
                .tooltip
                .unwrap()
                .contains(refused),
            "the server's text is in the tooltip"
        );
        assert_eq!(
            text(reconnecting(None, false)).1,
            "Reconnecting (attempt 3)"
        );
        // A crash loop is the kubelet's back-off, not a connection being retried.
        assert_eq!(
            text(reconnecting(None, true)),
            (Tone::Warning, BACK_OFF.into())
        );
        let ended = |error: Option<&str>, pod_deleted| LogState::Ended {
            hop: Default::default(),
            error: error.map(str::to_owned),
            pod_deleted,
        };
        assert_eq!(text(ended(None, false)), (Tone::Muted, "Ended".into()));
        assert_eq!(
            text(ended(Some("container not found"), false)),
            (Tone::Danger, "Ended · not found".into())
        );
        assert_eq!(
            text(ended(None, true)),
            (Tone::Muted, "Ended · pod deleted".into())
        );
        let previous = ended(
            Some(&format!(
                "{}: unable to retrieve container logs for containerd://c54f",
                flint_core::logs::PREVIOUS_NOT_AVAILABLE
            )),
            false,
        );
        assert_eq!(
            text(previous),
            (Tone::Warning, "Ended · not available".into())
        );
        assert_eq!(pause_text(0, false), "Paused");
        assert_eq!(pause_text(1, true), "Paused while selecting · 1 new line");
        assert_eq!(pause_text(12_345, false), "Paused · 12,345 new lines");
        assert_eq!(line_count(1), "1 line");
        assert_eq!(line_count(100_000), "100,000 lines");
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
    }

    // covers UP-40: a 403 or an unreachable server is said in plain words, once, with the
    // server's text as the detail; the status line stays short.
    #[test]
    fn common_errors_are_explained() {
        let forbidden = "Forbidden: pods is forbidden: User \"flint-e2e\" cannot get resource \
                         \"pods\" in API group \"\" in the namespace \"demo\"";
        let ex = explain(forbidden, FailedHop::Unknown, true);
        assert_eq!(
            ex.headline,
            "You don't have permission to read this pod's logs."
        );
        assert_eq!(ex.detail.as_deref(), Some(forbidden));
        assert_eq!(
            explain(forbidden, FailedHop::Unknown, false).headline,
            "You don't have permission to read these pods' logs."
        );
        let status = state_text(
            &LogState::Ended {
                hop: Default::default(),
                error: Some(forbidden.into()),
                pod_deleted: false,
            },
            true,
        );
        assert_eq!(status.text, "Ended · no permission");
        assert!(
            !status.text.contains("forbidden"),
            "said once, in the banner"
        );
        for network in [
            "connection refused by https://127.0.0.1:6443; is the API server running?",
            "could not resolve host example.invalid (https://example.invalid:6443)",
            "the connection to the API server dropped",
            "TLS handshake with https://h:6443 failed",
        ] {
            assert_eq!(
                explain(network, FailedHop::Unknown, true).headline,
                "Can't reach the API server.",
                "{network}"
            );
        }
        assert_eq!(
            explain(
                "https://h rejected the credentials (401 Unauthorized)",
                FailedHop::Unknown,
                true
            )
            .short,
            "credentials rejected"
        );
        assert_eq!(
            explain("boom", FailedHop::Unknown, true).headline,
            "Couldn't read the logs."
        );
        // The API server answered, but its proxy to the pod's kubelet failed: the session marks
        // the node as the failed hop, and the words follow it.
        let ex = explain(
            "InternalError: Get [REDACTED]: i/o timeout",
            FailedHop::Node,
            true,
        );
        assert_eq!(ex.headline, "Can't reach the node running this pod.");
        assert_eq!(ex.short, "can't reach the node");
        assert_eq!(
            explain("Timeout: i/o timeout", FailedHop::Unknown, true).short,
            "can't reach the API server"
        );
        // The previous container is gone, the pod is not.
        let previous = "BadRequest: previous terminated container \"app\" in pod \"p\" not found";
        assert_eq!(
            explain(previous, FailedHop::Unknown, true).headline,
            PREVIOUS_NOT_AVAILABLE
        );
    }

    // covers UP-43: timestamps show as fixed-width local time to the millisecond.
    #[test]
    fn timestamps_show_short_local_and_fixed_width() {
        let ts: Timestamp = "2026-11-20T02:20:46.939958268Z".parse().unwrap();
        assert_eq!(short_time(ts, &TimeZone::UTC), "02:20:46.939");
        let plus_one = TimeZone::fixed(jiff::tz::offset(1));
        assert_eq!(short_time(ts, &plus_one), "03:20:46.939");
        // Trailing zeros are kept: every timestamp is as wide.
        let whole: Timestamp = "2026-11-20T02:20:46Z".parse().unwrap();
        assert_eq!(short_time(whole, &TimeZone::UTC), "02:20:46.000");
        assert_eq!(short_time(whole, &TimeZone::UTC).len(), TIME_WIDTH);
        let full = full_time(ts, &plus_one);
        assert!(
            !full.contains("local"),
            "the zone is named, not assumed: {full}"
        );
        // The fallback when the system's zone cannot be read is UTC, and says so.
        assert!(full_time(ts, &TimeZone::UTC).ends_with("+00:00 UTC"));
        assert!(
            full.starts_with("2026-11-20T02:20:46.939958268Z\n"),
            "{full}"
        );
        assert!(full.contains("03:20:46.939958268 +01:00"), "{full}");
        // A line without a timestamp keeps the text in its column.
        let bare = LogLine::parse("no stamp", false);
        let shown = show(&bare, ts_on(), &SearchQuery::default(), None);
        assert_eq!(shown.text, format!("{} no stamp", " ".repeat(TIME_WIDTH)));
    }

    // covers UP-43: the view shows no date in a line, so a dated separator starts each day, and
    // the hour a fall-back repeats, where the UTC offset changes.
    #[test]
    fn a_dated_separator_starts_each_day_and_offset() {
        let rome = TimeZone::posix("CET-1CEST,M3.5.0,M10.5.0/3").unwrap();
        let line = |text: &str| LogLine::parse(text, false);
        // `before` newest first, as the view passes it.
        let sep = |before: &[&LogLine], l: &LogLine, tz: &TimeZone| {
            day_separator(before.iter().rev().copied(), l, tz)
        };
        let first = line("2026-11-19T22:59:00Z a");
        let same_day = line("2026-11-19T22:59:30Z b");
        let next_day = line("2026-11-19T23:00:00Z c");
        let bare = line("no timestamp");
        assert_eq!(
            sep(&[], &first, &rome).as_deref(),
            Some("Thursday 19 November 2026 · CET (UTC+01:00)")
        );
        assert_eq!(sep(&[&first], &same_day, &rome), None);
        assert_eq!(
            sep(&[&first, &same_day], &next_day, &rome).as_deref(),
            Some("Friday 20 November 2026 · CET (UTC+01:00)")
        );
        // In UTC the same lines share a day.
        assert_eq!(sep(&[&same_day], &next_day, &TimeZone::UTC), None);
        assert_eq!(sep(&[&first], &bare, &rome), None);
        // A line without a timestamp in between does not hide the change of day.
        assert_eq!(
            sep(&[&same_day, &bare], &next_day, &rome).as_deref(),
            Some("Friday 20 November 2026 · CET (UTC+01:00)")
        );
        // Untimestamped lines only before it: the first timestamped line starts the day.
        assert!(sep(&[&bare], &first, &rome).is_some());
        // A workload's pods interleave out of order across midnight: Thursday's late line after
        // Friday's does not start Thursday again, and Friday is not marked twice.
        let late_thursday = line("2026-11-19T22:59:59.990Z d");
        let friday_again = line("2026-11-19T23:00:00.010Z e");
        assert_eq!(sep(&[&same_day, &next_day], &late_thursday, &rome), None);
        assert_eq!(
            sep(
                &[&same_day, &next_day, &late_thursday],
                &friday_again,
                &rome
            ),
            None
        );
        // The fall-back hour: 02:30 shows twice on 25 October 2026; the second starts anew.
        let summer = line("2026-10-25T00:30:00Z d");
        let winter = line("2026-10-25T01:30:00Z e");
        assert_eq!(short_time(summer.timestamp.unwrap(), &rome), "02:30:00.000");
        assert_eq!(short_time(winter.timestamp.unwrap(), &rome), "02:30:00.000");
        assert_eq!(
            sep(&[&summer], &winter, &rome).as_deref(),
            Some("Sunday 25 October 2026 · CET (UTC+01:00)")
        );
        // Who said the previous instance's log is not available.
        assert_eq!(
            previous_speaker(
                "BadRequest: previous terminated container \"app\" in pod \"p\" not found"
            ),
            "The API server"
        );
        assert_eq!(
            previous_speaker("unable to retrieve container logs for containerd://c54f"),
            "The kubelet"
        );
    }

    fn ts_on() -> LineFormat {
        ts(true)
    }

    fn status_of(pods: &[(&str, &str)]) -> PodSetStatus {
        PodSetStatus {
            pods: pods.len(),
            streams: pods
                .iter()
                .map(|(pod, container)| StreamStatus {
                    source: Arc::new(LineSource::new(pod, container)),
                    state: LogState::Streaming,
                })
                .collect(),
            ..Default::default()
        }
    }

    // covers UP-45 (#62): a workload line's prefix is the part of the pod name that tells it
    // from its siblings, with the container only when the pod has several, in a colour of its
    // own; the full name stays in copies and the search.
    #[test]
    fn pod_prefixes_are_short_and_coloured_per_pod() {
        let mut labels = SourceLabels::default();
        assert!(labels.learn(&status_of(&[
            ("api-79db669d5-cd27k", "api"),
            ("api-79db669d5-6zn94", "api"),
        ])));
        let src = |pod: &str, c: &str| LineSource::new(pod, c);
        assert_eq!(labels.short(&src("api-79db669d5-cd27k", "api")), "cd27k");
        assert_eq!(labels.short(&src("api-79db669d5-6zn94", "api")), "6zn94");
        assert_ne!(
            labels.color(&src("api-79db669d5-cd27k", "api")),
            labels.color(&src("api-79db669d5-6zn94", "api"))
        );
        // Learning the same again changes nothing; colours stay as first given.
        let first = labels.color(&src("api-79db669d5-cd27k", "api"));
        assert!(!labels.learn(&status_of(&[("api-79db669d5-cd27k", "api")])));
        // A pod of a new ReplicaSet, with two containers: the shared part shrinks to `api-`,
        // and that pod's lines name their container.
        assert!(labels.learn(&status_of(&[
            ("api-5f8c-x2k", "api"),
            ("api-5f8c-x2k", "sidecar"),
        ])));
        assert_eq!(
            labels.short(&src("api-79db669d5-cd27k", "api")),
            "79db669d5-cd27k"
        );
        assert_eq!(
            labels.short(&src("api-5f8c-x2k", "sidecar")),
            "5f8c-x2k/sidecar"
        );
        assert_eq!(labels.color(&src("api-79db669d5-cd27k", "api")), first);
        assert_eq!(labels.width(), "5f8c-x2k/sidecar".len());
        // A pod alone still drops its workload's part.
        let mut alone = SourceLabels::default();
        alone.learn(&status_of(&[("web-7d4b9-qv4xv", "app")]));
        assert_eq!(alone.short(&src("web-7d4b9-qv4xv", "app")), "qv4xv");
        // A name that is the shared part itself is shown whole.
        let mut nested = SourceLabels::default();
        nested.learn(&status_of(&[("web", "app"), ("web-1", "app")]));
        assert_eq!(nested.short(&src("web", "app")), "web");
        assert_eq!(nested.short(&src("web-1", "app")), "web-1");
        // Never seen: shown in full.
        assert_eq!(alone.short(&src("other", "c")), "other/c");

        // Shown: padded to the widest label, so the text lines up; copied in full.
        let mut line = LogLine::parse("2024-05-01T12:00:00Z GET /a", false);
        line.source = Some(Arc::new(src("api-79db669d5-cd27k", "api")));
        let view = LineView {
            format: LineFormat {
                timestamps: false,
                source: true,
            },
            labels: &labels,
            tz: &TimeZone::UTC,
        };
        let query = SearchQuery {
            sources: true,
            ..SearchQuery::new("cd27k")
        };
        let shown = shown_line(&line, view, &query, None);
        let pad = labels.width() - "79db669d5-cd27k".len();
        assert_eq!(
            shown.text,
            format!("[79db669d5-cd27k]{} GET /a", " ".repeat(pad))
        );
        assert_eq!(shown.color, Some(first));
        assert_eq!(&shown.text[shown.matches[0].clone()], "cd27k");
        assert_eq!(line.display(false), "[api-79db669d5-cd27k/api] GET /a");
        // A search on the part the label leaves out marks the whole label.
        let query = SearchQuery {
            sources: true,
            ..SearchQuery::new("api-79db")
        };
        let shown = shown_line(&line, view, &query, None);
        assert_eq!(&shown.text[shown.matches[0].clone()], "79db669d5-cd27k");
        // The three drawn parts carry their own runs.
        assert_eq!(
            parts_in(&line_parts(&shown), 0..shown.source),
            [
                (0..1, Part::Source),
                (1..16, Part::Match),
                (16..shown.source, Part::Source)
            ]
        );
        assert!(parts_in(&line_parts(&shown), shown.prefix..shown.text.len()).is_empty());
    }

    // covers UP-48: matches are found in the whole message, then clipped to what is drawn.
    #[test]
    fn highlights_of_a_clipped_line_come_from_the_whole_message() {
        let format = LineFormat {
            timestamps: false,
            source: false,
        };
        let anchored = SearchQuery {
            regex: true,
            ..SearchQuery::new("Z$")
        };
        let clipped = LogLine::parse(&format!("{}Ztail", "a".repeat(9)), false);
        assert!(
            show(&clipped, format, &anchored, Some(10))
                .matches
                .is_empty()
        );
        let whole = LogLine::parse(&format!("{}Z", "a".repeat(9)), false);
        let shown = show(&whole, format, &anchored, Some(10));
        assert_eq!(shown.matches.as_slice(), std::slice::from_ref(&(9..10)));
        let crossing = LogLine::parse("xxabcd", false);
        let shown = show(&crossing, format, &SearchQuery::new("abc"), Some(4));
        assert_eq!(shown.matches.as_slice(), std::slice::from_ref(&(2..4)));
    }

    // covers UP-48: a frame out of highlighting time draws the row plain and asks for another;
    // what was found stays, per line and query.
    #[test]
    fn highlights_are_bounded_per_frame_and_kept_per_line() {
        let line = LogLine::parse("find me", false);
        let query = SearchQuery::new("find");
        let mut highlights = LineHighlights {
            budget: std::time::Duration::ZERO,
            ..Default::default()
        };
        assert!(
            highlights
                .matches(1, &line, &query, None, None)
                .body
                .is_empty()
        );
        assert!(highlights.take_redraw());
        assert!(!highlights.take_redraw());

        highlights.budget = std::time::Duration::from_secs(60);
        highlights.begin_frame();
        let found = std::slice::from_ref(&(0..4));
        assert_eq!(highlights.matches(1, &line, &query, None, None).body, found);
        assert!(!highlights.take_redraw());

        // Kept: no budget is needed to draw it again.
        highlights.budget = std::time::Duration::ZERO;
        highlights.begin_frame();
        assert_eq!(highlights.matches(1, &line, &query, None, None).body, found);
        assert!(!highlights.take_redraw());

        // Another query finds again.
        let other = SearchQuery::new("me");
        assert!(
            highlights
                .matches(1, &line, &other, None, None)
                .body
                .is_empty()
        );
        assert!(highlights.take_redraw());
    }

    // covers UP-48: the "⋯" menu fits a short window and keeps a usable height in a tiny one.
    #[test]
    fn the_more_menu_fits_the_window() {
        use gpui_kit::px;
        assert_eq!(menu_max_height(px(400.)), px(352.));
        assert_eq!(menu_max_height(px(100.)), px(120.));
    }

    // #309: a plain query tests only the drawn start of a line, and still sees a match that
    // crosses the cut.
    #[test]
    fn plain_highlights_test_the_drawn_prefix() {
        let query = SearchQuery::new("abc");
        let text = format!("xxabcd{}abc", "-".repeat(100));
        assert_eq!(drawn_window(&text, &query, Some(4)), &text[..16]);
        assert_eq!(drawn_window(&text, &query, None), text);
        let regex = SearchQuery {
            regex: true,
            ..query.clone()
        };
        assert_eq!(
            drawn_window(&text, &regex, Some(4)),
            text,
            "cut by the core"
        );
        let wide = "ööööööööö";
        assert!(wide.is_char_boundary(drawn_window(wide, &query, Some(1)).len()));

        let mut highlights = LineHighlights {
            budget: std::time::Duration::from_secs(60),
            ..Default::default()
        };
        let line = LogLine::parse(&text, false);
        let clipped = highlights.matches(1, &line, &query, None, Some(4)).body;
        assert_eq!(clipped, std::slice::from_ref(&(2..5)));
        // Drawn whole after Wrap: found again, not read from the clipped cache.
        let whole = highlights.matches(1, &line, &query, None, None).body;
        assert_eq!(whole, [2..5, 106..109]);
    }

    // #309: a long regex line drawn whole is listed once for the background scan, and what it
    // found replaces the bounded first pass unless the query moved on.
    #[test]
    fn long_regex_lines_are_scanned_whole_off_the_ui_thread() {
        let query = SearchQuery {
            regex: true,
            ..SearchQuery::new("needle")
        };
        let at = flint_core::logs::HIGHLIGHT_BYTES * 2;
        let line = LogLine::parse(&format!("{}needle", "x".repeat(at)), false);
        let mut highlights = LineHighlights {
            budget: std::time::Duration::from_secs(60),
            ..Default::default()
        };
        assert!(
            highlights
                .matches(1, &line, &query, None, None)
                .body
                .is_empty()
        );
        assert!(highlights.take_redraw(), "asks for a frame to start it");
        assert_eq!(highlights.take_wanted(), [1]);
        highlights.begin_frame();
        highlights.matches(1, &line, &query, None, None);
        assert!(highlights.take_wanted().is_empty(), "asked for once");

        let other = SearchQuery {
            regex: true,
            ..SearchQuery::new("other")
        };
        let whole = |range: Range<usize>, complete| flint_core::logs::LineRanges {
            ranges: vec![range],
            complete,
        };
        highlights.store_whole(&other, vec![(1, whole(0..1, true))]);
        assert!(
            highlights
                .matches(1, &line, &query, None, None)
                .body
                .is_empty()
        );
        assert!(
            highlights.matches(1, &line, &query, None, None).limited,
            "partial until the whole line is scanned"
        );
        highlights.store_whole(&query, vec![(1, whole(at..at + 6, true))]);
        let found = highlights.matches(1, &line, &query, None, None);
        assert_eq!(found.body, std::slice::from_ref(&(at..at + 6)));
        assert!(!found.limited);

        // A whole-line scan a limit stopped keeps what it found and stays marked.
        highlights.store_whole(&query, vec![(1, whole(at..at + 6, false))]);
        let found = highlights.matches(1, &line, &query, None, None);
        assert_eq!(found.body, std::slice::from_ref(&(at..at + 6)));
        assert!(found.limited);

        // Cut to the drawn part, nothing goes off the UI thread.
        highlights.matches(2, &line, &query, None, Some(UNWRAPPED_MAX_BYTES));
        assert!(highlights.take_wanted().is_empty());
    }

    // #309: a line a highlight limit stopped keeps the matches found and is shown as partial.
    #[test]
    fn a_limited_highlight_is_shown_as_partial() {
        let query = SearchQuery {
            regex: true,
            ..SearchQuery::new("b|x*")
        };
        let line = LogLine::parse(&format!("b{}x", "a".repeat(3_000)), false);
        let mut highlights = LineHighlights {
            budget: std::time::Duration::from_secs(60),
            ..Default::default()
        };
        let found = highlights.matches(1, &line, &query, None, None);
        assert_eq!(found.body, std::slice::from_ref(&(0..1)));
        assert!(found.limited);
        let short = LogLine::parse("b a", false);
        assert!(!highlights.matches(2, &short, &query, None, None).limited);
    }

    // covers UP-40: the toolbar stays one row, giving up parts as the tab narrows.
    #[test]
    fn the_toolbar_gives_way_by_width() {
        let full = toolbar_fit(None);
        assert!(full.toggles && full.jump_label && full.counts && full.match_count);
        assert_eq!(toolbar_fit(Some(1280.)), full);
        let compact = toolbar_fit(Some(860.));
        assert!(!compact.toggles && !compact.jump_label && compact.counts);
        let narrow = toolbar_fit(Some(470.));
        assert!(!narrow.counts && !narrow.match_count);
        assert!(toolbar_fit(Some(560.)).match_count);
    }

    #[test]
    fn match_text_counts_and_places_the_current_match() {
        let shown = lines(&["error a", "ok", "error b", "error c"]);
        let mut search = LogSearch::new(SearchQuery::new("error"));
        search.update(&shown);
        assert_eq!(match_text(&search, None), "3 matches");
        assert_eq!(match_text(&search, Some(2)), "2 of 3");
        assert_eq!(match_text(&search, Some(1)), "3 matches");
        let mut one = LogSearch::new(SearchQuery::new("b"));
        one.update(&shown);
        assert_eq!(match_text(&one, None), "1 match");
        assert_eq!(match_text(&LogSearch::default(), None), "No matches");
    }

    #[test]
    fn titles_and_file_names_name_the_container() {
        let target = LogTarget {
            namespace: "demo".into(),
            pod: "web-1".into(),
            container: Some("app".into()),
        };
        let workload = LogSubject::Workload(WorkloadTarget {
            namespace: "demo".into(),
            pods: PodSet::Workload {
                kind: WorkloadKind::Deployment,
                name: "web".into(),
            },
            container: None,
        });
        assert_eq!(tab_title(&workload, false), "web (Deployment)");
        assert_eq!(suggested_file_name(&workload, false), "deployment_web.log");
        let selector = LogSubject::Workload(WorkloadTarget {
            namespace: "demo".into(),
            pods: PodSet::Selector("app=web".into()),
            container: Some("app".into()),
        });
        assert_eq!(tab_title(&selector, false), "-l app=web · app");
        assert_eq!(suggested_file_name(&selector, false), "app_web_app.log");
        let target = LogSubject::Pod(target);
        assert_eq!(tab_title(&target, false), "web-1 · app");
        assert_eq!(tab_title(&target, true), "web-1 · app (previous)");
        assert_eq!(suggested_file_name(&target, true), "web-1_app_previous.log");
        let bare = LogSubject::Pod(LogTarget {
            namespace: "demo".into(),
            container: None,
            pod: "a/b c".into(),
        });
        assert_eq!(tab_title(&bare, false), "a/b c");
        assert_eq!(suggested_file_name(&bare, false), "a_b_c.log");
    }

    // covers UP-44 and #75: saved and copied text is exactly what is shown, not redacted.
    #[test]
    fn save_text_writes_the_chosen_lines_as_shown() {
        let shown = lines(&[
            "2024-05-01T12:00:00Z first password=hunter2",
            "2024-05-01T12:00:01Z error two",
            "2024-05-01T12:00:02Z three",
        ]);
        let (all, n) = save_text(&shown, SaveScope::All, &[], None, ts(false)).unwrap();
        assert_eq!(n, 3);
        assert_eq!(all, "first password=hunter2\nerror two\nthree\n");
        let (stamped, _) = save_text(&shown, SaveScope::All, &[], None, ts(true)).unwrap();
        assert!(stamped.starts_with("2024-05-01T12:00:00Z first"));
        let (matched, n) = save_text(&shown, SaveScope::Matches, &[1, 9], None, ts(false)).unwrap();
        assert_eq!((matched.as_str(), n), ("error two\n", 1));
        let selection = Some(LineSelection::new(2, 1));
        let (selected, n) =
            save_text(&shown, SaveScope::Selection, &[], selection, ts(false)).unwrap();
        assert_eq!((selected.as_str(), n), ("error two\nthree\n", 2));
        assert_eq!(
            save_text(&shown, SaveScope::Selection, &[], None, ts(false)),
            None
        );
        assert_eq!(
            save_text(&shown, SaveScope::Matches, &[], None, ts(false)),
            None
        );
        assert_eq!(
            save_text(&LogLines::default(), SaveScope::All, &[], None, ts(false)),
            None
        );
        // The count the confirmation shows is the count written.
        for (scope, sel) in [
            (SaveScope::All, None),
            (SaveScope::Matches, None),
            (SaveScope::Selection, selection),
            (SaveScope::Selection, None),
        ] {
            let written = save_text(&shown, scope, &[1, 9], sel, ts(false)).map_or(0, |(_, n)| n);
            assert_eq!(
                save_count(&shown, scope, &[1, 9], sel),
                written,
                "{scope:?}"
            );
        }
    }

    /// Keeps what it is given and the largest single write.
    #[derive(Default)]
    struct RecordingWriter {
        written: Vec<u8>,
        largest: usize,
    }

    impl std::io::Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.largest = self.largest.max(bytes.len());
            self.written.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_save_reaches_the_file_in_small_writes() {
        // As many lines as the fixture keeps, together several times the write size.
        let long = "x".repeat(SAVE_WRITE_BYTES / 20);
        let text: Vec<String> = (0..100)
            .map(|i| format!("2024-05-01T12:00:00Z {i} {long}"))
            .collect();
        let text: Vec<&str> = text.iter().map(String::as_str).collect();
        let shown = lines(&text);
        let expected = save_text(&shown, SaveScope::All, &[], None, ts(false))
            .unwrap()
            .0;
        let mut out = RecordingWriter::default();

        let count = write_saved(&mut out, &shown, SaveScope::All, &[], None, ts(false)).unwrap();

        assert_eq!(count, 100);
        assert_eq!(out.written, expected.as_bytes());
        assert!(
            out.largest <= SAVE_WRITE_BYTES,
            "one write of {} bytes out of {}",
            out.largest,
            expected.len()
        );
    }

    #[test]
    fn a_failed_write_is_reported() {
        struct Full;
        impl std::io::Write for Full {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("disk full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("disk full"))
            }
        }
        let shown = lines(&["2024-05-01T12:00:00Z only"]);

        let error = write_saved(Full, &shown, SaveScope::All, &[], None, ts(false)).unwrap_err();

        assert_eq!(error.to_string(), "disk full");
    }

    // covers UP-40, UP-42: a failed stream keeps its explanation when its lines are dropped.
    #[test]
    fn a_failure_stays_visible_when_all_lines_were_dropped() {
        let (text, detail) = with_lines_dropped(
            "The stream ended.".into(),
            Some("server said no".into()),
            true,
        );
        assert_eq!(text, "The stream ended.");
        let detail = detail.unwrap();
        assert!(detail.contains("server said no") && detail.contains(ALL_LINES_DROPPED));

        let (text, detail) = with_lines_dropped(
            "No log lines.".into(),
            Some("The pod was deleted.".into()),
            false,
        );
        assert_eq!(text, ALL_LINES_DROPPED);
        assert!(detail.unwrap().starts_with("The pod was deleted."));
    }

    // covers UP-42: only an ended tab that lost every line says so. A paused one with newer
    // lines waiting shows them on resume.
    #[test]
    fn all_lines_dropped_only_when_nothing_is_left_to_show() {
        use flint_core::{LogSnapshot, LogState};
        let ended = LogSnapshot {
            lines: Default::default(),
            state: LogState::Ended {
                hop: Default::default(),
                error: None,
                pod_deleted: true,
            },
            paused: false,
            held_back: 0,
            version: 1,
            pods: None,
            trimmed: true,
        };
        assert!(all_lines_dropped(&ended));
        let never_trimmed = LogSnapshot {
            trimmed: false,
            ..ended.clone()
        };
        assert!(!all_lines_dropped(&never_trimmed));
        let lines_wait = LogSnapshot {
            paused: true,
            held_back: 3,
            ..ended.clone()
        };
        assert!(!all_lines_dropped(&lines_wait));
        let running = LogSnapshot {
            state: LogState::Streaming,
            ..ended
        };
        assert!(!all_lines_dropped(&running));
    }
}
