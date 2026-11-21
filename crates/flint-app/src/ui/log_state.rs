//! A log tab's part of the visible view tree ([`crate::ui::view_state`]): its status, toggles,
//! search, selection, a workload's stream counts and its newest lines. Only the last
//! [`LogTabState::LINE_LIMIT`] lines go out, so a test can wait for a line without the state
//! carrying the buffer.

use flint_core::logs::LogLines;
use flint_core::{LogState, PodSetStatus, Severity};
use gpui_kit::*;

use super::super::log_model;
use super::super::view_state::{
    FilterState, Focus, LogLineState, LogTabState, SearchState, SelectionState, StreamCounts,
    StreamState, TabStatusState, error_text,
};
use super::LogView;

impl LogView {
    /// The tab as data.
    pub fn view_state(&self) -> LogTabState {
        let ended = self.has_ended();
        LogTabState {
            lines: self.snapshot.lines.len(),
            held_back: self.snapshot.held_back,
            trimmed: self.snapshot.trimmed,
            paused: !ended && (self.snapshot.paused || self.is_paused()),
            follows: self.follows(),
            following: self.list.is_following_tail(),
            timestamps: self.timestamps,
            sources: self.format().source,
            wrap: self.wrap,
            search: SearchState {
                query: self.query.text.clone(),
                matches: self.match_count(),
                // Among the matches of the lines shown: evicted ones lead the list until the
                // next scan drops them.
                current: self.current_match_index().map(|at| at + 1),
                searching: self.searching,
            },
            filter: FilterState {
                matching_only: self.matching_only,
                errors_only: self.errors_only,
                context: self.context,
                rows: self.is_filtered().then(|| self.row_count()),
                // A catch-up on the core's threads too: the rows shown are not up to date.
                filtering: self.filtering || self.catching_up,
            },
            selection: self.selection.map(|s| SelectionState {
                first: s.start(),
                last: s.end(),
                // As the hint counts them: the selected lines still shown.
                // The rows shown, in a filtered tab: what copy and save would write.
                lines: self.selected_rows(),
            }),
            streams: self.snapshot.pods.as_deref().map(stream_counts),
            // The newest lines drawn: the newest rows while filtered.
            last_lines: match self.filter_rows() {
                None => last_lines(&self.snapshot.lines, LogTabState::LINE_LIMIT),
                Some(rows) => rows
                    .iter()
                    .skip(rows.len().saturating_sub(LogTabState::LINE_LIMIT))
                    .filter_map(|id| {
                        self.snapshot
                            .lines
                            .get(*id)
                            .map(|line| line_state(*id, line))
                    })
                    .collect(),
            },
        }
    }

    /// How many lines shown match, as [`LogView::has_matches`] sees them. 0 while a scan runs
    /// on the core's threads.
    fn match_count(&self) -> usize {
        if self.searching {
            0
        } else {
            self.shown_match_count().0
        }
    }

    /// Where focus is in the tab, if it is: its search field or its lines.
    pub fn focus_in(&self, window: &Window, cx: &App) -> Option<Focus> {
        if self.search_input.focus_handle(cx).is_focused(window) {
            Some(Focus::LogSearch)
        } else if self.focus.contains_focused(window, cx) {
            Some(Focus::Log)
        } else {
            None
        }
    }

    /// The session's state, and the status line as drawn.
    pub fn status_state(&self) -> (TabStatusState, String) {
        let pod = !self.is_workload();
        (
            status(&self.snapshot.state, pod),
            log_model::state_text(&self.snapshot.state, pod).text,
        )
    }
}

/// A log session's state, with errors in the words the tab uses. `pod`: one pod's log (else a
/// workload's).
pub fn status(state: &LogState, pod: bool) -> TabStatusState {
    match state {
        LogState::Connecting => TabStatusState::Connecting,
        LogState::Streaming => TabStatusState::Streaming,
        LogState::Reconnecting {
            error,
            attempt,
            back_off,
        } => TabStatusState::Reconnecting {
            attempt: *attempt,
            back_off: *back_off,
            reason: match error {
                _ if *back_off => None,
                Some(e) => Some(reconnect_reason(e, pod)),
                None => None,
            },
        },
        LogState::Waiting { wait } => TabStatusState::Waiting {
            reason: log_model::wait_text(wait),
        },
        LogState::Ended { .. } if state.previous_not_available().is_some() => {
            TabStatusState::Failed {
                reason: log_model::PREVIOUS_NOT_AVAILABLE.to_owned(),
            }
        }
        LogState::Ended { error: Some(e), .. } => TabStatusState::Failed {
            reason: log_model::explain(e, pod).headline,
        },
        LogState::Ended { pod_deleted, .. } => TabStatusState::Ended {
            reason: pod_deleted.then(|| "pod deleted".to_owned()),
            pod_deleted: *pod_deleted,
        },
    }
}

/// Why a stream reconnects, as its status line says it: in plain words, or the error's gist
/// when there are none (sanitized, as every error in the state is).
fn reconnect_reason(error: &str, pod: bool) -> String {
    match log_model::explain(error, pod).short {
        "error" => error_text(&log_model::gist(error)),
        short => short.to_owned(),
    }
}

/// A workload's streams, counted as the toolbar counts them.
pub fn stream_counts(status: &PodSetStatus) -> StreamCounts {
    let ended = status
        .streams
        .iter()
        .filter(|s| matches!(s.state, LogState::Ended { error: None, .. }))
        .count();
    StreamCounts {
        pods: status.pods,
        live: status.live(),
        connecting: status.connecting(),
        waiting: status.waiting,
        failed: status.failed(),
        ended,
        capped: status.capped,
        each: status
            .streams
            .iter()
            .take(StreamCounts::LIMIT)
            .map(|s| StreamState {
                pod: s.source.pod().to_owned(),
                container: s.source.container().to_owned(),
                state: match &s.state {
                    LogState::Connecting => "connecting",
                    LogState::Streaming => "streaming",
                    LogState::Reconnecting { .. } => "reconnecting",
                    LogState::Waiting { .. } => "waiting",
                    LogState::Ended { error: Some(_), .. } => "failed",
                    LogState::Ended { .. } => "ended",
                },
            })
            .collect(),
    }
}

/// The newest `limit` lines of `lines`, oldest first.
pub fn last_lines(lines: &LogLines, limit: usize) -> Vec<LogLineState> {
    let ids = tail_ids(lines, limit);
    lines
        .range(ids.start, ids.end)
        .map(|(id, line)| line_state(id, line))
        .collect()
}

fn line_state(id: u64, line: &flint_core::LogLine) -> LogLineState {
    LogLineState {
        id,
        text: line.text.to_string(),
        severity: severity_word(line.severity),
        source: line.source.as_ref().map(|s| s.label().to_owned()),
    }
}

/// The ids of the newest `limit` lines of `lines`: a range read line by line, so `state` never
/// walks the whole buffer (up to 100,000 lines) on every poll.
pub fn tail_ids(lines: &LogLines, limit: usize) -> std::ops::Range<u64> {
    let end = lines.end_id();
    end.saturating_sub(limit as u64).max(lines.first_id())..end
}

fn severity_word(severity: Severity) -> &'static str {
    match severity {
        Severity::Normal => "normal",
        Severity::Warning => "warning",
        Severity::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use flint_core::logs::LogBuffer;
    use flint_core::{LineSource, LogLine, LogState, PodSetStatus, StreamStatus};
    use serde_json::json;

    use super::{last_lines, status, stream_counts};
    use crate::ui::view_state::TabStatusState;

    fn json(state: &LogState) -> serde_json::Value {
        serde_json::to_value(status(state, true)).unwrap()
    }

    #[test]
    fn statuses_say_their_state_in_plain_words() {
        assert_eq!(json(&LogState::Connecting), json!({"state": "connecting"}));
        assert_eq!(json(&LogState::Streaming), json!({"state": "streaming"}));
        assert_eq!(
            json(&LogState::Reconnecting {
                error: Some("connection refused".into()),
                attempt: 2,
                back_off: false,
            }),
            json!({"state": "reconnecting", "attempt": 2, "back_off": false,
                   "reason": "can't reach the API server"})
        );
        // Crash-looping: the wait is the kubelet's, not an error.
        assert_eq!(
            json(&LogState::Reconnecting {
                error: Some("container is waiting".into()),
                attempt: 1,
                back_off: true,
            }),
            json!({"state": "reconnecting", "attempt": 1, "back_off": true, "reason": null})
        );
        assert_eq!(
            json(&LogState::Ended {
                error: None,
                pod_deleted: true,
            }),
            json!({"state": "ended", "reason": "pod deleted", "pod_deleted": true})
        );
        assert_eq!(
            json(&LogState::Ended {
                error: None,
                pod_deleted: false,
            }),
            json!({"state": "ended", "reason": null, "pod_deleted": false})
        );
        assert_eq!(
            status(
                &LogState::Ended {
                    error: Some("Forbidden: pods \"web\" is forbidden".into()),
                    pod_deleted: false,
                },
                true
            ),
            TabStatusState::Failed {
                reason: "You don't have permission to read this pod's logs.".into()
            }
        );
        let gone = LogState::Ended {
            error: Some(format!(
                "{}: unable to retrieve container logs",
                flint_core::logs::PREVIOUS_NOT_AVAILABLE
            )),
            pod_deleted: false,
        };
        assert_eq!(
            status(&gone, true),
            TabStatusState::Failed {
                reason: crate::ui::log_model::PREVIOUS_NOT_AVAILABLE.into()
            }
        );
    }

    #[test]
    fn only_the_newest_lines_go_out() {
        let mut buffer = LogBuffer::new(100);
        for n in 0..10 {
            let mut line = LogLine::parse(&format!("2024-05-01T12:00:00Z ERROR line {n}"), false);
            if n == 9 {
                line.source = Some(Arc::new(LineSource::new("web-1", "app")));
            }
            buffer.push(line);
        }
        let lines = last_lines(&buffer.view(), 3);
        assert_eq!(
            serde_json::to_value(&lines).unwrap(),
            json!([
                {"id": 7, "text": "ERROR line 7", "severity": "error", "source": null},
                {"id": 8, "text": "ERROR line 8", "severity": "error", "source": null},
                {"id": 9, "text": "ERROR line 9", "severity": "error", "source": "web-1/app"},
            ])
        );
        assert_eq!(last_lines(&buffer.view(), 50).len(), 10);
        assert!(last_lines(&LogBuffer::new(4).view(), 50).is_empty());
    }

    // The newest lines are read by id, not by walking the buffer from its start: the ids asked
    // for are only the tail's, also once older lines were evicted.
    #[test]
    fn only_the_tail_is_read() {
        let mut buffer = LogBuffer::new(100);
        for n in 0..250 {
            buffer.push(LogLine::parse(&format!("line {n}"), false));
        }
        let lines = buffer.view();
        assert!(lines.first_id() > 0, "older lines were evicted");
        assert_eq!(super::tail_ids(&lines, 50), 200..250);
        assert_eq!(
            super::tail_ids(&lines, 1000),
            lines.first_id()..250,
            "never before the oldest line"
        );
        assert_eq!(super::tail_ids(&LogBuffer::new(4).view(), 50), 0..0);
        let last = last_lines(&lines, 50);
        assert_eq!((last[0].id, last[49].id), (200, 249));
        assert_eq!(last[49].text, "line 249");
    }

    // A reconnect for a reason the tab has no words for says the error's gist, as the screen
    // does, not "error".
    #[test]
    fn an_unknown_reconnect_reason_is_its_gist() {
        let reason = |error: &str| match status(
            &LogState::Reconnecting {
                error: Some(error.into()),
                attempt: 3,
                back_off: false,
            },
            true,
        ) {
            TabStatusState::Reconnecting { reason, .. } => reason,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            reason("stream reset by the proxy\nmore detail").as_deref(),
            Some("stream reset by the proxy")
        );
        let screen = crate::ui::log_model::state_text(
            &LogState::Reconnecting {
                error: Some("stream reset by the proxy".into()),
                attempt: 3,
                back_off: false,
            },
            true,
        )
        .text;
        assert_eq!(
            screen,
            "Reconnecting (attempt 3) · stream reset by the proxy"
        );
    }

    #[test]
    fn streams_are_counted_as_the_toolbar_counts_them() {
        let stream = |pod: &str, state| StreamStatus {
            source: Arc::new(LineSource::new(pod, "app")),
            state,
        };
        let counts = stream_counts(&PodSetStatus {
            selector: Some("app=web".into()),
            pods: 3,
            streams: vec![
                stream("web-1", LogState::Streaming),
                stream(
                    "web-2",
                    LogState::Ended {
                        error: Some("Forbidden".into()),
                        pod_deleted: false,
                    },
                ),
                stream(
                    "web-3",
                    LogState::Reconnecting {
                        error: None,
                        attempt: 1,
                        back_off: false,
                    },
                ),
                stream(
                    "web-4",
                    LogState::Ended {
                        error: None,
                        pod_deleted: false,
                    },
                ),
            ],
            waiting: 1,
            capped: 2,
            max_streams: 32,
        });
        assert_eq!(
            serde_json::to_value(counts).unwrap(),
            json!({"pods": 3, "live": 1, "connecting": 1, "waiting": 1, "failed": 1, "ended": 1,
                   "capped": 2, "each": [
                {"pod": "web-1", "container": "app", "state": "streaming"},
                {"pod": "web-2", "container": "app", "state": "failed"},
                {"pod": "web-3", "container": "app", "state": "reconnecting"},
                {"pod": "web-4", "container": "app", "state": "ended"},
            ]})
        );
        // Bounded.
        let many = PodSetStatus {
            pods: 100,
            streams: (0..100)
                .map(|n| stream(&format!("web-{n}"), LogState::Streaming))
                .collect(),
            ..Default::default()
        };
        assert_eq!(
            stream_counts(&many).each.len(),
            crate::ui::view_state::StreamCounts::LIMIT
        );
    }
}
