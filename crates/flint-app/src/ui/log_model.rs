//! What a log tab shows, without GPUI, so it can be unit tested: how a new snapshot changes the
//! list of lines, the text and highlights of one line, the session's state in words, and the
//! text that copy and save write.

use std::ops::Range;

use flint_core::{
    LineId, LineSelection, LogLine, LogLines, LogSearch, LogState, LogTarget, SearchQuery,
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

/// One line as drawn: its text (with the timestamp in front when shown), where the timestamp
/// ends, and the byte ranges of search matches in that text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownLine {
    pub text: String,
    /// The timestamp and the space after it; `0` when not shown.
    pub prefix: usize,
    pub matches: Vec<Range<usize>>,
    /// The line was cut: at [`flint_core::logs::MAX_LINE_BYTES`] when it arrived, or here at
    /// `max_bytes`.
    pub cut: bool,
}

/// `line` as shown: with its timestamp when `timestamps` is on, its search matches, and at most
/// `max_bytes` of its text (on a character boundary).
pub fn shown_line(
    line: &LogLine,
    timestamps: bool,
    query: &SearchQuery,
    max_bytes: Option<usize>,
) -> ShownLine {
    let mut text = match (&line.timestamp, timestamps) {
        (Some(ts), true) => format!("{ts} "),
        _ => String::new(),
    };
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
    let matches = query
        .match_ranges(body)
        .into_iter()
        .map(|r| r.start + prefix..r.end + prefix)
        .collect();
    ShownLine {
        text,
        prefix,
        matches,
        cut,
    }
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

/// A tab's title: the pod, the container when named, and "previous" for a terminated instance.
pub fn tab_title(target: &LogTarget, previous: bool) -> String {
    let mut title = match &target.container {
        Some(container) => format!("{} · {container}", target.pod),
        None => target.pod.clone(),
    };
    if previous {
        title.push_str(" (previous)");
    }
    title
}

/// The file name a save dialog suggests: `web-1_app.log`, `web-1_app_previous.log`. Only
/// characters safe in a file name are kept.
pub fn suggested_file_name(target: &LogTarget, previous: bool) -> String {
    let mut stem = target.pod.clone();
    if let Some(container) = &target.container {
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

/// The lines to save, as shown (timestamps when `timestamps` is on), one per line and ending
/// in a newline, and how many there are. Not redacted: the file holds what the tab showed.
/// `None` when the scope has nothing (no search, no selection).
pub fn save_text(
    lines: &LogLines,
    scope: SaveScope,
    matches: &[LineId],
    selection: Option<LineSelection>,
    timestamps: bool,
) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut count = 0;
    let mut push = |line: &LogLine| {
        out.push_str(&line.display(timestamps));
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
    use flint_core::logs::LogBuffer;

    use super::*;

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
        let plain = shown_line(&line, false, &query, None);
        assert_eq!(plain.text, "GET /api error 500 ERROR");
        assert_eq!(plain.prefix, 0);
        assert_eq!(plain.matches, [9..14, 19..24]);
        let stamped = shown_line(&line, true, &query, None);
        assert_eq!(stamped.prefix, "2024-05-01T12:00:00Z ".len());
        assert_eq!(&stamped.text[stamped.prefix..], plain.text);
        for (a, b) in stamped.matches.iter().zip(&plain.matches) {
            assert_eq!(&stamped.text[a.clone()], &plain.text[b.clone()]);
        }
        // A search never matches inside the timestamp.
        let in_ts = shown_line(&line, true, &SearchQuery::new("2024"), None);
        assert!(in_ts.matches.is_empty());
        assert!(!plain.cut);
    }

    #[test]
    fn long_lines_are_cut_on_a_character_boundary() {
        let line = LogLine::parse(&"é".repeat(10), false);
        let shown = shown_line(&line, false, &SearchQuery::default(), Some(5));
        assert_eq!(shown.text, "éé");
        assert!(shown.cut);
        assert!(shown.matches.is_empty());
        let truncated = LogLine::parse("x", true);
        assert!(shown_line(&truncated, false, &SearchQuery::default(), None).cut);
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
        assert_eq!(tab_title(&target, false), "web-1 · app");
        assert_eq!(tab_title(&target, true), "web-1 · app (previous)");
        assert_eq!(suggested_file_name(&target, true), "web-1_app_previous.log");
        let bare = LogTarget {
            container: None,
            pod: "a/b c".into(),
            ..target
        };
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
        let (all, n) = save_text(&shown, SaveScope::All, &[], None, false).unwrap();
        assert_eq!(n, 3);
        assert_eq!(all, "first password=hunter2\nerror two\nthree\n");
        let (stamped, _) = save_text(&shown, SaveScope::All, &[], None, true).unwrap();
        assert!(stamped.starts_with("2024-05-01T12:00:00Z first"));
        let (matched, n) = save_text(&shown, SaveScope::Matches, &[1, 9], None, false).unwrap();
        assert_eq!((matched.as_str(), n), ("error two\n", 1));
        let selection = Some(LineSelection::new(2, 1));
        let (selected, n) = save_text(&shown, SaveScope::Selection, &[], selection, false).unwrap();
        assert_eq!((selected.as_str(), n), ("error two\nthree\n", 2));
        assert_eq!(
            save_text(&shown, SaveScope::Selection, &[], None, false),
            None
        );
        assert_eq!(
            save_text(&shown, SaveScope::Matches, &[], None, false),
            None
        );
        assert_eq!(
            save_text(&LogLines::default(), SaveScope::All, &[], None, false),
            None
        );
    }
}
