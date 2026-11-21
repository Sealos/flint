//! Custom cluster names (#259): the Rename Cluster dialog and what it accepts. A name is only
//! what Flint shows: it lives in `settings.toml`, kubeconfig is never written (UP-90), and MCP,
//! `FLINT_CLUSTER`, exposure and trust keep the context name.

use std::collections::BTreeMap;

use flint_core::display::{ClusterNames, MAX_CUSTOM_NAME_CHARS, clean_custom_name, short_name};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::clusters::ClustersModel;
use super::confirm::{self, DialogText};

/// Why `typed` cannot be `context`'s name, among `contexts` and the custom `names` they have.
/// Nothing typed is fine: it removes the name.
fn problem(
    names: &BTreeMap<String, String>,
    contexts: &[&str],
    context: &str,
    typed: &str,
) -> Option<String> {
    if typed.trim().is_empty() {
        return None;
    }
    let Some(name) = clean_custom_name(typed) else {
        return Some(format!(
            "A cluster name has at most {MAX_CUSTOM_NAME_CHARS} characters, none of them hidden."
        ));
    };
    let mut with_it = names.clone();
    with_it.insert(context.to_owned(), name);
    ClusterNames::new(contexts.iter().copied(), &with_it)
        .custom(context)
        .is_none()
        .then(|| "Another cluster already goes by this name.".to_owned())
}

/// The dialog's text: the real context and server stay on screen while naming it.
fn dialog(context: &str, server: Option<&str>) -> DialogText {
    DialogText {
        title: format!("Rename {}", short_name(context)),
        text: format!(
            "Shown instead of the context name, in Flint only. kubeconfig is not changed, and AI \
             agents and shells keep the context name. Leave it empty to use the context name \
             again.\n\nContext: {context}\nServer: {}",
            server.unwrap_or("no server")
        ),
        ok: "Rename".into(),
        cancel: "Cancel".into(),
        id: "rename-cluster-description",
        default_cancel: false,
    }
}

/// The name field under the dialog's text.
struct NameField {
    input: Entity<InputState>,
    error: Option<String>,
}

impl Render for NameField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let danger = cx.theme().danger;
        v_flex()
            .gap_1()
            .child(Input::new(&self.input).id("rename-cluster-input").small())
            .when_some(self.error.clone(), |el, error| {
                el.child(div().text_xs().text_color(danger).child(error))
            })
    }
}

/// Opens the Rename Cluster dialog for `context`.
pub fn open(
    window: &mut Window,
    cx: &mut App,
    model: Entity<ClustersModel>,
    context: String,
    server: Option<String>,
) {
    let current = model
        .read(cx)
        .settings
        .cluster_names
        .get(&context)
        .cloned()
        .unwrap_or_default();
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(short_name(&context))
            .default_value(current)
    });
    let field = cx.new(|_| NameField {
        input: input.clone(),
        error: None,
    });
    let content = field.clone().into();
    let text = dialog(&context, server.as_deref());
    confirm::open_with(window, cx, text, Some(content), move |_, cx| {
        let typed = field.read(cx).input.read(cx).value().to_string();
        let refused = {
            let m = model.read(cx);
            let contexts: Vec<&str> = m.clusters.iter().map(|c| c.info.name.as_str()).collect();
            problem(&m.settings.cluster_names, &contexts, &context, &typed)
        };
        if let Some(error) = refused {
            field.update(cx, |f, cx| {
                f.error = Some(error);
                cx.notify();
            });
            return false;
        }
        let name = clean_custom_name(&typed);
        model.update(cx, |m, _| m.set_cluster_name(context.clone(), name));
        true
    });
    input.update(cx, |input, cx| input.focus(window, cx));
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{dialog, problem};

    // covers UP-01: what the dialog refuses, before anything is written.
    #[test]
    fn a_name_is_refused_when_it_could_pass_for_another_cluster() {
        let arn = "arn:aws:eks:eu-west-1:123456789012:cluster/example-prod";
        let contexts = ["dev", "staging", arn];
        let names = BTreeMap::from([("staging".to_owned(), "blue".to_owned())]);
        assert_eq!(problem(&names, &contexts, "dev", "green"), None);
        assert_eq!(problem(&names, &contexts, "dev", "   "), None, "removes it");
        assert_eq!(
            problem(&names, &contexts, "staging", "BLUE"),
            None,
            "its own"
        );
        for taken in ["blue", "Blue", "staging", "example-prod", arn] {
            assert!(
                problem(&names, &contexts, "dev", taken).is_some(),
                "{taken}"
            );
        }
        assert!(problem(&names, &contexts, "dev", "a\u{202e}b").is_some());
        assert!(problem(&names, &contexts, "dev", &"x".repeat(33)).is_some());
    }

    // covers UP-55: renaming keeps the real cluster on screen.
    #[test]
    fn the_dialog_names_the_real_context_and_server() {
        let arn = "arn:aws:eks:eu-west-1:123456789012:cluster/example-prod";
        let text = dialog(arn, Some("https://example.invalid")).text;
        assert!(text.contains(&format!("Context: {arn}")), "{text}");
        assert!(text.contains("Server: https://example.invalid"), "{text}");
        assert!(text.contains("kubeconfig is not changed"), "{text}");
    }
}
