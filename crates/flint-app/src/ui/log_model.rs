//! What a log tab shows, without GPUI, so it can be unit tested: how a new snapshot changes the
//! list of lines, the text and highlights of one line, the session's state in words, and the
//! text that copy and save write.

use std::ops::Range;

use flint_core::{
    LineFormat, LineId, LineSelection, LogLine, LogLines, LogSearch, LogState, LogSubject, PodSet,
    PodSetStatus, SearchQuery,
};

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

/// One line as drawn: its text, with the `[pod/container]` source and the timestamp in front
/// when shown (as [`LogLine::display_as`] writes it), where those end, and the byte ranges of
/// search matches in that text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownLine {
    pub text: String,
    /// The source prefix and the space after it; `0` when not shown (or a pod session).
    pub source: usize,
    /// Where the line's own text starts: after the source and the timestamp.
    pub prefix: usize,
    pub matches: Vec<Range<usize>>,
    /// The line was cut: at [`flint_core::logs::MAX_LINE_BYTES`] when it arrived, or here at
    /// `max_bytes`.
    pub cut: bool,
}

/// `line` as shown with `format`: its search matches (in the source too, when the query
/// searches sources and the prefix is shown), and at most `max_bytes` of its text (on a
/// character boundary).
pub fn shown_line(
    line: &LogLine,
    format: LineFormat,
    query: &SearchQuery,
    max_bytes: Option<usize>,
) -> ShownLine {
    let mut text = String::new();
    let mut matches = Vec::new();
    if let Some(source) = line.source.as_ref().filter(|_| format.source) {
        text.push('[');
        if query.sources {
            matches.extend(
                query
                    .match_ranges(source.label())
                    .into_iter()
                    .map(|r| r.start + 1..r.end + 1),
            );
        }
        text.push_str(source.label());
        text.push_str("] ");
    }
    let source = text.len();
    if let Some(ts) = line.timestamp.as_ref().filter(|_| format.timestamps) {
        text.push_str(&format!("{ts} "));
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
    matches.extend(
        query
            .match_ranges(body)
            .into_iter()
            .map(|r| r.start + prefix..r.end + prefix),
    );
    ShownLine {
        text,
        source,
        prefix,
        matches,
        cut,
    }
}

/// What a styled run of a shown line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Source,
    Timestamp,
    Match,
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

/// The session's state in a few words, and the tone to draw it in.
pub fn state_text(state: &LogState) -> (Tone, String) {
    match state {
        LogState::Connecting => (Tone::Muted, "Connecting…".to_owned()),
        LogState::Streaming => (Tone::Live, "Streaming".to_owned()),
        LogState::Reconnecting { error, attempt } => (
            Tone::Warning,
            match error {
                Some(e) => format!("Reconnecting (attempt {attempt}): {e}"),
                None => format!("Reconnecting (attempt {attempt})"),
            },
        ),
        LogState::Ended { error: None } => (Tone::Muted, "Ended".to_owned()),
        LogState::Ended { error: Some(e) } => (Tone::Danger, format!("Ended: {e}")),
    }
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

/// "3 of 17" when `current` is a match, "17 matches" otherwise, "No matches" for none.
pub fn match_text(search: &LogSearch, current: Option<LineId>) -> String {
    let count = search.count();
    let at = current.and_then(|id| search.matches().binary_search(&id).ok());
    match (count, at) {
        (0, _) => "No matches".to_owned(),
        (n, Some(i)) => format!("{} of {}", thousands(i as u64 + 1), thousands(n as u64)),
        (1, None) => "1 match".to_owned(),
        (n, None) => format!("{} matches", thousands(n as u64)),
    }
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
    let (name, container) = subject_name(subject);
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
        SaveScope::Selection => selection.map_or(0, |s| s.lines(lines).count()),
    }
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
    let mut count = 0;
    let mut push = |line: &LogLine| {
        out.push_str(&line.display_as(format));
        out.push('\n');
        count += 1;
    };
    match scope {
        SaveScope::All => lines.iter().for_each(|(_, line)| push(line)),
        SaveScope::Matches => matches
            .iter()
            .filter_map(|id| lines.get(*id))
            .for_each(&mut push),
        SaveScope::Selection => selection?.lines(lines).for_each(|(_, line)| push(line)),
    }
    (count > 0).then_some((out, count))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flint_core::logs::LogBuffer;
    use flint_core::{LineSource, LogTarget, StreamStatus, WorkloadKind, WorkloadTarget};

    use super::*;

    /// Timestamps on or off, no source prefix.
    fn ts(timestamps: bool) -> LineFormat {
        LineFormat {
            timestamps,
            source: false,
        }
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
        let plain = shown_line(&line, ts(false), &query, None);
        assert_eq!(plain.text, "GET /api error 500 ERROR");
        assert_eq!(plain.prefix, 0);
        assert_eq!(plain.matches, [9..14, 19..24]);
        let stamped = shown_line(&line, ts(true), &query, None);
        assert_eq!(stamped.prefix, "2024-05-01T12:00:00Z ".len());
        assert_eq!(&stamped.text[stamped.prefix..], plain.text);
        for (a, b) in stamped.matches.iter().zip(&plain.matches) {
            assert_eq!(&stamped.text[a.clone()], &plain.text[b.clone()]);
        }
        // A search never matches inside the timestamp.
        let in_ts = shown_line(&line, ts(true), &SearchQuery::new("2024"), None);
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
        let shown = shown_line(&line, both, &query, None);
        assert_eq!(shown.text, line.display_as(both));
        assert_eq!(&shown.text[..shown.source], "[web-1/app] ");
        assert_eq!(
            &shown.text[shown.source..shown.prefix],
            "2024-05-01T12:00:00Z "
        );
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
        let hidden = shown_line(&line, ts(false), &query, None);
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
                    error: Some("x".into()),
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
        let shown = shown_line(&line, ts(false), &SearchQuery::default(), Some(5));
        assert_eq!(shown.text, "éé");
        assert!(shown.cut);
        assert!(shown.matches.is_empty());
        let truncated = LogLine::parse("x", true);
        assert!(shown_line(&truncated, ts(false), &SearchQuery::default(), None).cut);
    }

    // covers UP-40: the session state is always shown.
    #[test]
    fn states_read_as_words() {
        assert_eq!(state_text(&LogState::Connecting).0, Tone::Muted);
        assert_eq!(
            state_text(&LogState::Streaming),
            (Tone::Live, "Streaming".into())
        );
        assert_eq!(
            state_text(&LogState::Reconnecting {
                error: Some("EOF".into()),
                attempt: 3
            }),
            (Tone::Warning, "Reconnecting (attempt 3): EOF".into())
        );
        assert_eq!(
            state_text(&LogState::Reconnecting {
                error: None,
                attempt: 1
            })
            .1,
            "Reconnecting (attempt 1)"
        );
        assert_eq!(
            state_text(&LogState::Ended { error: None }),
            (Tone::Muted, "Ended".into())
        );
        assert_eq!(
            state_text(&LogState::Ended {
                error: Some("container not found".into())
            }),
            (Tone::Danger, "Ended: container not found".into())
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
        assert_eq!(tab_title(&workload, false), "deployment/web");
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
}
