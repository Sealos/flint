//! The control for how long a pod may stay Pending before it counts as a problem: a submenu of
//! the header's Alerts menu, for all clusters and for the active one. The value is the typed
//! `pending_pod_seconds` threshold, and the core applies a change to running feeds.

use flint_core::settings::{ClusterPin, Settings};
use flint_core::thresholds::ThresholdMetric;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::*;
use serde::Deserialize;

use super::clusters::ClustersModel;
use super::registry;

const METRIC: ThresholdMetric = ThresholdMetric::PendingPodSeconds;

/// The values the menu offers. Any other in-range value can be set in `settings.toml`.
const PRESETS: [u32; 5] = [30, 60, 120, 300, 900];

/// Sets how long a pod may stay Pending, for one cluster or (with no `cluster`) all of them.
/// With no `seconds`, the value is cleared and follows the next level down.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = workspace, no_json)]
#[serde(deny_unknown_fields)]
pub struct SetPendingThreshold {
    #[serde(default)]
    pub cluster: Option<String>,
    #[serde(default)]
    pub seconds: Option<u32>,
}

/// `90 seconds`, `1 minute`, `2 hours`.
fn duration_label(seconds: u32) -> String {
    let (value, unit) = match seconds {
        s if s % 3600 == 0 => (s / 3600, "hour"),
        s if s % 60 == 0 => (s / 60, "minute"),
        s => (s, "second"),
    };
    let plural = if value == 1 { "" } else { "s" };
    format!("{value} {unit}{plural}")
}

/// What the menu shows, read before it is built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingChoices {
    global: u32,
    /// The active cluster, if it has a server to bind an override to.
    cluster: Option<ClusterChoice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClusterChoice {
    id: String,
    own: Option<u32>,
}

impl PendingChoices {
    fn of_settings(settings: &Settings, active: Option<(&str, &str)>) -> Self {
        let global = settings
            .thresholds
            .global
            .get(METRIC)
            .unwrap_or(METRIC.default_value());
        let cluster = active.map(|(id, server)| ClusterChoice {
            id: id.to_owned(),
            own: settings
                .thresholds
                .clusters
                .iter()
                .find(|entry| entry.cluster.matches(id, server))
                .and_then(|entry| entry.overrides.get(METRIC)),
        });
        Self { global, cluster }
    }

    pub(super) fn of(model: &ClustersModel) -> Self {
        let active = model.active_cluster().and_then(|active| {
            let server = active.info.server.as_deref()?;
            Some((active.info.name.as_str(), server))
        });
        Self::of_settings(&model.settings, active)
    }

    /// The value in force where the menu was opened.
    fn in_force(&self) -> u32 {
        self.cluster
            .as_ref()
            .and_then(|cluster| cluster.own)
            .unwrap_or(self.global)
    }

    fn title(&self) -> String {
        format!("Pending Pods: after {}", duration_label(self.in_force()))
    }
}

/// Adds the submenu to the Alerts menu.
pub(super) fn submenu(
    menu: PopupMenu,
    choices: PendingChoices,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let title = choices.title();
    menu.submenu(title, window, cx, move |sub, _, _| {
        let mut sub = sub.label("All clusters");
        for seconds in PRESETS {
            sub = sub.item(
                PopupMenuItem::new(duration_label(seconds))
                    .checked(choices.global == seconds)
                    .on_click(registry::on_click(SetPendingThreshold {
                        cluster: None,
                        seconds: Some(seconds),
                    })),
            );
        }
        let Some(cluster) = &choices.cluster else {
            return sub;
        };
        sub = sub.separator().label("This cluster").item(
            PopupMenuItem::new(format!(
                "Use the default ({})",
                duration_label(choices.global)
            ))
            .checked(cluster.own.is_none())
            .on_click(registry::on_click(SetPendingThreshold {
                cluster: Some(cluster.id.clone()),
                seconds: None,
            })),
        );
        for seconds in PRESETS {
            sub = sub.item(
                PopupMenuItem::new(duration_label(seconds))
                    .checked(cluster.own == Some(seconds))
                    .on_click(registry::on_click(SetPendingThreshold {
                        cluster: Some(cluster.id.clone()),
                        seconds: Some(seconds),
                    })),
            );
        }
        sub
    })
}

/// Handles the action for the whole app.
pub(super) fn install(model: &Entity<ClustersModel>, cx: &mut App) {
    let model = model.clone();
    cx.on_action(move |action: &SetPendingThreshold, cx| {
        model.update(cx, |model, _| {
            // An override is bound to the server the context points to now; without one there
            // is nothing to bind it to.
            let pin = match &action.cluster {
                None => None,
                Some(id) => {
                    let server = model
                        .clusters
                        .iter()
                        .find(|c| c.info.name == *id)
                        .and_then(|c| c.info.server.clone());
                    let Some(server) = server else {
                        return;
                    };
                    Some(ClusterPin::new(id.clone(), server))
                }
            };
            model.set_threshold(pin, METRIC, action.seconds);
        });
    });
}

#[cfg(test)]
mod tests {
    use flint_core::settings::{ClusterPin, Settings};

    use super::{METRIC, PRESETS, PendingChoices, duration_label};

    const SERVER: &str = "https://a.example:6443";

    #[test]
    fn every_preset_is_a_value_the_setting_accepts() {
        assert!(PRESETS.iter().all(|value| METRIC.range().contains(value)));
        assert!(PRESETS.contains(&METRIC.default_value()));
    }

    #[test]
    fn durations_read_in_the_largest_whole_unit() {
        assert_eq!(duration_label(45), "45 seconds");
        assert_eq!(duration_label(60), "1 minute");
        assert_eq!(duration_label(90), "90 seconds");
        assert_eq!(duration_label(900), "15 minutes");
        assert_eq!(duration_label(3600), "1 hour");
    }

    #[test]
    fn the_title_shows_the_value_in_force_for_the_active_cluster() {
        let dir = tempfile::tempdir().unwrap();
        let pin = ClusterPin::new("prod", SERVER);
        flint_core::settings::set_threshold(dir.path(), None, METRIC, Some(120)).unwrap();
        let settings =
            flint_core::settings::set_threshold(dir.path(), Some(&pin), METRIC, Some(300)).unwrap();

        let here = PendingChoices::of_settings(&settings, Some(("prod", SERVER)));
        assert_eq!(here.title(), "Pending Pods: after 5 minutes");
        let elsewhere = PendingChoices::of_settings(&settings, Some(("dev", SERVER)));
        assert_eq!(elsewhere.title(), "Pending Pods: after 2 minutes");
        let none = PendingChoices::of_settings(&Settings::default(), None);
        assert_eq!(none.title(), "Pending Pods: after 5 minutes");
    }
}
