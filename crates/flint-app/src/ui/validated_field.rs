//! A one-line field under a dialog's text that refuses what it cannot take: the replica count of
//! Scale, the name of Rename Cluster and Rename Cluster Set. The dialog stays open with the reason
//! under the field until the value is one it takes.

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The field, and why its last value was refused.
pub(in crate::ui) struct ValidatedField {
    input: Entity<InputState>,
    error: Option<String>,
    /// The automation ids of the input and of the reason under it.
    input_id: &'static str,
    error_id: &'static str,
}

impl ValidatedField {
    /// A field on `input`, its input and reason drawn as `input_id` and `error_id`.
    pub(in crate::ui) fn new(
        input: Entity<InputState>,
        input_id: &'static str,
        error_id: &'static str,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_| Self {
            input,
            error: None,
            input_id,
            error_id,
        })
    }

    /// What `check` makes of the typed value. When it refuses it, the field shows why and this is
    /// `None`: the dialog stays open.
    pub(in crate::ui) fn checked<T>(
        field: &Entity<Self>,
        cx: &mut App,
        check: impl FnOnce(&str, &App) -> Result<T, String>,
    ) -> Option<T> {
        let typed = field.read(cx).input.read(cx).value().to_string();
        match check(&typed, cx) {
            Ok(value) => Some(value),
            Err(error) => {
                field.update(cx, |field, cx| {
                    field.error = Some(error);
                    cx.notify();
                });
                None
            }
        }
    }

    /// Puts the keyboard in the field.
    pub(in crate::ui) fn focus(field: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let input = field.read(cx).input.clone();
        input.update(cx, |input, cx| input.focus(window, cx));
    }
}

impl Render for ValidatedField {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let danger = cx.theme().danger;
        let error_id = self.error_id;
        v_flex()
            .gap_1()
            .child(Input::new(&self.input).id(self.input_id).small())
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .id(error_id)
                        .aria_label(error.clone())
                        .text_xs()
                        .text_color(danger)
                        .child(error)
                        .test_support(),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::input::InputState;
    use gpui_kit::{AppContext as _, TestAppContext};

    use super::ValidatedField;
    use crate::ui::confirm::{self, DialogText};
    use crate::ui::support_tests::open_workspace;

    // A refused value keeps the dialog open with the reason under the field; one it takes closes
    // it with the checked value.
    #[gpui_kit::test]
    async fn a_refused_value_says_why_and_keeps_the_dialog(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let t = open_workspace(cx, dir.path()).await;
        let taken = Rc::new(Cell::new(None));
        let on_ok = taken.clone();
        cx.update_window(t.window, move |_, window, cx| {
            let input = cx.new(|cx| InputState::new(window, cx));
            let field = ValidatedField::new(input, "test-input", "test-error", cx);
            let text = DialogText {
                title: "Test".into(),
                text: "A number.".into(),
                ok: "OK".into(),
                cancel: "Cancel".into(),
                id: "test-description",
                default_cancel: false,
            };
            let checked = field.clone();
            confirm::open_with(
                window,
                cx,
                text,
                Some(field.clone().into()),
                move |_, cx| {
                    let value = ValidatedField::checked(&checked, cx, |typed, _| {
                        typed
                            .parse::<u8>()
                            .map_err(|_| format!("{typed} is no number"))
                    });
                    on_ok.set(value);
                    value.is_some()
                },
            );
            ValidatedField::focus(&field, window, cx);
        })
        .unwrap();
        let open = |cx: &mut TestAppContext| {
            cx.update_window(t.window, |_, window, cx| window.has_active_dialog(cx))
                .unwrap()
        };
        t.wait_for_element(cx, "test-input").await;
        t.type_text(cx, "x");
        t.click(cx, "ok");
        cx.run_until_parked();
        assert!(open(cx), "a refused value keeps the dialog");
        assert_eq!(t.label(cx, "test-error").as_deref(), Some("x is no number"));
        t.press(cx, "backspace");
        t.type_text(cx, "7");
        t.click(cx, "ok");
        cx.run_until_parked();
        assert_eq!(taken.get(), Some(7));
        assert!(!open(cx), "a value it takes closes it");
    }
}
