//! What the main area says while the selected cluster is connecting or waiting to try again.

use std::time::{Duration, Instant};

use flint_core::{ClusterSummary, ConnectionProgress, display};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::*;

use super::registry::{self, CancelConnection};
use super::workspace::server_text;

/// How long a connection stays a plain "Connecting…" before naming its server and time.
const DETAIL_AFTER: Duration = Duration::from_secs(5);

/// `12s`, or `3m 05s` from a minute on.
fn elapsed_text(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

/// The line for an attempt under way, once it has run for [`DETAIL_AFTER`]. The server is
/// sanitized and kept on one line, since it comes from a kubeconfig.
fn connecting_text(server: Option<&str>, elapsed: Duration) -> Option<String> {
    (elapsed >= DETAIL_AFTER).then(|| {
        format!(
            "Connecting to {} · {}",
            display::single_line(&server_text(server)),
            elapsed_text(elapsed)
        )
    })
}

/// The line for a failure with an automatic retry waiting. Rounded up, so it never reads 0s
/// before the retry has started.
fn retry_text(progress: &ConnectionProgress, now: Instant) -> Option<String> {
    let left = progress.next_retry?.saturating_duration_since(now);
    let seconds = left.as_secs() + u64::from(left.subsec_nanos() > 0);
    Some(format!(
        "Retrying in {} · retry {}",
        elapsed_text(Duration::from_secs(seconds)),
        progress.retries
    ))
}

fn cancel_button() -> impl IntoElement {
    Button::new("connection-cancel")
        .small()
        .ghost()
        .label("Cancel")
        .on_click(registry::on_click(CancelConnection))
}

/// The main area while the selected cluster connects.
pub(super) fn connecting(
    cluster: &ClusterSummary,
    progress: Option<&ConnectionProgress>,
    theme: &Theme,
) -> AnyElement {
    let detail =
        progress.and_then(|p| connecting_text(cluster.info.server.as_deref(), p.since.elapsed()));
    let Some(detail) = detail else {
        return div()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child("Connecting…")
            .into_any_element();
    };
    v_flex()
        .gap_2()
        .items_center()
        .max_w(px(640.))
        .child(
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(detail),
        )
        .child(cancel_button())
        .into_any_element()
}

/// A line above a view that stays open while the selected cluster connects, once there is
/// something to say.
pub(super) fn connecting_notice(
    cluster: &ClusterSummary,
    progress: Option<&ConnectionProgress>,
    theme: &Theme,
) -> Option<AnyElement> {
    let text = connecting_text(cluster.info.server.as_deref(), progress?.since.elapsed())?;
    Some(
        h_flex()
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(text),
            )
            .child(cancel_button())
            .into_any_element(),
    )
}

/// The waiting retry under a failure, with its Cancel, if one is waiting.
pub(super) fn retry_notice(
    progress: Option<&ConnectionProgress>,
    theme: &Theme,
) -> Option<AnyElement> {
    let text = retry_text(progress?, Instant::now())?;
    Some(
        h_flex()
            .pt_1()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(text),
            )
            .child(cancel_button())
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use flint_core::ConnectionProgress;

    use super::{connecting_text, elapsed_text, retry_text};

    fn progress(now: Instant, next_retry: Option<Duration>) -> ConnectionProgress {
        ConnectionProgress {
            cluster: "prod".into(),
            generation: 1,
            since: now,
            retries: 3,
            next_retry: next_retry.map(|wait| now + wait),
        }
    }

    #[test]
    fn the_server_and_time_appear_after_five_seconds() {
        let server = Some("https://user:secret@example.test:6443/");
        assert_eq!(connecting_text(server, Duration::from_millis(4_999)), None);
        let text = connecting_text(server, Duration::from_secs(5)).unwrap();
        assert_eq!(text, "Connecting to https://example.test:6443 · 5s");
    }

    #[test]
    fn a_long_wait_reads_in_minutes() {
        assert_eq!(elapsed_text(Duration::from_secs(59)), "59s");
        assert_eq!(elapsed_text(Duration::from_secs(185)), "3m 05s");
    }

    #[test]
    fn the_retry_countdown_rounds_up() {
        let now = Instant::now();
        let waiting = progress(now, Some(Duration::from_millis(7_200)));
        assert_eq!(
            retry_text(&waiting, now).as_deref(),
            Some("Retrying in 8s · retry 3")
        );
    }

    #[test]
    fn no_waiting_retry_shows_nothing() {
        let now = Instant::now();
        assert_eq!(retry_text(&progress(now, None), now), None);
    }
}
