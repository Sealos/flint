//! A cluster's agent activity (UP-16, ADR-0013): every call an AI agent made to Flint's MCP
//! server on the cluster, newest first, as `flint mcp` records them ([`flint_core::activity`]).
//! A dock tab that updates while the agent works: the core reads the log whenever it grows and
//! the cluster model holds it. It says what was asked for and how much came back; what came back
//! is never stored.

use std::sync::Arc;

use flint_core::ClusterId;
use flint_core::activity::{self, ActivityRecord, Outcome};
use flint_core::settings::server_identity;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::Timestamp;
use jiff::tz::TimeZone;

use super::clusters::ClustersModel;
use super::dock::{DockItem, TabKey, TabStatus};
use super::registry::{self, EditAiScope};

/// A row's height.
const ROW_HEIGHT: f32 = 24.;
/// What the view says before an agent called anything on the cluster.
pub const EMPTY: &str = "No agent has called Flint's MCP tools on this cluster yet.";

/// One call, as the view shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityRow {
    /// Local time; with the date when not today.
    pub time: String,
    pub tool: String,
    /// What the call named (`pods web/web-0, container app`), or a dash.
    pub target: String,
    /// How it ended: what came back, or `denied`, `failed`.
    pub result: String,
    pub outcome: Outcome,
}

impl ActivityRow {
    /// The row in one line, for accessibility and tests.
    pub fn label(&self) -> String {
        format!(
            "{} {} {} — {}",
            self.time, self.tool, self.target, self.result
        )
    }
}

/// The calls on `cluster` at the server with key `server` ([`activity::ServerKeys`]) in
/// `records`, newest first, with times in `tz` and `now`
/// deciding whether a date is shown: those that named the context, and those that probed it
/// (`list_clusters`). A context re-pointed at another server is another cluster: its calls there
/// are not shown (a record without a server, from a context not exposed, is). A call that probed
/// more clusters than its record lists ([`ActivityRecord::probed_more`]) is shown, marked, in the
/// tab of every cluster it does not list: it may have probed it.
pub fn rows(
    records: &[ActivityRecord],
    cluster: &str,
    server: Option<&str>,
    tz: &TimeZone,
    now: Timestamp,
) -> Vec<ActivityRow> {
    let today = now.to_zoned(tz.clone()).date();
    // By key: the names in a record may be cut.
    let cluster = activity::cluster_key(cluster);
    let mut mine: Vec<(usize, &ActivityRecord)> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| r.is_about(&cluster, server) || maybe_probed(r, &cluster))
        .collect();
    // Newest first; calls in the same millisecond (several `flint mcp` processes) in the order
    // they were written, last first.
    mine.sort_by_key(|(ix, r)| std::cmp::Reverse((r.at_ms, *ix)));
    mine.into_iter()
        .map(|(_, r)| ActivityRow {
            time: time(r.at_ms, tz, today),
            tool: shown(&r.tool),
            target: if r.is_about(&cluster, server) {
                target(r)
            } else {
                format!(
                    "{} clusters, maybe this one: not all are listed",
                    r.probed.len() as u64 + r.probed_more
                )
            },
            result: result(r),
            outcome: r.outcome,
        })
        .collect()
}

/// Whether `r` probed more clusters than it lists and `cluster` is not among those it lists: then
/// it may have probed `cluster`.
fn maybe_probed(r: &ActivityRecord, cluster: &str) -> bool {
    r.probed_more > 0 && !r.probed.iter().any(|p| p.cluster_key == cluster)
}

fn time(at_ms: i64, tz: &TimeZone, today: jiff::civil::Date) -> String {
    let Ok(at) = Timestamp::from_millisecond(at_ms) else {
        return "?".into();
    };
    let at = at.to_zoned(tz.clone());
    if at.date() == today {
        at.strftime("%H:%M:%S").to_string()
    } else {
        at.strftime("%Y-%m-%d %H:%M:%S").to_string()
    }
}

/// `text`, written by an agent, as the view shows it: control characters and the invisible
/// format characters that reorder or hide text (bidi embeddings, overrides and isolates,
/// zero-width and other default-ignorable characters, line and paragraph separators, blank
/// fillers) are shown as escapes (`\n`, `\u{202e}`), so a name cannot break its
/// row or pass itself off as another one. A backslash is doubled, so an escape and the same
/// characters written by the agent read differently. Everything else is kept.
fn shown(text: &str) -> String {
    fn hidden(c: char) -> bool {
        matches!(
            c,
            '\u{00ad}'
                | '\u{034f}'
                | '\u{061c}'
                | '\u{115f}'..='\u{1160}'
                | '\u{17b4}'..='\u{17b5}'
                | '\u{180b}'..='\u{180f}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{3164}'
                | '\u{fe00}'..='\u{fe0f}'
                | '\u{feff}'
                | '\u{ffa0}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0000}'..='\u{e0fff}'
        )
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\\' {
            out.push_str("\\\\");
        } else if c.is_control() {
            out.extend(c.escape_default());
        } else if hidden(c) {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// What a call named: the kind, the object (`namespace/name`, or the namespace alone), and the
/// container.
fn target(r: &ActivityRecord) -> String {
    // Each field escaped on its own, before they are joined: a name cannot pass itself off as
    // another field (an escaped `, container x` still reads as part of the name it is in).
    let field = |text: &Option<String>| text.as_deref().map(shown);
    let object = match (field(&r.namespace), field(&r.name)) {
        (Some(ns), Some(name)) => Some(format!("{ns}/{name}")),
        (None, Some(name)) => Some(name),
        (Some(ns), None) => Some(format!("in {ns}")),
        (None, None) => None,
    };
    let mut text = field(&r.kind)
        .into_iter()
        .chain(object)
        .collect::<Vec<_>>()
        .join(" ");
    if let Some(container) = field(&r.container) {
        text.push_str(&format!(", container {container}"));
    }
    if text.is_empty() { "—".into() } else { text }
}

/// How a call ended, with the size of its reply; never a failed call's error text.
fn result(r: &ActivityRecord) -> String {
    match r.outcome {
        Outcome::Denied => format!("denied: outside what agents may read, {}", size(r.bytes)),
        Outcome::Failed => format!("failed, {}", size(r.bytes)),
        Outcome::Ok => {
            let mut text = match r.items {
                Some(1) => "1 item".to_owned(),
                Some(n) => format!("{n} items"),
                None => String::new(),
            };
            if !text.is_empty() {
                text.push_str(", ");
            }
            text.push_str(&size(r.bytes));
            if r.truncated {
                text.push_str(", cut short");
            }
            text
        }
    }
}

/// A byte count in a few characters.
fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.),
    }
}

pub struct ActivityView {
    cluster: ClusterId,
    /// The server the context pointed at when the tab opened.
    server: Option<String>,
    /// The log the rows were built from: rebuilt only when it changes.
    shown: Option<Arc<Vec<ActivityRecord>>>,
    model: Entity<ClustersModel>,
    focus: FocusHandle,
    scroll: UniformListScrollHandle,
    tz: TimeZone,
    /// The rows shown, rebuilt when the model changes.
    rows: Vec<ActivityRow>,
    _observe: Subscription,
}

impl ActivityView {
    /// The tab for `cluster` at `server`. Calls are matched by the server's key under the log's
    /// own keys ([`ClustersModel::activity_keys`]), the ones `flint mcp` records with.
    pub fn new(
        model: Entity<ClustersModel>,
        cluster: ClusterId,
        server: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&model, |view, _, cx| {
            view.refresh(cx);
            cx.notify();
        });
        let mut view = ActivityView {
            cluster,
            server,
            shown: None,
            model,
            focus: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            tz: TimeZone::system(),
            rows: Vec::new(),
            _observe: observe,
        };
        view.refresh(cx);
        view
    }

    fn refresh(&mut self, cx: &App) {
        let model = self.model.read(cx);
        let records = model.agent_activity.clone();
        // The model also changes for connections, discovery and settings.
        if self
            .shown
            .as_ref()
            .is_some_and(|s| Arc::ptr_eq(s, &records))
        {
            return;
        }
        // None without a key (then calls match by context alone).
        let server_key = self
            .server
            .as_deref()
            .and_then(|s| model.activity_keys.server_key(s));
        self.rows = rows(
            &records,
            &self.cluster,
            server_key.as_deref(),
            &self.tz,
            Timestamp::now(),
        );
        self.shown = Some(records);
    }

    /// The rows, newest first.
    #[cfg(any(test, feature = "automation"))]
    pub fn rows(&self) -> &[ActivityRow] {
        &self.rows
    }

    fn render_rows(
        &mut self,
        range: std::ops::Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme();
        range
            .filter_map(|ix| {
                let row = self.rows.get(ix)?;
                let result_color = match row.outcome {
                    Outcome::Ok => theme.muted_foreground,
                    Outcome::Denied => theme.warning,
                    Outcome::Failed => theme.danger,
                };
                Some(
                    h_flex()
                        .id(("activity-row", ix))
                        .aria_label(row.label())
                        .h(px(ROW_HEIGHT))
                        .w_full()
                        .px_3()
                        .gap_3()
                        .text_xs()
                        .child(
                            div()
                                .w(px(132.))
                                .flex_none()
                                .text_color(theme.muted_foreground)
                                .child(row.time.clone()),
                        )
                        .child(div().w(px(140.)).flex_none().child(row.tool.clone()))
                        .child({
                            // Names may be cut in the cell, and in the log at 253 bytes (with
                            // how much was left out): the tooltip has what the log has.
                            let target = row.target.clone();
                            div()
                                .id(("activity-target", ix))
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(target.clone())
                                .tooltip(move |window, cx| {
                                    Tooltip::new(target.clone()).build(window, cx)
                                })
                        })
                        .child(
                            div()
                                .flex_none()
                                .text_color(result_color)
                                .child(row.result.clone()),
                        )
                        .test_support()
                        .into_any_element(),
                )
            })
            .collect()
    }
}

impl DockItem for ActivityView {
    fn tab_key(&self) -> TabKey {
        TabKey::Activity(self.cluster.clone())
    }

    fn tab_title(&self) -> SharedString {
        "Agent activity".into()
    }

    fn tab_tooltip(&self) -> SharedString {
        format!(
            "What AI agents asked Flint's MCP server for on {}, newest first. What they read is \
             not stored.",
            self.cluster
        )
        .into()
    }

    fn tab_status(&self) -> TabStatus {
        TabStatus::Live
    }

    fn tab_focus(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn can_restart(&self) -> bool {
        false
    }
}

impl Render for ActivityView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let count = match self.rows.len() {
            1 => "1 call".to_owned(),
            n => format!("{n} calls"),
        };
        // Only while the context still points at the server this tab is for: the scope is the
        // exposure's at the server the context points at now.
        let exposed = {
            let model = self.model.read(cx);
            model
                .clusters
                .iter()
                .find(|c| c.info.name == self.cluster)
                .filter(|c| {
                    c.info.server.as_deref().map(server_identity)
                        == self.server.as_deref().map(server_identity)
                })
                .and_then(|c| model.ai_scope(c))
                .is_some()
        };
        let rows = self.rows.len();
        v_flex()
            .id("agent-activity")
            .track_focus(&self.focus)
            .size_full()
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .child(
                        div()
                            .id("activity-count")
                            .flex_1()
                            .text_color(theme.muted_foreground)
                            .aria_label(count.clone())
                            .child(count)
                            .test_support(),
                    )
                    .when(exposed, |el| {
                        el.child(
                            Button::new("activity-edit-scope")
                                .ghost()
                                .xsmall()
                                .label("Edit AI Scope…")
                                .on_click(registry::on_click(EditAiScope {
                                    cluster: self.cluster.clone(),
                                })),
                        )
                    }),
            )
            .when(rows == 0, |el| {
                el.child(
                    div()
                        .id("activity-empty")
                        .p_3()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .aria_label(EMPTY)
                        .child(EMPTY)
                        .test_support(),
                )
            })
            .when(rows > 0, |el| {
                el.child(
                    uniform_list("activity-rows", rows, cx.processor(Self::render_rows))
                        .track_scroll(&self.scroll)
                        .flex_1(),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use flint_core::activity::{self, ActivityRecord, Outcome};
    use jiff::Timestamp;
    use jiff::tz::TimeZone;

    use super::{ActivityRow, result, rows, shown, size, target};

    /// Server keys under a fixed key.
    fn keys() -> activity::ServerKeys {
        activity::ServerKeys::with_secret(&[7; 32])
    }

    fn server_key(url: &str) -> String {
        keys().server_key(url).unwrap()
    }

    fn record(at_ms: i64, cluster: &str) -> ActivityRecord {
        ActivityRecord {
            at_ms,
            tool: "get_pod_logs".into(),
            cluster: Some(cluster.into()),
            server: Some("https://prod:6443".into()),
            kind: Some("pods".into()),
            namespace: Some("web".into()),
            name: Some("web-0".into()),
            container: Some("app".into()),
            items: Some(120),
            bytes: 5120,
            truncated: true,
            outcome: Outcome::Ok,
            cluster_key: Some(activity::cluster_key(cluster)),
            server_key: Some(server_key("https://prod:6443")),
            probed: Vec::new(),
            probed_more: 0,
        }
    }

    // covers UP-16: each call says when, which tool, what it named, how much came back, and
    // whether it was cut short or denied; newest first, this cluster only.
    #[test]
    fn rows_say_what_each_call_named_and_got() {
        let tz = TimeZone::UTC;
        let now: Timestamp = "2026-11-20T12:00:00Z".parse().unwrap();
        let today = now.as_millisecond() - 60_000;
        let yesterday = now.as_millisecond() - 86_400_000;
        let denied = ActivityRecord {
            tool: "list_resources".into(),
            kind: Some("secrets".into()),
            name: None,
            container: None,
            items: None,
            bytes: 80,
            outcome: Outcome::Denied,
            ..record(today, "prod")
        };
        let clusters = ActivityRecord {
            tool: "list_clusters".into(),
            cluster: None,
            cluster_key: None,
            ..record(today, "prod")
        };
        let records = [
            record(yesterday, "prod"),
            record(today, "staging"),
            clusters,
            denied,
        ];
        let labels: Vec<String> = rows(
            &records,
            "prod",
            Some(&server_key("https://prod:6443/")),
            &tz,
            now,
        )
        .iter()
        .map(ActivityRow::label)
        .collect();
        assert_eq!(
            labels,
            [
                "11:59:00 list_resources secrets in web — denied: outside what agents may read, \
                 80 B",
                "2026-11-19 12:00:00 get_pod_logs pods web/web-0, container app — 120 items, 5.0 \
                 KB, cut short",
            ]
        );
    }

    // covers UP-16: what an agent wrote cannot break its row or pass itself off as another
    // target: control characters and the invisible characters that reorder or hide text are
    // shown as escapes, in the cell and its tooltip (which show the same text).
    #[test]
    fn agent_text_is_shown_with_control_characters_escaped() {
        let now: Timestamp = "2026-11-20T12:00:00Z".parse().unwrap();
        let sneaky = ActivityRecord {
            tool: "get_\u{1b}[2Jresource".into(),
            namespace: Some("web\u{202e}metsys-ebuk".into()),
            name: Some("web-0\nkube-system/vault-0".into()),
            container: Some("app\u{2066}\u{200b}\t".into()),
            ..record(now.as_millisecond(), "prod")
        };
        let row = rows(&[sneaky], "prod", None, &TimeZone::UTC, now)
            .pop()
            .expect("the call is listed");
        assert_eq!(row.tool, "get_\\u{1b}[2Jresource");
        assert_eq!(
            row.target,
            "pods web\\u{202e}metsys-ebuk/web-0\\nkube-system/vault-0, container \
             app\\u{2066}\\u{200b}\\t"
        );
        for text in [&row.tool, &row.target] {
            assert!(
                !text
                    .chars()
                    .any(|c| c.is_control() || ('\u{2000}'..='\u{2fff}').contains(&c)),
                "{text:?}"
            );
        }
        // Everything else is kept as written: other scripts, emoji, punctuation.
        assert_eq!(
            shown("pods ns/naïve-日本-🚀 \"q\""),
            "pods ns/naïve-日本-🚀 \"q\""
        );
        // A backslash is doubled: the two characters `\n` written by the agent do not read as an
        // escaped newline.
        assert_eq!(shown("a\\nb"), "a\\\\nb");
        assert_eq!(shown("a\nb"), "a\\nb");
        assert_ne!(shown("a\\nb"), shown("a\nb"));
        // Other invisible characters: separators, soft hyphens, joiners, fillers, tags.
        for c in [
            '\u{00ad}',
            '\u{034f}',
            '\u{2028}',
            '\u{2029}',
            '\u{206a}',
            '\u{3164}',
            '\u{e0041}',
        ] {
            assert_eq!(
                shown(&format!("a{c}b")),
                format!("a{}b", c.escape_unicode())
            );
        }
        // Each field is escaped on its own, before the fields are joined.
        let spoof = ActivityRecord {
            name: Some("web-0\u{2028}".into()),
            container: None,
            ..record(now.as_millisecond(), "prod")
        };
        assert_eq!(target(&spoof), "pods web/web-0\\u{2028}");
    }

    // covers UP-16: a context re-pointed at another server shows only the calls on the server its
    // tab is for.
    #[test]
    fn rows_are_for_the_tabs_server() {
        let now: Timestamp = "2026-11-20T12:00:00Z".parse().unwrap();
        let at = |server: Option<&str>| ActivityRecord {
            server: server.map(str::to_owned),
            server_key: server.map(server_key),
            ..record(now.as_millisecond(), "prod")
        };
        let records = [
            at(Some("https://old:6443")),
            at(Some("https://prod:6443")),
            at(None),
        ];
        let count = |server: Option<&str>| {
            let key = server.map(server_key);
            rows(&records, "prod", key.as_deref(), &TimeZone::UTC, now).len()
        };
        assert_eq!(count(Some("https://prod:6443")), 2);
        assert_eq!(count(Some("https://old:6443/")), 2);
        assert_eq!(count(None), 3);
    }

    // covers UP-16: list_clusters names no cluster but reads about each exposed one: it shows on
    // the tab of each cluster it probed, at the server it probed.
    #[test]
    fn rows_list_the_calls_that_probed_the_cluster() {
        let now: Timestamp = "2026-11-20T12:00:00Z".parse().unwrap();
        let listed = ActivityRecord {
            tool: "list_clusters".into(),
            cluster: None,
            server: None,
            kind: None,
            namespace: None,
            name: None,
            container: None,
            items: Some(2),
            bytes: 300,
            truncated: false,
            cluster_key: None,
            server_key: None,
            probed: vec![
                activity::Probed::new(&keys(), "prod", Some("https://prod:6443")),
                activity::Probed::new(&keys(), "dev", Some("https://dev:6443")),
            ],
            ..record(now.as_millisecond(), "prod")
        };
        let labels = |cluster, server: Option<&str>| -> Vec<String> {
            let key = server.map(server_key);
            rows(
                std::slice::from_ref(&listed),
                cluster,
                key.as_deref(),
                &TimeZone::UTC,
                now,
            )
            .iter()
            .map(ActivityRow::label)
            .collect()
        };
        assert_eq!(
            labels("prod", Some("https://prod:6443/")),
            ["12:00:00 list_clusters — — 2 items, 300 B"]
        );
        assert_eq!(labels("dev", None).len(), 1);
        assert!(labels("prod", Some("https://elsewhere:6443")).is_empty());
        assert!(labels("staging", None).is_empty());

        // One that probed more than it lists shows, marked, in the tab of every cluster it does
        // not list: it may have probed it. Not in a listed one's tab at another server.
        let more = ActivityRecord {
            probed_more: 300,
            ..listed
        };
        let labels = |cluster| -> Vec<String> {
            rows(
                std::slice::from_ref(&more),
                cluster,
                Some(&server_key("https://staging:6443")),
                &TimeZone::UTC,
                now,
            )
            .iter()
            .map(ActivityRow::label)
            .collect()
        };
        assert_eq!(
            labels("staging"),
            [
                "12:00:00 list_clusters 302 clusters, maybe this one: not all are listed — 2 items, 300 B"
            ]
        );
        assert!(labels("prod").is_empty());
    }

    // covers UP-16: two servers that show as the same placeholder are two clusters: a context
    // re-pointed from one to the other does not show the old server's calls.
    #[test]
    fn rows_tell_apart_servers_that_show_alike() {
        let now: Timestamp = "2026-11-20T12:00:00Z".parse().unwrap();
        let old = "https://proxy.example.com/k8s/@old";
        let new = "https://proxy.example.com/k8s/@new";
        let call = ActivityRecord {
            server: Some(flint_core::redact::server_url(old)),
            server_key: Some(server_key(old)),
            ..record(now.as_millisecond(), "prod")
        };
        let count = |server| {
            rows(
                std::slice::from_ref(&call),
                "prod",
                Some(&server_key(server)),
                &TimeZone::UTC,
                now,
            )
            .len()
        };
        assert_eq!(count(old), 1);
        assert_eq!(count(new), 0);
    }

    // covers UP-16: newest first by time, calls in the same millisecond last-written first; a
    // context whose name was cut in the log still finds its calls (by key).
    #[test]
    fn rows_sort_by_time_and_match_by_key() {
        let now: Timestamp = "2026-11-20T12:00:00Z".parse().unwrap();
        let ms = now.as_millisecond();
        let call = |at: i64, tool: &str| ActivityRecord {
            tool: tool.into(),
            ..record(at, "prod")
        };
        let records = [
            call(ms - 10, "b"),
            call(ms - 20, "a"),
            call(ms, "c"),
            call(ms, "d"),
        ];
        let tools: Vec<String> = rows(&records, "prod", None, &TimeZone::UTC, now)
            .into_iter()
            .map(|r| r.tool)
            .collect();
        assert_eq!(tools, ["d", "c", "b", "a"]);

        let long = "p".repeat(400);
        let cut = ActivityRecord {
            cluster: Some(format!("{}…(+147 bytes)", &long[..253])),
            cluster_key: Some(activity::cluster_key(&long)),
            ..record(ms, "prod")
        };
        assert_eq!(rows(&[cut], &long, None, &TimeZone::UTC, now).len(), 1);
    }

    #[test]
    fn targets_and_sizes_read_naturally() {
        let bare = ActivityRecord {
            kind: None,
            namespace: None,
            name: None,
            container: None,
            ..record(0, "prod")
        };
        assert_eq!(target(&bare), "—");
        let cluster_scoped = ActivityRecord {
            kind: Some("nodes".into()),
            namespace: None,
            name: Some("node-1".into()),
            container: None,
            ..record(0, "prod")
        };
        assert_eq!(target(&cluster_scoped), "nodes node-1");
        let one = ActivityRecord {
            items: Some(1),
            bytes: 900,
            truncated: false,
            ..record(0, "prod")
        };
        assert_eq!(result(&one), "1 item, 900 B");
        // A failed or denied call says how big its reply was, never what it said.
        let failed = ActivityRecord {
            outcome: Outcome::Failed,
            items: None,
            bytes: 120,
            ..record(0, "prod")
        };
        assert_eq!(result(&failed), "failed, 120 B");
        assert_eq!(size(3 * 1_048_576), "3.0 MB");
    }
}
