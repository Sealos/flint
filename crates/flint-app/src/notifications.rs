//! Turns alert events into desktop notifications, behind a small [`NotificationSender`] trait.
//!
//! Nothing here runs on its own: a caller builds a [`Notification`] and hands it to a sender.
//!
//! Names come from the cluster and are hostile. Shown text goes through [`clean`], which bounds
//! it and removes control, invisible and markup characters, because freedesktop servers may
//! render a body as markup. Senders pass text as separate arguments, never inside a script.

#![cfg_attr(
    not(test),
    allow(dead_code, reason = "wired in by the alert delivery work")
)]

#[path = "investigate_prompt.rs"]
pub mod investigate_prompt;

use std::io;
use std::process::{Command, Stdio};
use std::sync::Arc;

use flint_core::alerts::{Alert, AlertEvent, AlertSubject};
use flint_core::display::single_line;
use flint_core::problems::ProblemSeverity;

/// The most characters of one name shown in a notification.
const MAX_NAME_CHARS: usize = 64;

/// The most characters of a name examined, so a huge value costs a bounded amount of work.
const MAX_SCANNED_CHARS: usize = 4096;

/// Shown in place of a name with nothing printable left.
const UNNAMED: &str = "(unnamed)";

/// A platform-neutral desktop notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// Cleaned and bounded.
    pub title: String,
    /// Cleaned and bounded.
    pub body: String,
    /// The exact cluster to open. An identity for lookups, never for display.
    pub cluster: Arc<str>,
}

impl Notification {
    /// The notification for an alert event. `cluster_label` is what the title calls the cluster:
    /// [`flint_core::display::ClusterNames::with_real`], so similar clusters stay distinct. It is
    /// cleaned like any other shown text.
    pub fn from_event(event: &AlertEvent, cluster_label: &str) -> Self {
        let text = event.text();
        let (heading, body, cluster) = match event {
            AlertEvent::Raised(alert) => (
                severity_heading(alert.severity()),
                with_subject(&text, alert),
                &alert.cluster,
            ),
            AlertEvent::Resolved(alert) => {
                ("Alert resolved", with_subject(&text, alert), &alert.cluster)
            }
            AlertEvent::Summary(summary) => {
                let heading = match summary.highest {
                    Some(ProblemSeverity::Critical) => "Critical alerts",
                    _ => "Alerts",
                };
                (heading, text, &summary.cluster)
            }
            AlertEvent::ClusterUnavailable { cluster, .. } => {
                ("Cluster unavailable", text, cluster)
            }
            AlertEvent::ClusterAvailable { cluster } => ("Cluster available", text, cluster),
        };
        Self {
            title: format!("{heading}: {}", clean(cluster_label)),
            body,
            cluster: Arc::clone(cluster),
        }
    }
}

fn severity_heading(severity: ProblemSeverity) -> &'static str {
    match severity {
        ProblemSeverity::Critical => "Critical alert",
        ProblemSeverity::Warning => "Warning",
    }
}

fn with_subject(text: &str, alert: &Alert) -> String {
    format!("{text}: {}", subject_label(&alert.key.subject))
}

fn subject_label(subject: &AlertSubject) -> String {
    match subject {
        AlertSubject::Workload { namespace, owner } => {
            format!(
                "{}/{}/{}",
                clean(namespace),
                clean(&owner.kind),
                clean(&owner.name)
            )
        }
        AlertSubject::Pod { namespace, name } | AlertSubject::Job { namespace, name } => {
            format!("{}/{}", clean(namespace), clean(name))
        }
        AlertSubject::Node { name } => clean(name),
    }
}

/// `text` safe to show on one line: whitespace becomes a space, control, invisible and markup
/// characters are removed, and a long value keeps both ends around an ellipsis.
fn clean(text: &str) -> String {
    let kept: Vec<char> = text
        .chars()
        .take(MAX_SCANNED_CHARS)
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .filter(|c| is_shown(*c))
        .collect();
    let kept = trimmed(&kept);
    if kept.is_empty() {
        return UNNAMED.to_owned();
    }
    if kept.len() <= MAX_NAME_CHARS {
        return kept.iter().collect();
    }
    let tail = (MAX_NAME_CHARS - 1) / 2;
    let head = MAX_NAME_CHARS - 1 - tail;
    let start = kept[..head].iter();
    let end = kept[kept.len() - tail..].iter();
    start.chain(['…'].iter()).chain(end).collect()
}

fn trimmed(chars: &[char]) -> &[char] {
    let start = chars.iter().take_while(|c| **c == ' ').count();
    let end = chars.len()
        - chars[start..]
            .iter()
            .rev()
            .take_while(|c| **c == ' ')
            .count();
    &chars[start..end]
}

/// Whether `c` may be shown: not markup, and not a character [`single_line`] would escape.
fn is_shown(c: char) -> bool {
    if matches!(c, '<' | '>' | '&') {
        return false;
    }
    if c == '\\' {
        return true;
    }
    let mut buffer = [0; 4];
    let text = c.encode_utf8(&mut buffer);
    single_line(text) == *text
}

/// Delivers a notification.
pub trait NotificationSender {
    /// Sends `notification`. May block briefly, so call it off the UI thread.
    fn send(&self, notification: &Notification) -> io::Result<()>;
}

/// Sends through `osascript`.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacosSender;

impl MacosSender {
    /// The title and body are script arguments, so they are never parsed as AppleScript. `--`
    /// ends the options, so they are never read as flags either.
    fn command(notification: &Notification) -> Command {
        let mut command = Command::new("/usr/bin/osascript");
        command
            .args(["-e", "on run argv"])
            .args([
                "-e",
                "display notification (item 2 of argv) with title (item 1 of argv)",
            ])
            .args(["-e", "end run"])
            .arg("--")
            .arg(&notification.title)
            .arg(&notification.body);
        command
    }
}

impl NotificationSender for MacosSender {
    fn send(&self, notification: &Notification) -> io::Result<()> {
        run(Self::command(notification))
    }
}

/// Sends through `notify-send`.
#[derive(Clone, Copy, Debug, Default)]
pub struct FreedesktopSender;

impl FreedesktopSender {
    /// `--` ends the options, so the title and body are never read as flags.
    fn command(notification: &Notification) -> Command {
        let mut command = Command::new("notify-send");
        command
            .arg("--app-name=Flint")
            .arg("--")
            .arg(&notification.title)
            .arg(&notification.body);
        command
    }
}

impl NotificationSender for FreedesktopSender {
    fn send(&self, notification: &Notification) -> io::Result<()> {
        run(Self::command(notification))
    }
}

fn run(mut command: Command) -> io::Result<()> {
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("notifier exited with {status}")))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::ffi::OsStr;

    use flint_core::alerts::{AlertKey, AlertSummary};
    use flint_core::display::ClusterNames;
    use flint_core::problems::{ProblemKind, ProblemOwner};

    use super::*;

    const HOSTILE: &str = "a\nb\r\t\u{1b}[31m\u{202e}\u{200b}<b>x</b>&amp;\u{7f}";

    fn alert(cluster: &str, subject: AlertSubject) -> Alert {
        Alert {
            cluster: Arc::from(cluster),
            key: AlertKey {
                kind: ProblemKind::CrashLoop,
                subject,
            },
            affected: 1,
            summary: "Container keeps crashing",
        }
    }

    fn pod(name: &str) -> AlertSubject {
        AlertSubject::Pod {
            namespace: "default".into(),
            name: name.into(),
        }
    }

    fn assert_safe(text: &str) {
        assert!(text.chars().all(is_shown), "unsafe character in {text:?}");
    }

    #[derive(Default)]
    struct FakeSender {
        sent: RefCell<Vec<Notification>>,
    }

    impl NotificationSender for FakeSender {
        fn send(&self, notification: &Notification) -> io::Result<()> {
            self.sent.borrow_mut().push(notification.clone());
            Ok(())
        }
    }

    #[test]
    fn a_raised_alert_names_its_cluster_and_subject() {
        let event = AlertEvent::Raised(alert("prod", pod("api-0")));
        let notification = Notification::from_event(&event, "prod");
        assert_eq!(notification.title, "Critical alert: prod");
        assert_eq!(notification.body, "Container keeps crashing: default/api-0");
        assert_eq!(&*notification.cluster, "prod");
    }

    #[test]
    fn every_event_kind_has_a_heading_and_target() {
        let cluster: Arc<str> = Arc::from("prod");
        let cases = [
            (
                AlertEvent::Resolved(alert("prod", pod("api-0"))),
                "Alert resolved: prod",
            ),
            (
                AlertEvent::Summary(AlertSummary {
                    cluster: Arc::clone(&cluster),
                    new: 3,
                    resolved: 0,
                    untracked: 0,
                    highest: Some(ProblemSeverity::Critical),
                }),
                "Critical alerts: prod",
            ),
            (
                AlertEvent::Summary(AlertSummary {
                    cluster: Arc::clone(&cluster),
                    new: 0,
                    resolved: 2,
                    untracked: 0,
                    highest: None,
                }),
                "Alerts: prod",
            ),
            (
                AlertEvent::ClusterUnavailable {
                    cluster: Arc::clone(&cluster),
                    open: 2,
                },
                "Cluster unavailable: prod",
            ),
            (
                AlertEvent::ClusterAvailable {
                    cluster: Arc::clone(&cluster),
                },
                "Cluster available: prod",
            ),
        ];
        for (event, title) in cases {
            let notification = Notification::from_event(&event, "prod");
            assert_eq!(notification.title, title);
            assert_eq!(notification.cluster, cluster);
        }
    }

    #[test]
    fn hostile_names_are_stripped_but_the_target_stays_exact() {
        let subject = AlertSubject::Workload {
            namespace: HOSTILE.into(),
            owner: ProblemOwner {
                group: "apps".into(),
                kind: HOSTILE.into(),
                name: HOSTILE.into(),
            },
        };
        let event = AlertEvent::Raised(alert(HOSTILE, subject));
        let notification = Notification::from_event(&event, HOSTILE);
        assert_safe(&notification.title);
        assert_safe(&notification.body);
        assert!(!notification.body.contains('<'));
        assert_eq!(&*notification.cluster, HOSTILE);
    }

    #[test]
    fn similar_clusters_keep_distinct_titles() {
        let project = "a-very-long-gcp-project-name-for-testing";
        let name = "a-very-long-shared-cluster-name-for-testing";
        let first = format!("gke_{project}_europe-west1-b_{name}");
        let second = format!("gke_{project}_europe-west1-c_{name}");
        assert_eq!(clean(&first), clean(&second));

        let names = ClusterNames::new([first.as_str(), second.as_str()], &BTreeMap::new());
        let title = |context: &str| {
            let event = AlertEvent::Raised(alert(context, pod("api-0")));
            Notification::from_event(&event, &names.with_real(context)).title
        };
        assert_ne!(title(&first), title(&second));
    }

    #[test]
    fn opening_a_notification_uses_the_exact_cluster_not_its_label() {
        let event = AlertEvent::Raised(alert("exact-context", pod("api-0")));
        let notification = Notification::from_event(&event, "Label\n<b>");
        assert_eq!(notification.title, "Critical alert: Label b");
        assert_eq!(&*notification.cluster, "exact-context");
    }

    #[test]
    fn long_names_are_bounded_and_keep_both_ends() {
        let name = format!("start-{}-end", "x".repeat(100_000));
        let shown = clean(&name);
        assert_eq!(shown.chars().count(), MAX_NAME_CHARS);
        assert!(shown.starts_with("start-"));
        assert!(shown.contains('…'));
        let short_tail = format!("start-{}-end", "x".repeat(1000));
        assert!(clean(&short_tail).ends_with("-end"));
    }

    #[test]
    fn a_name_with_nothing_printable_reads_as_unnamed() {
        assert_eq!(clean("\n\u{1b}\u{202e} \t"), UNNAMED);
        assert_eq!(clean(""), UNNAMED);
    }

    #[test]
    fn senders_pass_text_as_separate_arguments() {
        let notification = Notification {
            title: "--title\" & do shell script \"id".into(),
            body: "-body".into(),
            cluster: Arc::from("prod"),
        };
        let mac = MacosSender::command(&notification);
        let args: Vec<&OsStr> = mac.get_args().collect();
        assert_eq!(args[args.len() - 3], OsStr::new("--"));
        assert_eq!(args[args.len() - 2], OsStr::new(&notification.title));
        assert_eq!(args[args.len() - 1], OsStr::new("-body"));
        assert!(
            args[..args.len() - 2]
                .iter()
                .all(|arg| !arg.to_string_lossy().contains("shell script"))
        );

        let linux = FreedesktopSender::command(&notification);
        let args: Vec<&OsStr> = linux.get_args().collect();
        assert_eq!(args[args.len() - 3], OsStr::new("--"));
        assert_eq!(args[args.len() - 2], OsStr::new(&notification.title));
        assert_eq!(args[args.len() - 1], OsStr::new("-body"));
    }

    #[test]
    fn a_failing_notifier_is_an_error() {
        assert!(run(Command::new("false")).is_err());
        assert!(run(Command::new("true")).is_ok());
    }

    #[test]
    fn a_sender_receives_the_notification() {
        let sender = FakeSender::default();
        let notification =
            Notification::from_event(&AlertEvent::Raised(alert("prod", pod("api-0"))), "prod");
        let dyn_sender: &dyn NotificationSender = &sender;
        dyn_sender.send(&notification).unwrap();
        assert_eq!(*sender.sent.borrow(), vec![notification]);
    }
}
