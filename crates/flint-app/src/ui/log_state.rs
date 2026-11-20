//! A log tab's part of the visible view tree ([`crate::ui::view_state`]): its status, toggles,
//! search, selection, a workload's stream counts and its newest lines. Only the last
//! [`LogTabState::LINE_LIMIT`] lines go out, so a test can wait for a line without the state
//! carrying the buffer.

use flint_core::logs::LogLines;
use flint_core::{LogState, PodSetStatus, Severity};
use gpui_kit::*;

use super::super::log_model;
use super::super::view_state::{
    Focus, LogLineState, LogTabState, SearchState, SelectionState, StreamCounts, TabStatusState,
};
use super::LogView;

impl LogView {
    /// The tab as data.
    pub fn view_state(&self) -> LogTabState {
        let ended = self.has_ended();
        LogTabState {
            lines: self.snapshot.lines.len(),
            held_back: self.snapshot.held_back,
            paused: !ended && (self.snapshot.paused || self.is_paused()),
            follows: self.follows(),
            following: self.list.is_following_tail(),
            timestamps: self.timestamps,
            sources: self.format().source,
            wrap: self.wrap,
            search: SearchState {
                query: self.query.text.clone(),
                matches: self.search.count(),
                current: self
                    .current_match
                    .and_then(|id| self.search.matches().binary_search(&id).ok())
                    .map(|ix| ix + 1),
                searching: self.searching,
            },
            selection: self.selection.map(|s| SelectionState {
                first: s.start(),
                last: s.end(),
                lines: s.end() - s.start() + 1,
            }),
            streams: self.snapshot.pods.as_deref().map(stream_counts),
            last_lines: last_lines(&self.snapshot.lines, LogTabState::LINE_LIMIT),
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
                Some(e) => Some(log_model::explain(e, pod).short.to_owned()),
                None => None,
            },
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

/// A workload's streams, counted as the toolbar counts them.
pub fn stream_counts(status: &PodSetStatus) -> StreamCounts {
    StreamCounts {
        pods: status.pods,
        live: status.live(),
        connecting: status.connecting(),
        waiting: status.waiting,
        failed: status.failed(),
        capped: status.capped,
    }
}

/// The newest `limit` lines of `lines`, oldest first.
pub fn last_lines(lines: &LogLines, limit: usize) -> Vec<LogLineState> {
    let skip = lines.len().saturating_sub(limit);
    lines
        .iter()
        .skip(skip)
        .map(|(id, line)| LogLineState {
            id,
            text: line.text.to_string(),
            severity: severity_word(line.severity),
            source: line.source.as_ref().map(|s| s.label().to_owned()),
        })
        .collect()
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
            ],
            waiting: 1,
            capped: 2,
            max_streams: 32,
        });
        assert_eq!(
            serde_json::to_value(counts).unwrap(),
            json!({"pods": 3, "live": 1, "connecting": 1, "waiting": 1, "failed": 1, "capped": 2})
        );
    }
}
