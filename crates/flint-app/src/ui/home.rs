//! The fleet home screen (#806): what Flint shows on launch, and from the sidebar's Home entry.
//!
//! Each cluster kept watching gets a card (the shared [`cluster_card`]), read from its
//! keep-watching feed alone; every other cluster is a row with Open and Keep Watching. One flat
//! grid in sidebar order, no grouping by cluster set. Nothing here connects a cluster or starts a
//! feed, and Keep Watching goes through the same action as the row menu, so a sign-in command
//! still waits for its own consent.

use std::rc::Rc;

use flint_core::keep_watching::{BackgroundSignIn, KeepWatchView, KeepWatching};
use flint_core::settings::ClusterTag;
use flint_core::telemetry::VersionCheck;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{Disableable as _, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::Timestamp;

use super::cluster_tag::tag_dot;
use super::dashboard::cluster_card::{
    self, CARD_WIDTH, CardActions, ClusterCard, ClusterRead, render_card,
};
use super::focus::link_focus;
use super::keep_watching;
use super::registry::{self, SelectCluster, ToggleKeepWatching};

/// Which clusters get a card and which a row, each in sidebar order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct HomePlan {
    /// Kept watching (in any state but off), with that state.
    pub watched: Vec<(String, KeepWatching)>,
    pub unwatched: Vec<String>,
}

/// Splits `clusters` (in sidebar order, with their keep-watching state) into cards and rows.
pub(super) fn plan<'a>(clusters: impl IntoIterator<Item = (&'a str, KeepWatching)>) -> HomePlan {
    let mut plan = HomePlan::default();
    for (cluster, status) in clusters {
        match status {
            KeepWatching::Off => plan.unwatched.push(cluster.to_owned()),
            status => plan.watched.push((cluster.to_owned(), status)),
        }
    }
    plan
}

/// The card's word for a watched cluster's state.
pub(super) fn status_word(status: KeepWatching) -> &'static str {
    keep_watching::badge(status).unwrap_or(match status {
        KeepWatching::Paused => "paused",
        _ => "not watched",
    })
}

/// Why a watched cluster's feed may not be read now, if so: its card then shows no values.
pub(super) fn blocked_reason(status: KeepWatching) -> Option<String> {
    match status {
        KeepWatching::Watching | KeepWatching::Off => None,
        KeepWatching::SignInNeeded => {
            Some("Sign-in needed: its sign-in failed. Open the cluster to sign in.".to_owned())
        }
        KeepWatching::NeedsConfirming => {
            Some("Sign-in needed: open the cluster and allow its background sign-in.".to_owned())
        }
        KeepWatching::Paused => {
            Some("Paused: a kubeconfig file defining it can't be read.".to_owned())
        }
    }
}

/// What a card's Sign in was drawn for. The click is the consent to run that cluster's sign-in
/// command once, now, for exactly this (`Workspace::sign_in_home_card`): the core refuses it if
/// any of it changed since.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CardSignIn {
    pub cluster: String,
    /// The API server the card names.
    pub server: String,
    /// [`KeepWatchView::definition`].
    pub definition: Option<u64>,
    /// [`KeepWatchView::shown_sign_in`].
    pub shown: Option<u64>,
}

/// The Sign in of a watched cluster's card, if it offers one: only for a cluster that needs a
/// sign-in, is not `selected`, and signs in through an exec plugin. One signing in through an
/// auth provider keeps "select the cluster to sign in", and the selected cluster signs in as
/// the selected cluster.
pub(super) fn card_sign_in(
    cluster: &str,
    view: Option<&KeepWatchView>,
    server: Option<&str>,
    selected: bool,
) -> Option<CardSignIn> {
    let view = view?;
    let waiting = matches!(
        view.status,
        KeepWatching::SignInNeeded | KeepWatching::NeedsConfirming
    );
    let plugin = matches!(view.sign_in, BackgroundSignIn::Off | BackgroundSignIn::On);
    let server = server.filter(|server| !server.is_empty())?;
    let drawn = view.shown_sign_in.is_some() && view.definition.is_some();
    (waiting && plugin && drawn && !selected).then(|| CardSignIn {
        cluster: cluster.to_owned(),
        server: server.to_owned(),
        definition: view.definition,
        shown: view.shown_sign_in,
    })
}

/// Runs a card's Sign in.
pub(super) type OnSignIn = Rc<dyn Fn(CardSignIn, &mut App)>;

/// What a row's Keep Watching button does, as drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct KeepButton {
    pub enabled: bool,
    pub tip: String,
}

/// The Keep Watching button of a cluster not kept watching. Enabled only where the row menu's
/// toggle would turn watching on: a sign-in command still needs its own consent after that.
pub(super) fn keep_button(view: Option<&KeepWatchView>, settings_ok: bool) -> KeepButton {
    let disabled = |tip: &str| KeepButton {
        enabled: false,
        tip: tip.to_owned(),
    };
    if !settings_ok {
        return disabled("settings.toml is invalid.");
    }
    let Some(view) = view else {
        return disabled("Flint is still reading this cluster's sign-in.");
    };
    if let Some(why) = &view.refusal {
        return disabled(why);
    }
    if view.status != KeepWatching::Off || view.definition.is_none() {
        return disabled("This cluster can't be kept watching now.");
    }
    KeepButton {
        enabled: true,
        tip: keep_watching::toggle_tip(KeepWatching::Off).to_owned(),
    }
}

/// A watched cluster as the workspace found it, for [`build`].
pub(super) struct HomeRead {
    pub read: ClusterRead,
    pub status: KeepWatching,
    /// The text of its newest alert.
    pub latest_alert: Option<String>,
}

/// A watched cluster's card, with what the home screen says beside it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct HomeCard {
    pub card: ClusterCard,
    pub status: KeepWatching,
    pub version: Option<String>,
    /// Nodes and pods its feed counted.
    pub inventory: Option<(usize, usize)>,
    pub latest_alert: Option<String>,
}

impl HomeCard {
    pub fn cluster(&self) -> &str {
        &self.card.cluster
    }

    /// The line under the card: nodes and pods, version.
    fn facts(&self) -> Option<String> {
        let mut facts = Vec::new();
        if let Some((nodes, pods)) = self.inventory {
            facts.push(format!(
                "{nodes} {}, {pods} {}",
                if nodes == 1 { "node" } else { "nodes" },
                if pods == 1 { "pod" } else { "pods" }
            ));
        }
        if let Some(version) = &self.version {
            facts.push(format!("Kubernetes {version}"));
        }
        (!facts.is_empty()).then(|| facts.join(" · "))
    }
}

/// The cards of `reads`, in their order. Runs off the UI thread; the objects behind the counts
/// are dropped here.
pub(super) fn build(reads: Vec<HomeRead>, now: Timestamp) -> Vec<HomeCard> {
    reads
        .into_iter()
        .map(|home| {
            let telemetry = home
                .read
                .telemetry
                .as_ref()
                .filter(|_| home.read.blocked.is_none());
            let version = telemetry.and_then(|t| match &t.snapshot.version {
                VersionCheck::Known(version) => Some(version.clone()),
                VersionCheck::Waiting | VersionCheck::Unknown => None,
            });
            let inventory = telemetry.and_then(|t| {
                let inventory = &t.snapshot.inventory;
                Some((inventory.nodes?, inventory.pods?))
            });
            let card = cluster_card::card(home.read, now);
            let live = card.state.counts();
            HomeCard {
                card,
                status: home.status,
                version: version.filter(|_| live),
                inventory: inventory.filter(|_| live),
                latest_alert: home.latest_alert,
            }
        })
        .collect()
}

/// An unwatched cluster's row, as drawn.
pub(super) struct HomeRow {
    pub cluster: String,
    pub label: String,
    pub tag: Option<ClusterTag>,
    pub keep: KeepButton,
}

/// A watched cluster on the home screen.
pub(super) struct WatchedCard<'a> {
    pub cluster: String,
    /// Its name as shown.
    pub shown: String,
    pub status: KeepWatching,
    /// Its card once read; until then its name and state stand in.
    pub card: Option<&'a HomeCard>,
    pub sign_in: Option<CardSignIn>,
}

/// What the home screen draws.
pub(super) struct HomeView<'a> {
    /// One per watched cluster, in sidebar order.
    pub watched: Vec<WatchedCard<'a>>,
    pub unwatched: Vec<HomeRow>,
    /// Why the last Sign in was refused.
    pub error: Option<String>,
}

fn section_title(text: &'static str, theme: &Theme) -> impl IntoElement {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.muted_foreground)
        .child(text)
}

fn open_button(id: String, cluster: &str, shown: &str) -> Button {
    Button::new(SharedString::from(id))
        .xsmall()
        .outline()
        .label("Open")
        .tooltip(format!(
            "Select {shown}, as a click on it in the sidebar does."
        ))
        .on_click(registry::on_click(SelectCluster {
            cluster: cluster.to_owned(),
        }))
}

/// A card's Sign in. Deliberately not a registered action: no command palette entry, key
/// binding or automation `dispatch` reaches it, only a click (or Enter or Space on it).
fn sign_in_button(target: &CardSignIn, shown: &str, on_sign_in: &OnSignIn) -> Button {
    let (target, on_sign_in) = (target.clone(), Rc::clone(on_sign_in));
    Button::new(SharedString::from(format!("home-sign-in-{}", target.cluster)))
        .xsmall()
        .primary()
        .label("Sign in")
        .tooltip(format!(
            "Run {shown}'s sign-in command once, now. Nothing is saved, and nothing runs it again."
        ))
        // not-an-action: the click is the consent for one sign-in run.
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            on_sign_in(target.clone(), cx);
        })
}

fn render_watched(
    watched: &WatchedCard<'_>,
    actions: &CardActions,
    on_sign_in: &OnSignIn,
    focus: &FocusHandle,
    theme: &Theme,
) -> impl IntoElement {
    let WatchedCard {
        cluster,
        shown,
        status,
        card,
        sign_in,
    } = watched;
    let status_text = status_word(*status);
    let status_color = match status {
        KeepWatching::Watching => theme.success,
        _ => theme.warning,
    };
    let body = match card {
        Some(card) => render_card(&card.card, actions, focus, theme).into_any_element(),
        // Not read yet: the name and state, in a card's place.
        None => v_flex()
            .w(px(CARD_WIDTH))
            .p_3()
            .gap_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(shown.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(super::dashboard::METRICS_WAITING),
            )
            .into_any_element(),
    };
    let facts = card.and_then(|card| card.facts());
    let alert = card.and_then(|card| card.latest_alert.clone());
    v_flex()
        .id(SharedString::from(format!("home-card-{cluster}")))
        .aria_label(match card {
            Some(card) => format!("{} ({status_text})", card.card.description()),
            None => format!("{shown} ({status_text})"),
        })
        .w(px(CARD_WIDTH))
        .flex_none()
        .gap_1()
        .child(body)
        .child(
            h_flex()
                .px_1()
                .gap_2()
                .justify_between()
                .text_xs()
                .child(
                    h_flex()
                        .gap_1()
                        .min_w_0()
                        .child(super::theme::status_dot(
                            status_color,
                            *status == KeepWatching::Watching,
                        ))
                        .child(
                            div()
                                .id(SharedString::from(format!("home-status-{cluster}")))
                                .aria_label(status_text)
                                .text_color(theme.muted_foreground)
                                .child(status_text)
                                .test_support(),
                        ),
                )
                .child(
                    h_flex()
                        .flex_none()
                        .gap_1()
                        .when_some(sign_in.as_ref(), |el, target| {
                            el.child(sign_in_button(target, shown, on_sign_in))
                        })
                        .child(open_button(format!("home-open-{cluster}"), cluster, shown)),
                ),
        )
        .when_some(facts, |el, facts| {
            el.child(
                div()
                    .id(SharedString::from(format!("home-facts-{cluster}")))
                    .aria_label(facts.clone())
                    .px_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(facts)
                    .test_support(),
            )
        })
        .when_some(alert, |el, alert| {
            let text = format!("Latest alert: {alert}");
            el.child(
                div()
                    .id(SharedString::from(format!("home-alert-{cluster}")))
                    .aria_label(text.clone())
                    .px_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(text)
                    .test_support(),
            )
        })
        .test_support()
}

fn render_row(row: &HomeRow, theme: &Theme) -> impl IntoElement {
    let cluster = &row.cluster;
    h_flex()
        .id(SharedString::from(format!("home-row-{cluster}")))
        .aria_label(format!("{}: not watched", row.label))
        .w_full()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_sm()
        .hover(|el| el.bg(theme.table_hover))
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .text_sm()
                .when_some(row.tag, |el, tag| el.child(tag_dot(cluster, tag, theme)))
                .child(div().truncate().child(row.label.clone())),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("not watched"),
        )
        .child(open_button(
            format!("home-open-{cluster}"),
            cluster,
            &row.label,
        ))
        .child(
            Button::new(SharedString::from(format!("home-keep-{cluster}")))
                .xsmall()
                .ghost()
                .label(keep_watching::toggle_label(
                    KeepWatching::Off,
                    flint_core::keep_watching::BackgroundSignIn::NotNeeded,
                ))
                .disabled(!row.keep.enabled)
                .tooltip(row.keep.tip.clone())
                .on_click(registry::on_click(ToggleKeepWatching {
                    cluster: cluster.clone(),
                })),
        )
        .test_support()
}

/// Draws the home screen. A click on a card performs what `actions` names, from `focus`.
pub(super) fn render(
    view: &HomeView<'_>,
    actions: &CardActions,
    on_sign_in: &OnSignIn,
    focus: &FocusHandle,
    scope: &FocusHandle,
    theme: &Theme,
) -> AnyElement {
    let mut content =
        v_flex()
            .gap_4()
            .child(v_flex().gap_1().child(div().text_lg().child("Home")).child(
                div().text_sm().text_color(theme.muted_foreground).child(
                    "Clusters kept watching show their health here. Opening this \
                     screen connects nothing.",
                ),
            ))
            .when_some(view.error.clone(), |el, error| {
                el.child(
                    div()
                        .id("home-error")
                        .aria_label(error.clone())
                        .text_sm()
                        .text_color(theme.danger)
                        .child(error)
                        .test_support(),
                )
            });
    if view.watched.is_empty() {
        content = content.child(
            div()
                .id("home-no-watched")
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(
                    "No cluster is kept watching. Keep one watching to see its health here \
                     without selecting it.",
                )
                .test_support(),
        );
    } else {
        content = content.child(
            v_flex()
                .gap_2()
                .child(section_title("WATCHED", theme))
                .child(
                    h_flex()
                        .id("home-cards")
                        .flex_wrap()
                        .items_start()
                        .gap_3()
                        .children(
                            view.watched
                                .iter()
                                .map(|watched| {
                                    render_watched(watched, actions, on_sign_in, focus, theme)
                                }),
                        )
                        .test_support(),
                ),
        );
    }
    if !view.unwatched.is_empty() {
        content = content.child(
            v_flex()
                .gap_1()
                .max_w(px(720.))
                .child(section_title("NOT WATCHED", theme))
                .child(
                    v_flex()
                        .id("home-unwatched")
                        .gap_px()
                        .children(view.unwatched.iter().map(|row| render_row(row, theme)))
                        .test_support(),
                ),
        );
    }
    link_focus(v_flex().id("home"), scope)
        .track_focus(scope)
        .flex_1()
        .min_w_0()
        .h_full()
        .overflow_y_scroll()
        .p_6()
        .child(content)
        .test_support()
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use flint_core::keep_watching::{BackgroundSignIn, KeepWatchView, KeepWatching};
    use flint_core::thresholds::Thresholds;
    use jiff::Timestamp;

    use super::super::dashboard::cluster_card::{CardState, ClusterRead};
    use super::{
        CardSignIn, HomeCard, HomeRead, blocked_reason, build, card_sign_in, keep_button, plan,
        status_word,
    };

    fn view(status: KeepWatching, definition: Option<u64>) -> KeepWatchView {
        KeepWatchView {
            status,
            sign_in: BackgroundSignIn::NotNeeded,
            command: None,
            shown_sign_in: None,
            definition,
            refusal: None,
        }
    }

    #[test]
    fn watched_clusters_get_cards_and_the_rest_rows_in_sidebar_order() {
        let plan = plan([
            ("pinned", KeepWatching::Off),
            ("prod", KeepWatching::Watching),
            ("dev", KeepWatching::Off),
            ("eks", KeepWatching::NeedsConfirming),
            ("gke", KeepWatching::SignInNeeded),
            ("moved", KeepWatching::Paused),
        ]);
        assert_eq!(
            plan.watched,
            [
                ("prod".to_owned(), KeepWatching::Watching),
                ("eks".to_owned(), KeepWatching::NeedsConfirming),
                ("gke".to_owned(), KeepWatching::SignInNeeded),
                ("moved".to_owned(), KeepWatching::Paused),
            ]
        );
        assert_eq!(plan.unwatched, ["pinned", "dev"]);
    }

    #[test]
    fn with_nothing_watched_every_cluster_is_a_row() {
        let plan = plan([("a", KeepWatching::Off), ("b", KeepWatching::Off)]);
        assert!(plan.watched.is_empty());
        assert_eq!(plan.unwatched, ["a", "b"]);
    }

    #[test]
    fn the_card_says_watching_or_sign_in_needed() {
        assert_eq!(status_word(KeepWatching::Watching), "watching");
        assert_eq!(status_word(KeepWatching::SignInNeeded), "sign-in needed");
        assert_eq!(status_word(KeepWatching::NeedsConfirming), "sign-in needed");
        assert_eq!(status_word(KeepWatching::Paused), "paused");
        assert_eq!(blocked_reason(KeepWatching::Watching), None);
        for status in [
            KeepWatching::SignInNeeded,
            KeepWatching::NeedsConfirming,
            KeepWatching::Paused,
        ] {
            assert!(blocked_reason(status).is_some(), "{status:?}");
        }
    }

    #[test]
    fn keep_watching_is_offered_only_where_the_toggle_would_turn_it_on() {
        let on = keep_button(Some(&view(KeepWatching::Off, Some(7))), true);
        assert!(on.enabled);
        // Not reported yet, no definition, already on, refused, or settings invalid.
        assert!(!keep_button(None, true).enabled);
        assert!(!keep_button(Some(&view(KeepWatching::Off, None)), true).enabled);
        assert!(!keep_button(Some(&view(KeepWatching::Watching, Some(7))), true).enabled);
        let mut refused = view(KeepWatching::Off, Some(7));
        refused.refusal = Some("Five clusters are kept watching already.".to_owned());
        let button = keep_button(Some(&refused), true);
        assert!(!button.enabled);
        assert_eq!(button.tip, "Five clusters are kept watching already.");
        assert!(!keep_button(Some(&view(KeepWatching::Off, Some(7))), false).enabled);
    }

    fn read(cluster: &str, blocked: Option<String>) -> ClusterRead {
        ClusterRead {
            cluster: cluster.to_owned(),
            shown: cluster.to_owned(),
            tag: None,
            blocked,
            telemetry: None,
            server: "https://127.0.0.1:6443".to_owned(),
            thresholds: Thresholds::default(),
        }
    }

    #[test]
    fn a_card_reads_only_its_feed_and_shows_nothing_while_blocked() {
        let cards = build(
            vec![
                HomeRead {
                    read: read("prod", None),
                    status: KeepWatching::Watching,
                    latest_alert: Some("Pod web failed".to_owned()),
                },
                HomeRead {
                    read: read("eks", blocked_reason(KeepWatching::NeedsConfirming)),
                    status: KeepWatching::NeedsConfirming,
                    latest_alert: None,
                },
            ],
            Timestamp::now(),
        );
        let clusters: Vec<&str> = cards.iter().map(HomeCard::cluster).collect();
        assert_eq!(clusters, ["prod", "eks"], "in the order given");
        // No feed yet: waiting, with no values.
        assert_eq!(cards[0].card.state, CardState::Waiting);
        assert_eq!(cards[0].version, None);
        assert_eq!(cards[0].latest_alert.as_deref(), Some("Pod web failed"));
        assert!(matches!(cards[1].card.state, CardState::Blocked(_)));
        assert!(cards[1].card.feed.is_none());
        assert!(
            flint_core::dashboard::ProblemCategory::ALL
                .iter()
                .all(|category| cards[1].card.counts.get(*category).is_none())
        );
    }

    #[test]
    fn a_card_offers_sign_in_for_exactly_what_it_shows() {
        let mut plugin = view(KeepWatching::NeedsConfirming, Some(7));
        plugin.sign_in = BackgroundSignIn::Off;
        plugin.shown_sign_in = Some(9);
        let server = Some("https://127.0.0.1:6443");
        let target = card_sign_in("eks", Some(&plugin), server, false).expect("offered");
        assert_eq!(
            target,
            CardSignIn {
                cluster: "eks".to_owned(),
                server: "https://127.0.0.1:6443".to_owned(),
                definition: Some(7),
                shown: Some(9),
            }
        );
        // Selected, no server, or not drawn with a plugin definition: nothing to sign in.
        assert!(card_sign_in("eks", Some(&plugin), server, true).is_none());
        assert!(card_sign_in("eks", Some(&plugin), None, false).is_none());
        assert!(card_sign_in("eks", Some(&plugin), Some(""), false).is_none());
        assert!(card_sign_in("eks", None, server, false).is_none());
        let mut unnamed = plugin.clone();
        unnamed.shown_sign_in = None;
        assert!(card_sign_in("eks", Some(&unnamed), server, false).is_none());
        // An auth provider keeps "select the cluster"; a watching card needs nothing.
        let mut provider = plugin.clone();
        provider.sign_in = BackgroundSignIn::Unavailable;
        assert!(card_sign_in("eks", Some(&provider), server, false).is_none());
        let mut watching = plugin;
        watching.status = KeepWatching::Watching;
        assert!(card_sign_in("eks", Some(&watching), server, false).is_none());
    }

    #[test]
    fn the_facts_line_names_nodes_pods_and_version() {
        let mut card = build(
            vec![HomeRead {
                read: read("prod", None),
                status: KeepWatching::Watching,
                latest_alert: None,
            }],
            Timestamp::now(),
        )
        .remove(0);
        assert_eq!(card.facts(), None);
        card.inventory = Some((1, 12));
        card.version = Some("v1.31.0".to_owned());
        assert_eq!(
            card.facts().as_deref(),
            Some("1 node, 12 pods · Kubernetes v1.31.0")
        );
    }
}
