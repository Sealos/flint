//! The command palette (Cmd-Shift-P, UP-30): a fuzzy search over every registered action that
//! is available where focus was when it opened, with each action's category and shortcut.
//! Enter runs the selected one from that focus, exactly as its key or menu item would
//! ([`registry::dispatch_in`]); an action that takes arguments asks for them first, one at a
//! time, from a list (a cluster, a namespace, a kind, a column, a container) or as typed text (a
//! label selector). Esc, or Backspace in the empty field, goes back a step. Recently run actions
//! come first.
//!
//! Matching runs when the query changes ([`palette_model`]), never while drawing, and the rows
//! are a virtual list, so it stays quick with every action listed and with long argument lists
//! (thousands of namespaces).
//!
//! The same view is the resource switcher (Cmd-P, UP-31) in [`Mode::Resources`]: a fuzzy
//! search over the active cluster's kinds and the objects its watches hold in memory
//! ([`super::switcher_model`]), never the network. Enter on a kind runs Open Kind…
//! ([`OpenView`]); on an object, [`OpenObject`], which opens its list with it selected and
//! its details shown. The objects arrive a moment after it opens (the workspace reads the
//! stores off the UI thread); until then it lists the kinds.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::input::{self, Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::{Map, Value};

use super::palette_model::{self, Candidate, Recents};
use super::registry::{
    self, ActionError, ActionSpec, Arg, ArgKind, Bind, OnView, OpenObject, OpenView, PaletteBack,
    PaletteConfirm, PaletteDismiss, PaletteDown, PalettePick, PaletteUp, PasteText, Scope,
    ToggleCommandPalette, ToggleResourceSwitcher, Visibility,
};
use super::switcher_model::{Index, Search};

/// A row's height, in points: dense, as the lists are.
const ROW_HEIGHT: f32 = 26.;
/// Rows shown before the list scrolls.
const ROWS_SHOWN: usize = 12;
/// The palette's width, in points.
const WIDTH: f32 = 560.;
/// The shortcut column's width, the same on every action row so the categories line up.
const SHORTCUT_WIDTH: f32 = 64.;
/// The field's placeholder while it lists actions.
const SEARCH: &str = "Search actions…";
/// The field's placeholder in the resource switcher.
const SEARCH_RESOURCES: &str = "Go to a kind or object…";
/// The cluster column's width in the resource switcher.
const CLUSTER_WIDTH: f32 = 120.;

/// What the palette searches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The actions available where focus was (Cmd-Shift-P).
    Actions,
    /// The active cluster's kinds and the objects in memory (Cmd-P).
    Resources,
}

/// One value an argument can take, as the palette lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// What the action gets.
    pub value: String,
    /// What the row says.
    pub label: String,
    /// Said beside it, muted (a context's full name, a kind's id).
    pub detail: Option<String>,
}

impl Choice {
    /// A choice whose label is its value.
    pub fn plain(value: impl Into<String>) -> Self {
        let value = value.into();
        Choice {
            label: value.clone(),
            value,
            detail: None,
        }
    }
}

/// How the palette asks for one argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgInput {
    /// Pick one of these; with `free`, a value typed that is not among them is offered too (a
    /// namespace the user may not list).
    Choices { items: Vec<Choice>, free: bool },
    /// Type it (a label selector, a number).
    Text,
}

/// Where the palette gets an argument's choices: the workspace, which knows the clusters,
/// namespaces, kinds, columns and containers.
pub type ChoiceSource = Rc<dyn Fn(&ActionSpec, &Arg, &App) -> ArgInput>;

/// What the palette tells its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteEvent {
    /// It closed: an action ran, Esc, or focus went elsewhere in the window.
    Dismissed,
}

/// The actions run from the palette, for the whole app session.
#[derive(Default)]
struct RecentActions(Recents);

impl Global for RecentActions {}

/// An action the palette lists.
struct Entry {
    spec: &'static ActionSpec,
    /// Its key binding where the palette came from, as the OS writes it.
    shortcut: Option<SharedString>,
}

/// What the palette is asking for.
enum Step {
    /// Which action.
    Actions,
    /// Which kind or object (the resource switcher).
    Resources,
    /// The argument `spec.args[arg]` of `spec`, with the ones given so far in `args`, and for
    /// each earlier step the argument it asked for and the text shown for its pick.
    Argument {
        spec: &'static ActionSpec,
        arg: usize,
        args: Map<String, Value>,
        picked: Vec<(usize, String)>,
        input: ArgInput,
    },
}

/// A row as drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    /// An action, with the label's matched bytes.
    Action {
        entry: usize,
        ranges: Vec<Range<usize>>,
    },
    /// A listed value of the argument asked for.
    Choice {
        index: usize,
        ranges: Vec<Range<usize>>,
    },
    /// The text typed, as the argument's value.
    Typed(String),
    /// A kind or an object, by its index in the switcher's [`Index`], with the text's matched
    /// bytes.
    Resource {
        item: u32,
        ranges: Vec<Range<usize>>,
    },
}

/// What the resource switcher opens on, from the workspace.
pub struct SwitcherScope {
    /// The active cluster, as the rows name it; `None` with no cluster active.
    pub cluster: Option<SharedString>,
    /// The kinds, at first; the objects come through [`CommandPalette::set_index`].
    pub index: Index,
    /// The objects are being read from the stores.
    pub loading: bool,
    /// The cluster's discovery failed: no kinds are coming.
    pub discovery_failed: bool,
    /// The cluster watches only the kinds open in a view.
    pub on_view: bool,
}

/// What the resource switcher searches.
struct Resources {
    /// The active cluster, as the rows name it; `None` with no cluster active.
    cluster: Option<SharedString>,
    index: Arc<Index>,
    /// The objects are still being read from the stores.
    loading: bool,
    discovery_failed: bool,
    on_view: bool,
    search: Search,
    /// How many items the query matched, listed or not.
    matched: usize,
}

pub struct CommandPalette {
    /// The search field's focus: it leaving (in an active window) closes the palette, and the
    /// palette's controls dispatch from it.
    focus: FocusHandle,
    query: Entity<InputState>,
    /// The query as last matched.
    query_text: String,
    /// The query that picked the action whose arguments are asked for, restored on going back.
    action_query: String,
    /// Focused before the palette opened: actions are listed and run from here, and focus goes
    /// back here when it closes.
    previous: FocusHandle,
    /// The workspace's focus target, used instead of `previous` once that is no longer drawn
    /// (a menu that closed as the palette took focus).
    fallback: FocusHandle,
    /// The key contexts at `previous` when the palette opened (outermost first), for the
    /// shortcuts that fire there.
    contexts: Vec<KeyContext>,
    choices: ChoiceSource,
    /// The actions listed, filled in by [`CommandPalette::load`].
    entries: Vec<Entry>,
    step: Step,
    /// What the query is matched against, for the current step.
    candidates: Vec<Candidate>,
    rows: Vec<Row>,
    /// Counts each change of `rows`, so a click drawn on older rows is told apart.
    rows_version: u64,
    selected: usize,
    /// The selection was moved with the arrows, so a refreshed list keeps it; else the best
    /// row (row 0) is selected again.
    chosen: bool,
    mode: Mode,
    /// The resource switcher's index and search, in [`Mode::Resources`].
    resources: Option<Resources>,
    /// Why the last Enter did nothing (a selector that does not parse, a cluster that went away).
    error: Option<String>,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PaletteEvent> for CommandPalette {}

impl CommandPalette {
    /// A palette over the actions available from `previous` (or, once that is no longer drawn,
    /// from `fallback`), with `contexts` the key contexts at `previous` (read while it still has
    /// focus) and arguments' choices from `choices`. Its list is empty until
    /// [`CommandPalette::load`]: listing reads the guards, which read the workspace, so the
    /// workspace loads it once its own update is done.
    pub fn new(
        previous: FocusHandle,
        fallback: FocusHandle,
        contexts: Vec<KeyContext>,
        choices: ChoiceSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(
            previous,
            fallback,
            contexts,
            choices,
            Mode::Actions,
            window,
            cx,
        )
    }

    /// The resource switcher over `scope`. Enter runs Open Kind… or Open Object… from
    /// `previous` (or `fallback`), as the palette runs any action. The objects come later,
    /// through [`CommandPalette::set_index`].
    pub fn switcher(
        previous: FocusHandle,
        fallback: FocusHandle,
        scope: SwitcherScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let choices: ChoiceSource = Rc::new(|_, _, _| ArgInput::Text);
        let mut palette = Self::build(
            previous,
            fallback,
            Vec::new(),
            choices,
            Mode::Resources,
            window,
            cx,
        );
        palette.resources = Some(Resources {
            cluster: scope.cluster,
            index: Arc::new(scope.index),
            loading: scope.loading,
            discovery_failed: scope.discovery_failed,
            on_view: scope.on_view,
            search: Search::default(),
            matched: 0,
        });
        palette.step = Step::Resources;
        palette.set_query(String::new(), SEARCH_RESOURCES.to_owned(), window, cx);
        palette
    }

    /// The switcher's index once the objects in memory are read: the search starts over on
    /// it, with what was typed.
    pub fn set_index(&mut self, index: Arc<Index>, cx: &mut Context<Self>) {
        // The item the user moved to with the arrows, by what it is: its row may move as
        // objects arrive. A selection nobody moved goes to the best row, which may be new (the
        // rule live refreshes follow).
        let selected = self.rows.get(self.selected).filter(|_| self.chosen);
        let kept = match (selected, &self.resources) {
            (Some(Row::Resource { item, .. }), Some(resources)) => {
                let item = &resources.index.items[*item as usize];
                Some((
                    resources.index.kind_of(item).id.clone(),
                    item.object.clone(),
                ))
            }
            _ => None,
        };
        let Some(resources) = &mut self.resources else {
            return;
        };
        resources.index = index;
        resources.loading = false;
        resources.search = Search::default();
        self.refilter(cx);
        let Some((kind, object)) = kept else {
            return;
        };
        let Some(resources) = &self.resources else {
            return;
        };
        let at = self.rows.iter().position(|row| match row {
            Row::Resource { item, .. } => {
                let item = &resources.index.items[*item as usize];
                resources.index.kind_of(item).id == kind && item.object == object
            }
            _ => false,
        });
        // Gone from the results: the first row, as after typing.
        self.selected = at.unwrap_or(0);
        self.chosen = at.is_some();
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Nearest);
    }

    /// What the palette searches.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    fn build(
        previous: FocusHandle,
        fallback: FocusHandle,
        contexts: Vec<KeyContext>,
        choices: ChoiceSource,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder(SEARCH));
        let on_query = cx.subscribe_in(&query, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                this.refilter(cx);
            }
        });
        // The field's own: the panel tracks no focus of its own, so a click on it (a row, the
        // footer) leaves focus in the field.
        let focus = query.focus_handle(cx);
        // Focus moving elsewhere in the window closes it. The window going to the background
        // (Cmd-Tab, Hide) does not, as with a dialog: focus is back in the field on return.
        let on_leave = cx.on_focus_out(&focus, window, |_, _, window, cx| {
            if window.is_window_active() {
                cx.emit(PaletteEvent::Dismissed);
            }
        });
        query.update(cx, |q, cx| q.focus(window, cx));
        CommandPalette {
            focus,
            query,
            query_text: String::new(),
            action_query: String::new(),
            previous,
            fallback,
            contexts,
            choices,
            entries: Vec::new(),
            step: Step::Actions,
            candidates: Vec::new(),
            rows: Vec::new(),
            rows_version: 0,
            selected: 0,
            chosen: false,
            mode,
            resources: None,
            error: None,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: vec![on_query, on_leave],
        }
    }

    /// Where actions are listed and run from, and where focus goes back: the focus before the
    /// palette opened while it is still drawn in the workspace, else the workspace's target.
    /// GPUI treats a focus that is no longer drawn as the window's root, where none of Flint's
    /// actions are handled and App ones (Quit, Appearance) are lost.
    pub fn origin(&self, window: &Window) -> FocusHandle {
        if window.is_action_available_in(&ToggleCommandPalette, &self.previous) {
            self.previous.clone()
        } else {
            self.fallback.clone()
        }
    }

    /// Lists the actions available from where the palette came from, with their shortcuts
    /// there. Must not run while the workspace is being updated (the guards read it).
    pub fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let origin = self.origin(window);
        // From the workspace's target, the contexts down to the workspace's own.
        let contexts: Vec<KeyContext> = if origin == self.previous {
            self.contexts.clone()
        } else {
            let end = self
                .contexts
                .iter()
                .position(|c| c.contains(registry::WORKSPACE))
                .map_or(0, |i| i + 1);
            self.contexts[..end].to_vec()
        };
        let palette = ToggleCommandPalette::name_for_type();
        // Copy is handled everywhere (gpui-component's root copies selected text), so the Edit
        // actions are listed only where they act: in a text field, where their keys are bound,
        // or in a shell tab, which handles Copy and Paste itself.
        let bound = |name: &str| {
            registry::find(name).is_some_and(|spec| spec.bound_in(&contexts, &origin, window, cx))
        };
        // Cut is bound only in a text field, Paste Terminal Text only in a shell tab.
        let in_field = bound(input::Cut::name_for_type());
        let in_shell = bound(PasteText::name_for_type());
        let shell_edits = [input::Copy::name_for_type(), input::Paste::name_for_type()];
        self.entries = registry::available_in(&origin, window, cx)
            .filter(|spec| {
                spec.visibility == Visibility::Palette
                    && spec.scope != Scope::Palette
                    && spec.name != palette
                    && (spec.scope != Scope::TextField
                        || in_field
                        || (in_shell && shell_edits.contains(&spec.name)))
            })
            .map(|spec| Entry {
                spec,
                shortcut: spec
                    .shortcut_in(&contexts, &origin, window, cx)
                    .map(SharedString::from),
            })
            .collect();
        self.show_actions(String::new(), window, cx);
    }

    /// Lists the actions, with `query` in the field.
    fn show_actions(&mut self, query: String, window: &mut Window, cx: &mut Context<Self>) {
        let recents = cx.try_global::<RecentActions>();
        self.candidates = self
            .entries
            .iter()
            .map(|e| Candidate {
                label: e.spec.label.to_owned(),
                alt: Some(format!("{} {}", e.spec.category.name(), e.spec.label)),
                recent: recents.and_then(|r| r.0.rank(e.spec.name)),
            })
            .collect();
        self.step = Step::Actions;
        self.set_query(query, SEARCH.to_owned(), window, cx);
    }

    /// Puts `text` in the field, with `placeholder`, and matches it.
    fn set_query(
        &mut self,
        text: String,
        placeholder: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.query.update(cx, |q, cx| {
            q.set_value(text, window, cx);
            q.set_placeholder(placeholder, window, cx);
            // A click on a row took focus off the field: each step starts typed into.
            q.focus(window, cx);
        });
        self.refilter(cx);
    }

    /// Refreshes the asked argument's choices when `entity` changes (the clusters, discovery,
    /// namespaces, the shown pod), so an open step does not keep a stale list.
    pub fn watch<T: 'static>(&mut self, entity: &Entity<T>, cx: &mut Context<Self>) {
        let watch = cx.observe(entity, |this, _, cx| this.refresh_choices(cx));
        self._subscriptions.push(watch);
    }

    /// Asks the source again for the current step's choices; when they changed, lists the new
    /// ones for the same query, keeping the selected row where it is still listed.
    fn refresh_choices(&mut self, cx: &mut Context<Self>) {
        let Step::Argument {
            spec, arg, input, ..
        } = &self.step
        else {
            return;
        };
        let fresh = (self.choices)(spec, &spec.args[*arg], cx);
        if fresh == *input {
            return;
        }
        let selected = self.rows.get(self.selected).filter(|_| self.chosen);
        let kept = match (selected, input) {
            (Some(Row::Choice { index, .. }), ArgInput::Choices { items, .. }) => {
                Some(Ok(items[*index].value.clone()))
            }
            (Some(Row::Typed(text)), _) => Some(Err(text.clone())),
            _ => None,
        };
        self.candidates = candidates_of(&fresh);
        if let Step::Argument { input, .. } = &mut self.step {
            *input = fresh;
        }
        let error = self.error.take();
        self.refilter(cx);
        self.error = error;
        let Step::Argument { input, .. } = &self.step else {
            return;
        };
        let at = self.rows.iter().position(|row| match (row, &kept, input) {
            (Row::Choice { index, .. }, Some(Ok(value)), ArgInput::Choices { items, .. }) => {
                items[*index].value == *value
            }
            (Row::Typed(text), Some(Err(kept)), _) => text == kept,
            _ => false,
        });
        if let Some(at) = at {
            self.selected = at;
            self.chosen = true;
            self.scroll.scroll_to_item(at, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    /// Matches the query against the current step's candidates, and selects the first row.
    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.query.read(cx).value().to_string();
        self.query_text = query.clone();
        if let (Step::Resources, Some(resources)) = (&self.step, &mut self.resources) {
            let found = resources.search.run(&resources.index, &query);
            resources.matched = found.matched;
            self.rows = found
                .hits
                .into_iter()
                .map(|h| Row::Resource {
                    item: h.item,
                    ranges: h.ranges,
                })
                .collect();
            // A click drawn on the rows before is told apart (`PalettePick::rows`), so it
            // cannot open the object now in its place.
            self.rows_version += 1;
            self.selected = 0;
            self.chosen = false;
            self.error = None;
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            cx.notify();
            return;
        }
        let hits = palette_model::rank(&query, &self.candidates);
        self.rows = match &self.step {
            Step::Actions => hits
                .into_iter()
                .map(|h| Row::Action {
                    entry: h.index,
                    ranges: h.ranges,
                })
                .collect(),
            Step::Argument { input, .. } => {
                let typed = query.trim();
                let offer_typed = !typed.is_empty()
                    && match input {
                        ArgInput::Choices { items, free } => {
                            *free
                                && !items
                                    .iter()
                                    .any(|c| c.value.to_lowercase() == typed.to_lowercase())
                        }
                        ArgInput::Text => true,
                    };
                let mut hits = hits;
                // Listed values equal to what was typed come first, the stored value before
                // what is shown for it: with contexts `cluster/prod` and an EKS ARN shown as
                // `cluster/prod`, Enter on "cluster/prod" takes the context of that name. The
                // sort is stable, so the rest keep their fuzzy order.
                if let ArgInput::Choices { items, .. } = input
                    && !typed.is_empty()
                {
                    // The typed text lowered once, and a sort only when something is exact.
                    let folded = typed.to_lowercase();
                    let keys: Vec<u8> = hits
                        .iter()
                        .map(|h| exactness(typed, &folded, &items[h.index]))
                        .collect();
                    if keys.iter().any(|&k| k < NOT_EXACT) {
                        let mut keyed: Vec<_> = keys.into_iter().zip(hits).collect();
                        keyed.sort_by_key(|(k, _)| *k);
                        hits = keyed.into_iter().map(|(_, h)| h).collect();
                    }
                }
                // Else what was typed comes first when it may be used as is: Enter on "kube"
                // takes "kube", not "kube-system".
                offer_typed
                    .then(|| Row::Typed(typed.to_owned()))
                    .into_iter()
                    .chain(hits.into_iter().map(|h| Row::Choice {
                        index: h.index,
                        ranges: h.ranges,
                    }))
                    .collect()
            }
            Step::Resources => Vec::new(),
        };
        self.rows_version += 1;
        self.selected = 0;
        self.chosen = false;
        self.error = None;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn move_by(&mut self, step: isize, cx: &mut Context<Self>) {
        self.selected = palette_model::step(self.selected, self.rows.len(), step);
        self.chosen = true;
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn pick(&mut self, action: &PalettePick, window: &mut Window, cx: &mut Context<Self>) {
        // Drawn on rows that a live refresh has replaced since: the row may hold another entry.
        if action.rows.is_some_and(|v| v != self.rows_version) {
            return;
        }
        if action.row < self.rows.len() {
            self.selected = action.row;
            self.confirm(window, cx);
        }
    }

    /// Enter: runs the selected action, or asks for its first argument; in an argument step,
    /// takes the selected (or typed) value and asks for the next one, or runs the action.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(self.selected).cloned() else {
            // A typed argument with nothing typed says what to type; an empty pick list says so
            // in the footer already.
            if let Step::Argument {
                spec,
                arg,
                input: ArgInput::Text,
                ..
            } = &self.step
            {
                self.error = Some(format!("Type the {} first", spec.args[*arg].description));
                cx.notify();
            }
            return;
        };
        match (&mut self.step, row) {
            (Step::Actions, Row::Action { entry, .. }) => {
                let spec = self.entries[entry].spec;
                if spec.takes_args() {
                    self.action_query = self.query_text.clone();
                    self.ask(spec, 0, Map::new(), Vec::new(), window, cx);
                } else {
                    self.run(spec, Value::Null, true, window, cx);
                }
            }
            (Step::Resources, Row::Resource { item, .. }) => {
                let Some(resources) = &self.resources else {
                    return;
                };
                let item = &resources.index.items[item as usize];
                let kind = resources.index.kind_of(item).id.to_string();
                let (name, args) = match &item.object {
                    None => (
                        OpenView::name_for_type(),
                        serde_json::json!({ "kind": kind }),
                    ),
                    Some(object) => (
                        OpenObject::name_for_type(),
                        serde_json::json!({
                            "kind": kind,
                            "namespace": object.namespace.as_deref(),
                            "name": &*object.name,
                        }),
                    ),
                };
                if let Some(spec) = registry::find(name) {
                    self.run(spec, args, false, window, cx);
                }
            }
            (
                Step::Argument {
                    spec,
                    arg,
                    args,
                    picked,
                    input,
                },
                row,
            ) => {
                let (text, shown, typed) = match (row, &*input) {
                    (Row::Choice { index, .. }, ArgInput::Choices { items, .. }) => (
                        items[index].value.clone(),
                        items[index].label.clone(),
                        false,
                    ),
                    (Row::Typed(text), _) => (text.clone(), text, true),
                    _ => return,
                };
                let (spec, at) = (*spec, *arg);
                let value = match arg_value(spec.args[at].kind, &text, typed) {
                    Ok(value) => value,
                    Err(why) => {
                        self.error = Some(why);
                        cx.notify();
                        return;
                    }
                };
                // Copies: until the action runs, this step keeps its picks, so a refusal at the
                // last step can be retried with the earlier ones.
                let mut args = args.clone();
                let mut picked = picked.clone();
                args.insert(spec.args[at].name.to_owned(), value);
                picked.push((at, shown));
                self.ask(spec, at + 1, args, picked, window, cx);
            }
            _ => {}
        }
    }

    /// Asks for `spec`'s next required argument from `from` on (optional ones keep their
    /// defaults), with `args` given so far; runs the action when none is left.
    fn ask(
        &mut self,
        spec: &'static ActionSpec,
        from: usize,
        args: Map<String, Value>,
        picked: Vec<(usize, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(arg) = next_required(spec, from) else {
            self.run(spec, Value::Object(args), true, window, cx);
            return;
        };
        let input = (self.choices)(spec, &spec.args[arg], cx);
        self.candidates = candidates_of(&input);
        self.step = Step::Argument {
            spec,
            arg,
            args,
            picked,
            input,
        };
        let placeholder = capitalized(spec.args[arg].description);
        self.set_query(String::new(), placeholder, window, cx);
    }

    /// Goes back a step: to the argument asked before, with the earlier picks kept, or to the
    /// actions, with the query that found this one. `false` while it lists actions.
    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Step::Argument {
            spec, args, picked, ..
        } = &mut self.step
        else {
            return false;
        };
        let spec = *spec;
        let mut args = std::mem::take(args);
        let mut picked = std::mem::take(picked);
        match picked.pop() {
            Some((arg, _)) => {
                args.remove(spec.args[arg].name);
                self.ask(spec, arg, args, picked, window, cx);
            }
            None => {
                let query = self.action_query.clone();
                self.show_actions(query, window, cx);
            }
        }
        true
    }

    /// Builds the action, checks it from where the palette came from, and runs it there once
    /// the palette has closed and given focus back. A refusal stays in the palette. With
    /// `remember`, the action goes first among the recent ones (not for the switcher's
    /// Open Kind… and Open Object…).
    fn run(
        &mut self,
        spec: &'static ActionSpec,
        args: Value,
        remember: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let action = match spec.build(args) {
            Ok(action) => action,
            Err(e) => {
                self.error = Some(refusal(&e));
                cx.notify();
                return;
            }
        };
        let origin = self.origin(window);
        if let Err(e) = registry::check_in(&origin, &*action, window, cx) {
            self.error = Some(refusal(&e));
            cx.notify();
            return;
        }
        if remember {
            cx.default_global::<RecentActions>().0.used(spec.name);
        }
        window.focus(&origin, cx);
        cx.emit(PaletteEvent::Dismissed);
        // After this update, so a handler that reads the palette's owner finds it closed.
        window.defer(cx, move |window, cx| {
            if let Err(e) = registry::dispatch_in(&origin, &*action, window, cx) {
                tracing::warn!("{e}");
            }
        });
    }

    /// Esc: goes back a step while an argument is asked for; else closes the palette, giving
    /// focus back.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.back(window, cx) {
            return;
        }
        let origin = self.origin(window);
        window.focus(&origin, cx);
        cx.emit(PaletteEvent::Dismissed);
    }

    /// Backspace: in the empty field of an argument step, goes back a step; else the field
    /// deletes, as usual.
    fn backspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.query_text.is_empty() || !self.back(window, cx) {
            cx.propagate();
        }
    }

    /// The palette's actions and their handlers, for [`Bind`].
    pub fn bind_actions<B: Bind<Self>>(bind: B) -> B {
        bind.bind::<PaletteUp>(|this, _, _, cx| this.move_by(-1, cx))
            .bind::<PaletteDown>(|this, _, _, cx| this.move_by(1, cx))
            .bind::<PaletteConfirm>(|this, _, window, cx| this.confirm(window, cx))
            .bind::<PaletteDismiss>(|this, _, window, cx| this.dismiss(window, cx))
            .bind::<PaletteBack>(|this, _, window, cx| this.backspace(window, cx))
            .bind::<PalettePick>(Self::pick)
    }

    /// The label and muted texts of row `ix`: what it says, its matched bytes, its category or
    /// detail, and its shortcut.
    fn row_texts(&self, ix: usize) -> Option<RowTexts> {
        let row = self.rows.get(ix)?;
        Some(match (row, &self.step) {
            (Row::Action { entry, ranges }, Step::Actions) => {
                let entry = &self.entries[*entry];
                RowTexts {
                    label: entry.spec.label.into(),
                    ranges: ranges.clone(),
                    detail: Some(entry.spec.category.name().into()),
                    shortcut: entry.shortcut.clone(),
                    action: Some(entry.spec.name),
                    kind: None,
                    cluster: None,
                }
            }
            (
                Row::Choice { index, ranges },
                Step::Argument {
                    input: ArgInput::Choices { items, .. },
                    ..
                },
            ) => {
                let choice = &items[*index];
                RowTexts {
                    label: choice.label.clone().into(),
                    ranges: ranges.clone(),
                    detail: choice.detail.clone().map(SharedString::from),
                    shortcut: None,
                    action: None,
                    kind: None,
                    cluster: None,
                }
            }
            (Row::Typed(text), _) => RowTexts {
                label: format!("“{text}”").into(),
                ranges: Vec::new(),
                detail: Some("typed".into()),
                shortcut: None,
                action: None,
                kind: None,
                cluster: None,
            },
            (Row::Resource { item, ranges }, Step::Resources) => {
                let resources = self.resources.as_ref()?;
                let item = resources.index.items.get(*item as usize)?;
                let kind = resources.index.kind_of(item);
                RowTexts {
                    label: item.text.to_string().into(),
                    ranges: ranges.clone(),
                    // An object says its kind; a kind, that it is one, and how many of its
                    // objects are in memory.
                    detail: Some(match (&item.object, kind.in_memory) {
                        (Some(_), _) => kind.label.clone().into(),
                        (None, _) if resources.loading => "Kind".into(),
                        (None, Some(n)) => format!("Kind · {n} in memory").into(),
                        (None, None) => "Kind · not loaded".into(),
                    }),
                    shortcut: None,
                    action: None,
                    kind: Some(kind.id.to_string()),
                    cluster: resources.cluster.clone(),
                }
            }
            _ => return None,
        })
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let theme = cx.theme();
        let highlight = HighlightStyle {
            color: Some(theme.list_highlight_foreground),
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        };
        let actions = matches!(self.step, Step::Actions);
        let resources = matches!(self.step, Step::Resources);
        range
            .filter_map(|ix| {
                let texts = self.row_texts(ix)?;
                let selected = ix == self.selected;
                let runs = texts
                    .ranges
                    .iter()
                    .map(|r| (r.clone(), highlight))
                    .collect::<Vec<_>>();
                Some(
                    h_flex()
                        .id(("palette-row", ix))
                        .aria_label(texts.label.clone())
                        .h(px(ROW_HEIGHT))
                        .w_full()
                        .px_3()
                        .gap_2()
                        .relative()
                        .when(selected, |el| {
                            el.bg(theme.list_active).child(super::theme::selection_edge(
                                theme.list_active_border,
                                px(0.),
                            ))
                        })
                        .when(!selected, |el| el.hover(|el| el.bg(theme.list_hover)))
                        .on_click(registry::on_click_in(
                            self.focus.clone(),
                            PalettePick {
                                row: ix,
                                rows: Some(self.rows_version),
                            },
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(StyledText::new(texts.label).with_highlights(runs)),
                        )
                        .when_some(texts.detail, |el, detail| {
                            el.child(
                                div()
                                    .flex_none()
                                    .max_w(px(220.))
                                    .truncate()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(detail),
                            )
                        })
                        // The switcher's rows name their cluster, in a column of its own.
                        .when(resources, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .w(px(CLUSTER_WIDTH))
                                    .flex()
                                    .justify_end()
                                    .text_xs()
                                    .truncate()
                                    .text_color(theme.muted_foreground)
                                    .children(texts.cluster.clone()),
                            )
                        })
                        // Every action row has the column, keyed or not, so categories line up.
                        .when(actions, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .w(px(SHORTCUT_WIDTH))
                                    .flex()
                                    .justify_end()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .children(texts.shortcut),
                            )
                        })
                        .test_support()
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// The line under the rows: why the last Enter did nothing, else what the selected action
    /// does, else why nothing is listed.
    fn footer(&self) -> Option<(SharedString, bool)> {
        if let Some(error) = &self.error {
            return Some((error.clone().into(), true));
        }
        if let (Step::Resources, Some(resources)) = (&self.step, &self.resources) {
            return Some((resources_footer(resources, self.rows.len()), false));
        }
        match (&self.step, self.rows.get(self.selected)) {
            (Step::Actions, Some(Row::Action { entry, .. })) => {
                Some((self.entries[*entry].spec.description.into(), false))
            }
            (Step::Actions, None) => Some(("No matching action available here".into(), false)),
            (
                Step::Argument {
                    spec,
                    arg,
                    input: ArgInput::Choices { items, free },
                    ..
                },
                None,
            ) => Some((
                if items.is_empty() {
                    nothing_to_pick(spec.args[*arg].kind, *free)
                } else {
                    "Nothing matches".into()
                },
                false,
            )),
            _ => None,
        }
    }
}

#[cfg(any(test, feature = "automation"))]
impl CommandPalette {
    /// What the palette shows, as data.
    pub fn view_state(&self) -> super::view_state::PaletteState {
        use super::view_state::{PALETTE_ROWS, PaletteRowState, PaletteState};
        let (action, step, argument) = match &self.step {
            Step::Argument { spec, arg, .. } => (
                Some(spec.name.to_owned()),
                Some(self.heading()),
                Some(spec.args[*arg].name.to_owned()),
            ),
            Step::Actions | Step::Resources => (None, None, None),
        };
        PaletteState {
            mode: match self.mode {
                Mode::Actions => "actions",
                Mode::Resources => "resources",
            },
            query: self.query_text.clone(),
            action,
            step,
            argument,
            rows: (0..self.rows.len().min(PALETTE_ROWS))
                .filter_map(|ix| self.row_texts(ix))
                .map(|t| PaletteRowState {
                    label: t.label.to_string(),
                    action: t.action.map(str::to_owned),
                    detail: t.detail.map(|d| d.to_string()),
                    shortcut: t.shortcut.map(|s| s.to_string()),
                    kind: t.kind,
                    cluster: t.cluster.map(|c| c.to_string()),
                })
                .collect(),
            total: self.rows.len(),
            selected: (!self.rows.is_empty()).then_some(self.selected),
            footer: self.footer().map(|(text, _)| text.to_string()),
            note: match (&self.step, &self.resources) {
                (Step::Resources, Some(resources)) => {
                    resources_note(resources).map(|n| n.to_string())
                }
                _ => None,
            },
            error: self.error.clone(),
        }
    }

    /// Whether focus is in the palette (its search field).
    pub fn has_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }
}

impl CommandPalette {
    /// Above the field while an argument is asked for: the action, then what was picked so far
    /// ("Open Logs by Label Selector… · demo").
    fn heading(&self) -> String {
        match &self.step {
            Step::Argument { spec, picked, .. } => std::iter::once(spec.label)
                .chain(picked.iter().map(|(_, shown)| shown.as_str()))
                .collect::<Vec<_>>()
                .join(" · "),
            Step::Actions | Step::Resources => String::new(),
        }
    }
}

/// What a row shows.
struct RowTexts {
    label: SharedString,
    ranges: Vec<Range<usize>>,
    detail: Option<SharedString>,
    shortcut: Option<SharedString>,
    /// The action's name, on an action row.
    #[cfg_attr(
        not(any(test, feature = "automation")),
        expect(dead_code, reason = "for the automation state")
    )]
    action: Option<&'static str>,
    /// The kind's id, on a switcher row.
    #[cfg_attr(
        not(any(test, feature = "automation")),
        expect(dead_code, reason = "for the automation state")
    )]
    kind: Option<String>,
    /// The cluster, on a switcher row.
    cluster: Option<SharedString>,
}

/// The switcher's line under the rows: what it searches, or why nothing is listed.
/// The switcher's second line in On view mode, where most kinds have no objects in memory.
const ON_VIEW_NOTE: &str = "Only kinds being watched list their objects; open a kind to load them.";

/// The switcher's note under its footer, if any.
fn resources_note(resources: &Resources) -> Option<SharedString> {
    (resources.on_view && resources.cluster.is_some()).then(|| ON_VIEW_NOTE.into())
}

fn resources_footer(resources: &Resources, rows: usize) -> SharedString {
    let index = &resources.index;
    if resources.cluster.is_none() {
        return "No cluster is active; select one first".into();
    }
    if rows == 0 {
        return if index.items.is_empty() && resources.loading {
            "Reading the objects in memory…".into()
        } else if index.items.is_empty() && resources.discovery_failed {
            "No kinds: discovery failed".into()
        } else if index.items.is_empty() {
            "No kinds (discovery has not finished)".into()
        } else {
            "Nothing matches".into()
        };
    }
    if resources.loading {
        return "Kinds only so far; reading the objects in memory…".into();
    }
    if resources.matched > rows {
        return format!(
            "The best {rows} of {} matches; type more to narrow them",
            resources.matched
        )
        .into();
    }
    format!(
        "{} kinds and {} objects in memory",
        index.kinds.len(),
        index.objects
    )
    .into()
}

/// What the query is matched against for an argument: each choice's label, and its stored
/// value too where that differs.
fn candidates_of(input: &ArgInput) -> Vec<Candidate> {
    match input {
        ArgInput::Choices { items, .. } => items
            .iter()
            .map(|c| Candidate {
                label: c.label.clone(),
                alt: (c.value != c.label).then(|| c.value.clone()),
                recent: None,
            })
            .collect(),
        ArgInput::Text => Vec::new(),
    }
}

/// [`exactness`] of a choice that is not what was typed.
const NOT_EXACT: u8 = 4;

/// How exactly `choice` is what was `typed` (`folded`: in lower case), best first: its stored value as typed, the value
/// ignoring case, the label as typed, the label ignoring case, then neither. Case counts first,
/// so contexts `ABC` and `abc` do not collide.
fn exactness(typed: &str, folded: &str, choice: &Choice) -> u8 {
    if choice.value == typed {
        0
    } else if choice.value.to_lowercase() == folded {
        1
    } else if choice.label == typed {
        2
    } else if choice.label.to_lowercase() == folded {
        3
    } else {
        NOT_EXACT
    }
}

/// The first required argument of `spec` from `from` on.
fn next_required(spec: &ActionSpec, from: usize) -> Option<usize> {
    (from..spec.args.len()).find(|&i| spec.args[i].required)
}

/// What an empty pick list says.
fn nothing_to_pick(kind: ArgKind, free: bool) -> SharedString {
    match kind {
        ArgKind::Cluster => "No clusters".into(),
        ArgKind::Container => "No containers".into(),
        ArgKind::Kind => "No kinds (discovery has not finished)".into(),
        ArgKind::Column => "No columns".into(),
        ArgKind::Namespace if free => "No namespaces listed; type one".into(),
        ArgKind::Namespace => "No namespaces".into(),
        _ => "Nothing to pick".into(),
    }
}

/// `text` as the JSON value an argument of `kind` takes. Typed text loses its surrounding
/// spaces; a value picked from a list is taken as it is (a context may be named `prod `).
fn arg_value(kind: ArgKind, text: &str, typed: bool) -> Result<Value, String> {
    let text = if typed { text.trim() } else { text };
    if text.is_empty() {
        return Err("Type a value first".to_owned());
    }
    match kind {
        ArgKind::Number => match text.parse::<u64>() {
            Ok(n) => Ok(Value::from(n)),
            Err(_) => text
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite())
                .map(Value::from)
                .ok_or_else(|| format!("“{text}” is not a number")),
        },
        ArgKind::Bool => match text.to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" => Ok(Value::Bool(true)),
            "false" | "no" | "off" => Ok(Value::Bool(false)),
            _ => Err(format!("“{text}” is not yes or no")),
        },
        ArgKind::List => Ok(Value::Array(
            text.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_owned()))
                .collect(),
        )),
        _ => Ok(Value::String(text.to_owned())),
    }
}

/// A refusal as the palette words it: the reason alone when the guard gave one.
fn refusal(e: &ActionError) -> String {
    match e {
        ActionError::Rejected { reason, .. } => capitalized(reason),
        ActionError::NotAvailable(_) => "Not available here any more".to_owned(),
        other => other.to_string(),
    }
}

/// `text` with its first letter in capitals.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

impl Focusable for CommandPalette {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cx: &Context<Self> = cx;
        let theme = cx.theme();
        let rows = self.rows.len();
        let list_height = rows.min(ROWS_SHOWN) as f32 * ROW_HEIGHT;
        let heading = match &self.step {
            Step::Argument { .. } => Some(SharedString::from(self.heading())),
            Step::Actions | Step::Resources => None,
        };
        let footer = self.footer();
        let note = match (&self.step, &self.resources) {
            (Step::Resources, Some(resources)) => resources_note(resources),
            _ => None,
        };
        let root = v_flex()
            .id("command-palette")
            .key_context(registry::PALETTE)
            .w(px(WIDTH))
            .max_w_full()
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius_lg)
            .shadow_lg()
            .overflow_hidden()
            .text_sm()
            // Pointer events stop at the panel, so nothing behind it gets them.
            .occlude()
            // A press outside closes it, also while it asks for an argument (only Esc and
            // Backspace go back a step), and goes on to what it pressed. Its own mode's toggle,
            // which closes it: the other mode's would switch to it.
            .map(|el| match self.mode {
                Mode::Actions => {
                    el.on_mouse_down_out(registry::on_click_passing(ToggleCommandPalette))
                }
                Mode::Resources => {
                    el.on_mouse_down_out(registry::on_click_passing(ToggleResourceSwitcher))
                }
            })
            .when_some(heading, |el, label| {
                el.child(
                    div()
                        .id("palette-step")
                        .aria_label(label.clone())
                        .px_3()
                        .pt_2()
                        .text_xs()
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(label)
                        .test_support(),
                )
            })
            .child(
                div().p_1().child(
                    Input::new(&self.query)
                        .id("palette-query")
                        .appearance(false)
                        .prefix(Icon::new(IconName::Search).small()),
                ),
            )
            .when(rows > 0, |el| {
                el.child(
                    div().border_t_1().border_color(theme.border).py_1().child(
                        uniform_list("palette-rows", rows, cx.processor(Self::render_rows))
                            .track_scroll(&self.scroll)
                            .h(px(list_height)),
                    ),
                )
            })
            .when_some(footer, |el, (text, error)| {
                el.child(
                    div()
                        .id("palette-footer")
                        .aria_label(text.clone())
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(theme.border)
                        .text_xs()
                        .truncate()
                        .text_color(if error {
                            theme.danger
                        } else {
                            theme.muted_foreground
                        })
                        .child(text)
                        .test_support(),
                )
            })
            .when_some(note, |el, note| {
                el.child(
                    div()
                        .id("palette-note")
                        .aria_label(note.clone())
                        .px_3()
                        .pb_1()
                        .text_xs()
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(note)
                        .test_support(),
                )
            });
        Self::bind_actions(OnView::new(root, cx))
            .into_element()
            .test_support()
    }
}

#[cfg(test)]
#[path = "palette_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "switcher_tests.rs"]
mod switcher_tests;
