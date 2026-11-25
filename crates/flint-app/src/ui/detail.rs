//! The generic detail view (#10): Overview (metadata, labels, annotations, owners, conditions,
//! events, and Secret data masked per key) and YAML with syntax highlighting, editable through
//! `detail_edit`.
//!
//! The object is fetched through the core ([`CoreHandle::fetch_object`]) and fetched again when
//! the list reports that its resource version moved.

use std::collections::HashSet;
use std::sync::Arc;

use flint_core::read::event_time;
use flint_core::table::{human_duration, unix_now};
use flint_core::{ClusterId, CoreHandle, KindId, ObjectDetail, SyncState, WorkloadKind};
use gpui_kit::base::{TestSupportExt as _, TestTextExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::Diff;
use gpui_kit::component::input::{Editor, EditorState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _, Theme,
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::icons::FlintIcon;
use super::list::tabular_figures;
use super::list_usage::{UsageSource, UsageSubject};
use super::log_view::LogRequest;
use super::menu_fit::{fit_picker, picker_item};
use super::menu_record;
use super::notice::{clamped, notice, notice_detail, notice_with};
use super::object::{self, MASK, Overview, PodContainer, SecretValue};
use super::registry::{
    self, ApplyYamlEdit, BackToYamlEdit, CancelYamlEdit, Close, EditYaml, OpenContainerLogs,
    OpenContainerShell, OpenLogs, OpenPreviousLogs, OpenShell, Refresh, ReloadYamlEdit,
    ReviewYamlEdit, ShowOverview, ShowYaml, ToggleAnnotations, ToggleLabels, ToggleManagedFields,
    ToggleSecretValue, UnlockCluster,
};
use super::terminal_model::ShellRequest;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTab {
    Overview,
    Yaml,
}

/// Identifies the object a detail view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    pub cluster: ClusterId,
    pub kind: KindId,
    /// What to call the kind ("Deployments" for a list, the singular from the object itself).
    pub kind_label: String,
    pub namespace: Option<String>,
    pub name: String,
    pub uid: Box<str>,
}

enum Fetch {
    Loading,
    Loaded {
        detail: Arc<ObjectDetail>,
        /// Shared, so a render borrows it instead of copying labels, annotations and events.
        overview: Arc<Overview>,
    },
    Failed(String),
}

/// A fetched object, read and rendered off the UI thread.
struct Prepared {
    detail: Arc<ObjectDetail>,
    overview: Arc<Overview>,
    /// The YAML tab's text, rendered with `managed` (whether managedFields were shown).
    yaml: String,
    managed: bool,
    /// When the fetch began: the copy is at least this recent.
    fetched_at: jiff::Timestamp,
}

impl Prepared {
    fn new(
        detail: Arc<ObjectDetail>,
        is_secret: bool,
        managed: bool,
        fetched_at: jiff::Timestamp,
    ) -> Self {
        let overview = Arc::new(object::overview(&detail.object, is_secret));
        let yaml = object::render_yaml(&detail, is_secret, managed);
        Prepared {
            detail,
            overview,
            yaml,
            managed,
            fetched_at,
        }
    }
}

pub enum DetailEvent {
    Close,
    /// Open (or show) a log tab for one of the pod's containers, or for a workload's pods.
    OpenLogs(LogRequest, TakeFocus),
    /// Open (or show) a shell tab in a container of the pod, or of a workload's ready pod.
    OpenShell(ShellRequest, TakeFocus),
    /// Logs or Shell was refused, or could not be done once what it waited for loaded: one
    /// sentence saying what and why ([`refused`]), for the window to say.
    Refused(String),
    /// Apply was pressed on a reviewed edit: the window asks to confirm it, naming the cluster.
    /// The guard is on the connection the review started on; `None` if it had none settled.
    ConfirmWrite(
        Box<flint_core::write::WriteReview>,
        Option<super::write_binding::ConnectionGuard>,
    ),
}

/// Whether the tab an action opens takes keyboard focus.
#[derive(Debug, Clone, PartialEq)]
pub enum TakeFocus {
    /// Yes: it opens as asked.
    Yes,
    /// Only if focus is still where it was when the action was asked for: it waited for its
    /// object to load, and the user may have moved on meanwhile.
    IfStill(Option<FocusHandle>),
}

/// What Logs and Shell act on: a pod, a workload's pods, or nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum Subject {
    Pod,
    Workload(WorkloadKind),
    Other,
}

impl Subject {
    pub(in crate::ui) fn of(kind: &KindId) -> Self {
        if *kind == KindId::pods() {
            Subject::Pod
        } else {
            WorkloadKind::from_kind(kind).map_or(Subject::Other, Subject::Workload)
        }
    }
}

/// Logs or Shell, as the detail pane runs them: at once, or once what they need has loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::ui) enum Pending {
    Logs {
        previous: bool,
    },
    /// In the named container, or the default one.
    Shell {
        container: Option<String>,
    },
}

/// How long an action waits for its object (or a workload's pods) before it gives up.
pub(in crate::ui) const PENDING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// An action waiting for what it needs.
pub(in crate::ui) struct Waiting {
    pub action: Pending,
    /// Whether its tab takes focus when it opens: for an ask from the user, only if focus
    /// is still where it was then ([`TakeFocus::IfStill`]).
    pub focus: TakeFocus,
    /// Gives up after [`PENDING_TIMEOUT`].
    pub _timeout: Task<()>,
}

impl EventEmitter<DetailEvent> for DetailView {}

pub struct DetailView {
    core: CoreHandle,
    object: ObjectRef,
    is_secret: bool,
    subject: Subject,
    tab: DetailTab,
    fetch: Fetch,
    /// A fetch is running.
    fetching: bool,
    /// The object changed while a fetch ran: fetch again when it ends.
    refetch: bool,
    /// The last fetch failed while an older copy is still shown.
    refresh_error: Option<String>,
    /// Whether the copy shown can be presented as current, from the kind's sync state.
    freshness: sync::Freshness,
    /// When the copy shown was fetched.
    fetched_at: Option<jiff::Timestamp>,
    /// Automatic fetches made since the last success, which space out the next one.
    error_retries: u32,
    /// The kind was seen current while an automatic fetch could not start yet.
    error_retry_due: bool,
    /// Runs until another automatic fetch may start.
    _error_retry_cooldown: Option<Task<()>>,
    /// The object disappeared from the list (deleted).
    gone: bool,
    revealed: HashSet<String>,
    show_annotations: bool,
    show_labels: bool,
    yaml: Entity<EditorState>,
    yaml_text: String,
    /// Show `metadata.managedFields` in the YAML (hidden by default).
    show_managed_fields: bool,
    /// A workload's pods, for Shell ([`pods`]).
    pods: Pods,
    /// The workload changed since its pods were listed (or the list failed): what they show
    /// is not to be trusted, and they are listed again soon.
    pods_stale: bool,
    /// Counts the workload's changes, so a list that ran across one stays stale.
    pods_generation: u64,
    /// A list of the stale pods is due ([`pods::RELIST_DELAY`]).
    _relist: Option<Task<()>>,
    /// The pod list running.
    listing: Option<pods::Listing>,
    /// A current pod list was asked for while a cached one ran: it starts when that one ends.
    current_wanted: bool,
    /// Logs or Shell asked for while loading (`FLINT_VIEW=…#logs`, a row just selected).
    pending: Option<Waiting>,
    /// Why no session can open on the object's cluster (it is not connected), as the workspace
    /// says it (`Workspace::sessions_blocked`): Logs and Shell are disabled, saying so.
    sessions_blocked: Option<String>,
    /// The object's cluster is protected (ADR-0015), as the workspace says it: Shell is
    /// disabled, saying so, with Unlock beside it. Logs and browsing are not affected.
    protected: bool,
    /// What the protected reason calls the object's cluster, as the workspace says it.
    cluster_label: String,
    /// For a row of a cluster set: the exact context, always shown as what the actions apply to.
    row_cluster: Option<String>,
    /// For a row of a cluster set: the server it was admitted on. Never changes.
    row_server: Option<String>,
    /// For a row of a cluster set: why the core refuses writes on its member now, if it does.
    row_writes_paused: Option<String>,
    /// The server the object's cluster shows, as the workspace says it: what an edit is for.
    server: Option<String>,
    /// The confirmed endpoint number of the object's cluster, for binding an edit to it.
    endpoint: Option<u64>,
    /// What RBAC says about this object's actions, for showing them ([`access`]).
    access: access::ObjectAccess,
    /// Guards a YAML edit's review on its connection; `None` until the workspace gives it, when
    /// no edit can be confirmed.
    connection_guards: Option<super::write_binding::ConnectionGuards>,
    /// The YAML edit under way, in its own editor: a list update never touches its text.
    edit: Option<edit::YamlEdit>,
    /// The pane's own focus, for Tab and Shift-Tab between panes (UP-33).
    focus: FocusHandle,
    _fetch: Task<()>,
    /// Follows the kind's sync state. The pane lives inside its list, which watches the same
    /// kind, so this subscription starts no watch of its own.
    _sync: Task<()>,
    _show_pods: Task<()>,
    _yaml: Task<()>,
    /// A pod's or node's use against its budget, from the cluster feed's metrics poll.
    usage: usage::DetailUsage,
    /// Follows that poll. Makes no request.
    _usage: Task<()>,
}

impl DetailView {
    pub fn new(
        core: CoreHandle,
        object: ObjectRef,
        tab: DetailTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let yaml = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("yaml")
                .line_number(true)
                .soft_wrap(false)
        });
        let is_secret = object.kind.as_str() == "secrets";
        let subject = Subject::of(&object.kind);
        let mut subscription = core.subscribe(&object.cluster, &object.kind);
        let first = subscription.latest();
        let freshness = sync::Freshness::new(first.state.clone());
        // What identifies the rows: a namespace-limited list has a revision per namespace instead
        // of its own, and a snapshot no watch built has neither.
        let mut rows = (first.revision, first.limited_to.clone());
        let follow_sync = cx.spawn_in(window, async move |this, cx| {
            while subscription.changed().await {
                let snapshot = subscription.latest();
                let next = (snapshot.revision, snapshot.limited_to.clone());
                let republished = (next.0 != 0 || next.1.is_some()) && next == rows;
                rows = next;
                let state = snapshot.state.clone();
                let followed = this.update_in(cx, |view, window, cx| {
                    view.sync_changed(state, republished, window, cx);
                });
                if followed.is_err() {
                    break;
                }
            }
        });
        let object_access = access::ObjectAccess::new(
            &object.kind,
            object.namespace.as_deref(),
            &object.name,
            subject,
        );
        let cluster_label = flint_core::display::single_line(&object.cluster);
        let follow_usage = match UsageSubject::of(&object.kind) {
            Some(subject) => {
                let (core, cluster) = (core.clone(), object.cluster.clone());
                let mut updates = core.telemetry_updates();
                cx.spawn(async move |this, cx| {
                    while updates.changed().await {
                        let source = UsageSource::of(subject, core.telemetry(&cluster).as_ref());
                        let followed = this.update(cx, |view, cx| {
                            if view.usage.set_source(source) {
                                cx.notify();
                            }
                        });
                        if followed.is_err() {
                            break;
                        }
                    }
                })
            }
            None => Task::ready(()),
        };
        let mut view = DetailView {
            core,
            object,
            is_secret,
            subject,
            tab,
            fetch: Fetch::Loading,
            fetching: false,
            refetch: false,
            refresh_error: None,
            freshness,
            fetched_at: None,
            error_retries: 0,
            error_retry_due: false,
            _error_retry_cooldown: None,
            gone: false,
            revealed: HashSet::new(),
            show_annotations: false,
            show_labels: true,
            yaml,
            yaml_text: String::new(),
            show_managed_fields: false,
            pods: Pods::NotListed,
            pods_stale: false,
            pods_generation: 0,
            _relist: None,
            listing: None,
            current_wanted: false,
            pending: None,
            sessions_blocked: None,
            protected: false,
            cluster_label,
            row_cluster: None,
            row_server: None,
            row_writes_paused: None,
            server: None,
            endpoint: None,
            access: object_access,
            connection_guards: None,
            edit: None,
            focus: cx.focus_handle(),
            _fetch: Task::ready(()),
            _sync: follow_sync,
            _show_pods: Task::ready(()),
            _yaml: Task::ready(()),
            usage: usage::DetailUsage::default(),
            _usage: follow_usage,
        };
        view.refresh(window, cx);
        view.list_pods_soon(cx);
        view
    }

    /// The pane's own focus, which Tab and Shift-Tab move to.
    pub(in crate::ui) fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Says why no session can open on the object's cluster now, or `None` once it can.
    pub(in crate::ui) fn set_sessions_blocked(
        &mut self,
        why: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.sessions_blocked != why {
            self.sessions_blocked = why;
            cx.notify();
        }
    }

    /// Says whether the object's cluster is protected (ADR-0015), and what its reason calls it.
    pub(in crate::ui) fn set_protected(
        &mut self,
        protected: bool,
        label: String,
        cx: &mut Context<Self>,
    ) {
        if self.protected != protected || self.cluster_label != label {
            self.protected = protected;
            self.cluster_label = label;
            cx.notify();
        }
    }

    /// Makes this the detail of a cluster set's row: names its exact context in the header,
    /// whatever the window's width, and pins every write from here to its admitted `server`.
    pub(in crate::ui) fn set_row_cluster(
        &mut self,
        cluster: String,
        server: String,
        cx: &mut Context<Self>,
    ) {
        self.row_cluster = Some(cluster);
        self.server = Some(server.clone());
        self.row_server = Some(server);
        cx.notify();
    }

    /// Says why a set's row cannot write now, or `None` once it can.
    pub(in crate::ui) fn set_row_writes_paused(
        &mut self,
        why: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.row_writes_paused != why {
            self.row_writes_paused = why;
            cx.notify();
        }
    }

    /// Why a set's row cannot write now.
    pub(in crate::ui) fn row_writes_paused(&self) -> Option<&str> {
        self.row_writes_paused.as_deref()
    }

    /// Says which server the object's cluster points at now. A set's row keeps its pin.
    /// Where a YAML edit's review takes the guard its confirmation is bound to.
    pub(in crate::ui) fn set_connection_guards(
        &mut self,
        guards: super::write_binding::ConnectionGuards,
    ) {
        self.connection_guards = Some(guards);
    }

    pub(in crate::ui) fn set_server(&mut self, server: Option<String>) {
        if self.row_server.is_none() {
            self.server = server;
        }
    }

    /// The admitted server of a cluster set's row: its writes go there or nowhere.
    pub(in crate::ui) fn row_server(&self) -> Option<&str> {
        self.row_server.as_deref()
    }

    /// The server a YAML edit started now would be for.
    #[cfg(test)]
    pub(in crate::ui) fn edit_server(&self) -> Option<&str> {
        self.server.as_deref()
    }

    pub fn object(&self) -> &ObjectRef {
        &self.object
    }

    /// Why RBAC forbids `what` on this object, if it clearly does.
    pub(in crate::ui) fn forbidden(&self, what: access::Gated) -> Option<String> {
        self.access.forbidden(what)
    }

    pub fn tab(&self) -> DetailTab {
        self.tab
    }

    /// Opens the logs once the object is fetched, as Logs would (`FLINT_VIEW=…#logs`).
    pub fn open_logs_when_loaded(&mut self, cx: &mut Context<Self>) {
        // Asked for at startup, not by a key: the tab takes focus when it opens.
        self.wait_for(Pending::Logs { previous: false }, TakeFocus::Yes, cx);
    }

    /// What Logs and Shell act on.
    pub(in crate::ui) fn subject(&self) -> Subject {
        self.subject
    }

    /// Fetches the object again. Calls while a fetch runs coalesce into one more fetch.
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The workload changed: the pods listed may be out of date. They are listed again
        // soon, at most once per `pods::RELIST_DELAY` (see `pods`).
        self.pods_changed(cx);
        if self.fetching {
            self.refetch = true;
            return;
        }
        self.fetching = true;
        self.refetch = false;
        let o = &self.object;
        let fetch = self
            .core
            .fetch_object(&o.cluster, &o.kind, o.namespace.as_deref(), &o.name);
        let is_secret = self.is_secret;
        let managed = self.show_managed_fields;
        let started = jiff::Timestamp::now();
        self._fetch = cx.spawn_in(window, async move |this, cx| {
            let result = fetch.await;
            // Reading the object and rendering YAML are not free for big objects: off the UI
            // thread.
            let prepared = cx
                .background_spawn(async move {
                    result.map(|detail| Prepared::new(detail, is_secret, managed, started))
                })
                .await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.fetching = false;
                match prepared {
                    Ok(prepared) => view.loaded(prepared, window, cx),
                    Err(error) => match view.fetch {
                        Fetch::Loaded { .. } => view.refresh_error = Some(error),
                        _ => {
                            view.fetch = Fetch::Failed(error);
                            view.run_pending(cx);
                        }
                    },
                }
                if view.refetch {
                    view.refresh(window, cx);
                } else if view.error_retry_due {
                    view.retry_held_error(window, cx);
                }
                cx.notify();
            });
        });
    }

    /// The kind's sync state moved: redraw, and fetch again if the connection came back.
    /// `republished`: the same rows came again, as they do when a cluster's pause ends.
    fn sync_changed(
        &mut self,
        state: SyncState,
        republished: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if *self.freshness.state() != state {
            if !state.is_current() {
                self.error_retries = 0;
            }
            if self.freshness.update(state, self.holds_fetch_error()) {
                self.refresh(window, cx);
            }
            cx.notify();
        }
        self.retry_held_error(window, cx);
        if republished && self.freshness.state().is_current() {
            self.retry_failed_pods(cx);
        }
    }

    fn holds_fetch_error(&self) -> bool {
        !self.gone && (matches!(self.fetch, Fetch::Failed(_)) || self.refresh_error.is_some())
    }

    /// Fetches again if the pane holds an error while its kind is current. A subscription can
    /// skip a brief outage, so the pane cannot rely on seeing the kind come back. Only the kind's
    /// updates start this, and [`sync::error_retry_delay`] spaces it out on a lasting failure.
    fn retry_held_error(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.gone || !self.freshness.state().is_current() {
            self.error_retry_due = false;
        } else if self.fetching {
            self.error_retry_due = true;
        } else if !self.holds_fetch_error() {
            self.error_retry_due = false;
        } else if self._error_retry_cooldown.is_some() {
            self.error_retry_due = true;
        } else {
            self.error_retry_due = false;
            let timer = cx
                .background_executor()
                .timer(sync::error_retry_delay(self.error_retries));
            self.error_retries = self.error_retries.saturating_add(1);
            self._error_retry_cooldown = Some(cx.spawn_in(window, async move |this, cx| {
                timer.await;
                let _ = this.update_in(cx, |view, window, cx| {
                    view._error_retry_cooldown = None;
                    if view.error_retry_due {
                        view.retry_held_error(window, cx);
                    }
                });
            }));
            self.refresh(window, cx);
        }
    }

    /// Whether to show the stale notice. A deleted object's own notice says what the copy is
    /// once the connection is back, when "until it is fetched" could never come true.
    fn shows_stale_notice(&self) -> bool {
        self.freshness.is_stale()
            && matches!(self.fetch, Fetch::Loaded { .. })
            && !(self.gone && self.freshness.state().is_current())
    }

    /// `color`, or the muted colour while the copy shown may be out of date: a status from
    /// before an outage must not read as the object's status now.
    fn status_color(&self, color: Hsla, theme: &Theme) -> Hsla {
        if self.freshness.is_stale() {
            theme.muted_foreground
        } else {
            color
        }
    }

    /// The moment ages are counted to: now, or when the copy was fetched if it may be stale.
    fn age_reference(&self) -> i64 {
        match self.fetched_at {
            Some(at) if self.freshness.is_stale() => at.as_second(),
            _ => unix_now(),
        }
    }

    /// Fetches the object again, from an error.
    fn render_retry(&self) -> Button {
        Button::new("detail-retry")
            .label("Retry")
            .xsmall()
            .disabled(self.fetching || self.gone)
            .on_click(registry::on_click(Refresh))
    }

    /// The name now belongs to another object, so the one this pane is for was deleted. Its copy
    /// stays, and the other object is neither shown nor available to an edit.
    fn replaced(&mut self, cx: &mut Context<Self>) {
        self.gone = true;
        self.usage.mark_deleted();
        self.refetch = false;
        self.error_retry_due = false;
        self._error_retry_cooldown = None;
        self.refresh_error = None;
        if !matches!(self.fetch, Fetch::Loaded { .. }) {
            self.fetch = Fetch::Failed(sync::REPLACED.to_owned());
        }
        self.run_pending(cx);
    }

    /// Shows a fetched object: what a fetch that succeeded ends with.
    fn loaded(&mut self, prepared: Prepared, window: &mut Window, cx: &mut Context<Self>) {
        let Prepared {
            detail,
            overview,
            yaml,
            managed,
            fetched_at,
        } = prepared;
        if sync::is_other_object(&self.object, &detail.object) {
            self.replaced(cx);
            return;
        }
        self.refresh_error = None;
        self.error_retries = 0;
        self.error_retry_due = false;
        self._error_retry_cooldown = None;
        self.fetched_at = Some(fetched_at);
        self.freshness.fetched(self.refetch);
        // A re-render of the previous copy must not land after this one.
        self._yaml = Task::ready(());
        self.usage
            .set_object(UsageSubject::of(&self.object.kind), &detail.object);
        self.fetch = Fetch::Loaded { detail, overview };
        self.run_pending(cx);
        if self.show_managed_fields == managed {
            self.set_yaml(yaml, window, cx);
        } else {
            // Toggled while this fetch ran.
            self.rerender_yaml(window, cx);
        }
    }

    fn set_yaml(&mut self, yaml: String, window: &mut Window, cx: &mut Context<Self>) {
        if yaml != self.yaml_text {
            self.yaml
                .update(cx, |state, cx| state.set_value(yaml.clone(), window, cx));
            self.yaml_text = yaml;
        }
    }

    /// Renders the YAML again (off the UI thread) after the managedFields toggle changed.
    fn rerender_yaml(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Fetch::Loaded { detail, .. } = &self.fetch else {
            return;
        };
        let detail = detail.clone();
        let (is_secret, managed) = (self.is_secret, self.show_managed_fields);
        self._yaml = cx.spawn_in(window, async move |this, cx| {
            let yaml = cx
                .background_spawn(async move { object::render_yaml(&detail, is_secret, managed) })
                .await;
            let _ = this.update_in(cx, |view, window, cx| {
                if view.show_managed_fields == managed {
                    view.set_yaml(yaml, window, cx);
                    cx.notify();
                }
            });
        });
    }

    fn toggle_managed_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_managed_fields = !self.show_managed_fields;
        self.rerender_yaml(window, cx);
        cx.notify();
    }

    /// The object was deleted: keep showing the last copy, marked as gone.
    pub fn mark_gone(&mut self, cx: &mut Context<Self>) {
        self.gone = true;
        self.usage.mark_deleted();
        cx.notify();
    }

    /// Shows the object as still loading, as on a slow server (the test servers fail at once).
    #[cfg(test)]
    pub fn show_loading(&mut self, cx: &mut Context<Self>) {
        self._fetch = Task::ready(());
        self.fetching = false;
        self.fetch = Fetch::Loading;
        cx.notify();
    }

    /// Shows the object as failed to fetch, as if its first fetch had returned `error`.
    #[cfg(test)]
    pub fn show_failed(&mut self, error: String, cx: &mut Context<Self>) {
        self._fetch = Task::ready(());
        self.fetching = false;
        self.fetch = Fetch::Failed(error);
        self.run_pending(cx);
        cx.notify();
    }

    /// Shows `detail` as a fetch that succeeds does, through the same code, after cancelling
    /// the fetch that is running: the test servers never answer.
    #[cfg(test)]
    pub fn show_fetched(
        &mut self,
        detail: ObjectDetail,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._fetch = Task::ready(());
        self.fetching = false;
        self.refetch = false;
        let prepared = Prepared::new(
            Arc::new(detail),
            self.is_secret,
            self.show_managed_fields,
            jiff::Timestamp::now(),
        );
        self.loaded(prepared, window, cx);
        cx.notify();
    }

    fn render_header(&self, theme: &Theme, _cx: &mut Context<Self>) -> impl IntoElement {
        let selected = match self.tab {
            DetailTab::Overview => 0,
            DetailTab::Yaml => 1,
        };
        let kind = match &self.fetch {
            Fetch::Loaded { overview, .. } if !overview.kind.is_empty() => overview.kind.clone(),
            _ => self.object.kind_label.clone(),
        };
        let full_name = match &self.object.namespace {
            Some(ns) => format!("{ns}/{}", self.object.name),
            None => self.object.name.clone(),
        };
        v_flex()
            .flex_none()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .h(px(super::workspace::HEADER_HEIGHT))
                    .px_3()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child(kind),
                    )
                    .child(
                        div()
                            .id("detail-name")
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_semibold()
                            .font_family(theme.mono_font_family.clone())
                            .truncate()
                            .child(self.object.name.clone())
                            .tooltip(move |window, cx| {
                                Tooltip::new(full_name.clone()).build(window, cx)
                            }),
                    )
                    .when(self.fetching, |el| el.child(Spinner::new().xsmall()))
                    .children(self.render_logs_button())
                    .children(self.render_shell_button())
                    .child(
                        Button::new("detail-refresh")
                            .icon(IconName::RefreshCw)
                            .xsmall()
                            .ghost()
                            .tooltip("Fetch again")
                            .on_click(registry::on_click(Refresh)),
                    )
                    .child(
                        Button::new("detail-close")
                            .icon(IconName::Close)
                            .xsmall()
                            .ghost()
                            .tooltip("Close (Esc)")
                            .on_click(registry::on_click(Close)),
                    ),
            )
            .when_some(self.row_cluster.clone(), |el, cluster| {
                // Wrapped, never cut: near-identical contexts must read apart.
                let text = format!("Logs, Shell and actions apply to {cluster}");
                el.child(
                    div()
                        .id("detail-cluster")
                        .px_3()
                        .pb_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .aria_label(text.clone())
                        .child(text)
                        .test_support(),
                )
            })
            .children(self.render_managed_by(theme))
            .children(self.render_captions(theme))
            .child(
                TabBar::new("detail-tabs")
                    .underline()
                    .small()
                    .px_2()
                    .selected_index(selected)
                    .child(Tab::new().label("Overview"))
                    .child(Tab::new().label("YAML"))
                    .on_click(|ix, window, cx| {
                        let shown = if *ix == 0 {
                            registry::perform(&ShowOverview, window, cx)
                        } else {
                            registry::perform(&ShowYaml, window, cx)
                        };
                        if let Err(e) = shown {
                            tracing::warn!("{e}");
                        }
                    }),
            )
    }

    /// "managed by Argo CD: shop" or "managed by Flux: flux-system/apps", for a loaded object a
    /// GitOps controller manages, on its own line so it never crowds the header's buttons. Long
    /// names are cut; the tooltip has the whole text.
    fn render_managed_by(&self, theme: &Theme) -> Option<AnyElement> {
        let Fetch::Loaded { overview, .. } = &self.fetch else {
            return None;
        };
        let text = format!("managed by {}", overview.managed_by.as_ref()?);
        let full: SharedString = text.clone().into();
        Some(
            div()
                .id("detail-managed-by")
                .px_3()
                .pb_1()
                .min_w_0()
                .text_xs()
                .text_color(theme.muted_foreground)
                .truncate()
                .child(text.clone())
                .test_text(text)
                .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
                .test_support()
                .into_any_element(),
        )
    }

    /// For pods: Logs, with a container picker when the pod has several containers or a
    /// previous instance to read. For workloads: Logs of every pod (UP-45).
    fn render_logs_button(&self) -> Option<AnyElement> {
        if self.subject == Subject::Other {
            return None;
        }
        if let Some(why) = &self.sessions_blocked {
            return Some(
                Button::new("detail-logs")
                    .xsmall()
                    .ghost()
                    .label("Logs")
                    .disabled(true)
                    .tooltip(why.clone())
                    .into_any_element(),
            );
        }
        let keys = registry::key_hint("secondary-l");
        let button = Button::new("detail-logs").xsmall().ghost().label("Logs");
        // Logs that cannot open (the pod could not be fetched, say) stay, disabled, saying why.
        if let Err(e) = self.log_plan::<OpenLogs>(None, false) {
            return Some(
                button
                    .disabled(true)
                    .tooltip(actions::reason(&e))
                    .into_any_element(),
            );
        }
        if let Subject::Workload(_) = self.subject {
            return Some(
                button
                    .tooltip(format!("Open the logs of every pod in the dock ({keys})"))
                    .on_click(registry::on_click(OpenLogs))
                    .into_any_element(),
            );
        }
        let containers: Vec<PodContainer> = self.log_containers().to_vec();
        match containers.as_slice() {
            // Still loading: Logs waits for the pod.
            [] => Some(
                button
                    .tooltip(format!("Open the logs in the dock ({keys})"))
                    .on_click(registry::on_click(OpenLogs))
                    .into_any_element(),
            ),
            [only] if !only.has_previous => Some(
                button
                    .tooltip(format!("Open the logs in the dock ({keys})"))
                    .on_click(registry::on_click(OpenLogs))
                    .into_any_element(),
            ),
            _ => Some(
                with_dropdown_chevron(button)
                    .tooltip(picker_hint(
                        containers.iter().find(|c| c.default).map(|c| &c.name[..]),
                        &keys,
                    ))
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                        menu_record::record(cx, "detail-logs", || {
                            let label =
                                |c: &PodContainer| container_label(&c.name, c.init, c.default);
                            containers
                                .iter()
                                .chain(containers.iter().filter(|c| c.has_previous))
                                .map(|c| menu_record::Entry::new(label(c), true))
                                .collect()
                        });
                        let mut menu = fit_picker(menu, window, cx).label("Container");
                        for c in &containers {
                            let (item, action) = log_item(c, false);
                            menu = menu.item(item.on_click(registry::on_click(action)));
                        }
                        if containers.iter().any(|c| c.has_previous) {
                            menu = menu.separator().label("Previous instance");
                            for c in containers.iter().filter(|c| c.has_previous) {
                                let (item, action) = log_item(c, true);
                                menu = menu.item(item.on_click(registry::on_click(action)));
                            }
                        }
                        menu
                    })
                    .into_any_element(),
            ),
        }
    }

    /// For pods and workloads: Shell (in a workload's first ready pod), with a container picker
    /// when the pod has several app containers. When Shell cannot open (no ready pod, a pod
    /// list that failed, a finished or deleted pod) it shows disabled, saying why.
    fn render_shell_button(&self) -> Option<AnyElement> {
        if self.subject == Subject::Other {
            return None;
        }
        let button = Button::new("detail-shell").xsmall().ghost().label("Shell");
        if let Some(why) = self
            .sessions_blocked
            .as_ref()
            .or(self.row_writes_paused.as_ref())
        {
            return Some(
                button
                    .disabled(true)
                    .tooltip(why.clone())
                    .into_any_element(),
            );
        }
        if self.protected {
            // Disabled, saying why, with the unlock in place (UP-54).
            let why = protected_reason(&self.cluster_label);
            return Some(
                h_flex()
                    .gap_1()
                    .child(button.disabled(true).tooltip(why))
                    .child(
                        Button::new("detail-unlock")
                            .xsmall()
                            .ghost()
                            .icon(FlintIcon::Lock)
                            .label("Unlock…")
                            .tooltip(format!(
                                "Unlock {} until Flint quits, after a confirmation naming its \
                                 server",
                                self.object.cluster
                            ))
                            .on_click(registry::on_click(UnlockCluster {
                                cluster: self.object.cluster.clone(),
                            })),
                    )
                    .into_any_element(),
            );
        }
        // A pod whose default container is not running still offers the others.
        let picker = self.shell_picker();
        if let Err(e) = self.shell_check::<OpenShell>(None)
            && picker.is_none()
        {
            return Some(
                button
                    .disabled(true)
                    .tooltip(actions::reason(&e))
                    .into_any_element(),
            );
        }
        let keys = registry::key_hint("secondary-shift-e");
        let shell = self.shell_pod();
        let button = match (&shell, self.subject) {
            (Some(shell), Subject::Workload(_)) => button.tooltip(format!(
                "Open a shell in {} in the dock ({keys})",
                shell.pod
            )),
            (None, Subject::Workload(_)) => match &self.pods {
                // Shell lists them again, but the user should know why no pod is named.
                Pods::Failed(e) => button.tooltip(format!(
                    "Open a shell in a ready pod in the dock ({keys}). Listing the pods \
                     failed: {e}"
                )),
                _ => button.tooltip(format!("Open a shell in a ready pod in the dock ({keys})")),
            },
            // A pod still loading: Shell waits for it.
            _ => button.tooltip(format!("Open a shell in the dock ({keys})")),
        };
        let Some(ShellPod {
            pod,
            containers,
            default,
            not_running,
        }) = shell
        else {
            return Some(
                button
                    .on_click(registry::on_click(OpenShell))
                    .into_any_element(),
            );
        };
        if containers.len() < 2 {
            return Some(
                button
                    .on_click(registry::on_click(OpenShell))
                    .into_any_element(),
            );
        }
        Some(
            with_dropdown_chevron(button)
                .tooltip({
                    let hint = if not_running.contains(&default) {
                        format!("Choose a container (the default, {default}, is not running)")
                    } else {
                        picker_hint(Some(&default), &keys)
                    };
                    match self.subject {
                        Subject::Workload(_) => format!("In {pod}: {hint}"),
                        _ => hint,
                    }
                })
                .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                    menu_record::record(cx, "detail-shell", || {
                        containers
                            .iter()
                            .map(|name| {
                                let running = !not_running.contains(name);
                                menu_record::Entry::new(
                                    shell_label(name, *name == default, running),
                                    running,
                                )
                            })
                            .collect()
                    });
                    let mut menu = fit_picker(menu, window, cx).label("Container");
                    for name in &containers {
                        let running = !not_running.contains(name);
                        let (item, action) = shell_item(name, *name == default, running);
                        menu = menu.item(item.on_click(registry::on_click(action)));
                    }
                    menu
                })
                .into_any_element(),
        )
    }

    /// Why Logs or Shell cannot open, as the captions under the pane's buttons say it: one
    /// sentence ([`refused`]) for each, or one for both when the reason is the same.
    pub(in crate::ui) fn action_captions(&self) -> Vec<String> {
        if self.subject == Subject::Other {
            return Vec::new();
        }
        if let Some(why) = &self.sessions_blocked {
            return vec![refused(Opening::LogsAndShell, why)];
        }
        // The body (or the deleted notice) already says what went wrong: the caption does not
        // repeat the server's words.
        if self.subject == Subject::Pod && matches!(self.fetch, Fetch::Failed(_)) {
            return vec!["Can't open the logs or a shell until the pod is fetched.".to_owned()];
        }
        if self.gone {
            return Vec::new();
        }
        let logs = self
            .log_plan::<OpenLogs>(None, false)
            .err()
            .map(|e| actions::reason(&e));
        // A picker that still offers a running container says nothing against Shell.
        let shell = self
            .shell_check::<OpenShell>(None)
            .err()
            .filter(|_| self.shell_picker().is_none())
            .map(|e| actions::reason(&e));
        match (logs, shell) {
            (Some(logs), Some(shell)) if logs == shell => {
                vec![refused(Opening::LogsAndShell, &logs)]
            }
            (logs, shell) => logs
                .map(|why| refused(Opening::Logs, &why))
                .into_iter()
                .chain(shell.map(|why| {
                    if self.shell_protected() {
                        actions::PROTECTED_CAPTION.to_owned()
                    } else {
                        refused(Opening::Shell, &why)
                    }
                }))
                .collect(),
        }
    }

    /// The captions under the header: why Logs or Shell is disabled, in muted text, so the
    /// reason shows without hovering the button (its tooltip says it too).
    fn render_captions(&self, theme: &Theme) -> Option<AnyElement> {
        let captions = self.action_captions();
        if captions.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .id("detail-captions")
                .px_3()
                .pb_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .children(captions.into_iter().enumerate().map(|(ix, text)| {
                    let full: SharedString = text.clone().into();
                    clamped(div())
                        .id(("detail-caption", ix))
                        .min_w_0()
                        .child(text.clone())
                        .test_text(text)
                        .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
                }))
                .test_support()
                .into_any_element(),
        )
    }

    fn render_notices(&self, theme: &Theme) -> Vec<AnyElement> {
        let mut notices = Vec::new();
        if self.gone {
            notices.push(notice(
                "detail-gone",
                "This object was deleted. Showing the last copy fetched.",
                theme.warning,
            ));
        }
        if let Some(waiting) = self.waiting() {
            notices.push(notice("detail-pending", waiting, theme.info));
        }
        if self.shows_stale_notice() {
            notices.push(notice_detail(
                "detail-stale",
                self.freshness.headline(),
                sync::last_updated(self.fetched_at),
                theme.warning,
                theme.muted_foreground,
                div(),
            ));
        }
        if let Some(error) = &self.refresh_error {
            notices.push(notice_with(
                "detail-refresh-error",
                format!("Could not fetch the latest version: {error}"),
                theme.danger,
                self.render_retry(),
            ));
        }
        if self.is_secret && self.tab == DetailTab::Yaml {
            notices.push(notice(
                "detail-secret-yaml",
                "Secret values are hidden here. Reveal them one by one in Overview. In an edit, \
                 a value left hidden keeps what the cluster has.",
                theme.muted_foreground,
            ));
        }
        notices
            .into_iter()
            .map(IntoElement::into_any_element)
            .collect()
    }

    /// The YAML tab's body: the object, the edit, or the edit's diff.
    fn render_yaml_body(&self) -> AnyElement {
        match &self.edit {
            None => Editor::new(&self.yaml)
                .readonly(true)
                .bordered(false)
                .h_full()
                .into_any_element(),
            Some(edit) => match edit.phase {
                edit::Phase::Reviewed(_) | edit::Phase::Applying => div()
                    .id("yaml-edit-diff")
                    .test_support()
                    .size_full()
                    .child(Diff::new(&edit.diff))
                    .into_any_element(),
                _ => div()
                    .id("yaml-edit-editor")
                    .test_support()
                    .size_full()
                    .child(
                        Editor::new(&edit.editor)
                            // What Review sent is what its diff shows.
                            .readonly(matches!(edit.phase, edit::Phase::Reviewing))
                            .bordered(false)
                            .h_full(),
                    )
                    .into_any_element(),
            },
        }
    }

    /// What the edit's last step said, or that the object changed.
    fn render_edit_message(&self, theme: &Theme) -> Option<AnyElement> {
        let edit = self.edit.as_ref()?;
        let conflict = matches!(edit.phase, edit::Phase::Conflict);
        let competing = edit.competing_warning();
        let warned = conflict || competing.is_some();
        let text = match (&edit.error, conflict) {
            (Some(error), _) => error.clone(),
            (None, false) if competing.is_some() => competing?,
            (None, true) => "This object changed since you opened it, so nothing was applied. Reload keeps your \
             edit and shows its diff against the current object, including any change it would \
             undo."
                .to_owned(),
            (None, false) => return None,
        };
        let color = if warned { theme.warning } else { theme.danger };
        // Wrapped, not a one-line notice: it can be a server's whole rejection.
        Some(
            div()
                .id("yaml-edit-message")
                .test_support()
                .flex_none()
                .px_3()
                .py_1()
                .border_b_1()
                .border_color(color)
                .text_xs()
                .text_color(theme.foreground)
                // Every overwritten path stays reachable, and the editor keeps its room.
                .max_h(px(120.))
                .overflow_y_scroll()
                .child(text)
                .into_any_element(),
        )
    }

    /// The bar's buttons while an edit is under way.
    fn render_edit_buttons(&self, edit: &edit::YamlEdit) -> Div {
        let blocked = self.edit_blocked();
        let primary = |button: Button| match &blocked {
            Some(why) => button.disabled(true).tooltip(why.clone()),
            None => button,
        };
        let cancel = Button::new("yaml-edit-cancel")
            .xsmall()
            .ghost()
            .label("Cancel")
            .on_click(registry::on_click(CancelYamlEdit));
        let buttons = h_flex().gap_1().flex_none();
        let buttons = match edit.phase {
            edit::Phase::Editing => buttons.child(cancel).child(primary(
                Button::new("yaml-edit-review")
                    .xsmall()
                    .primary()
                    .label("Review")
                    .tooltip("Run a server dry run and show the diff; nothing is changed")
                    .on_click(registry::on_click(ReviewYamlEdit)),
            )),
            edit::Phase::Reviewing | edit::Phase::Applying => buttons.child(Spinner::new().small()),
            edit::Phase::Reviewed(_) => buttons
                .child(cancel)
                .child(
                    Button::new("yaml-edit-back")
                        .xsmall()
                        .ghost()
                        .label("Back to Edit")
                        .on_click(registry::on_click(BackToYamlEdit)),
                )
                .child(primary(
                    Button::new("yaml-edit-apply")
                        .xsmall()
                        .primary()
                        .label("Apply…")
                        .on_click(registry::on_click(ApplyYamlEdit)),
                )),
            edit::Phase::Conflict => buttons.child(cancel).child(primary(
                Button::new("yaml-edit-reload")
                    .xsmall()
                    .primary()
                    .label("Reload and Review")
                    .on_click(registry::on_click(ReloadYamlEdit)),
            )),
        };
        buttons.when(self.protected, |el| el.child(self.render_edit_unlock()))
    }

    fn render_edit_unlock(&self) -> Button {
        Button::new("yaml-edit-unlock")
            .xsmall()
            .ghost()
            .icon(FlintIcon::Lock)
            .label("Unlock…")
            .tooltip(format!(
                "Unlock {} until Flint quits, after a confirmation naming its server",
                self.object.cluster
            ))
            .on_click(registry::on_click(UnlockCluster {
                cluster: self.object.cluster.clone(),
            }))
    }

    /// Edit, disabled with its reason when it cannot run, and Unlock beside it when that is why.
    fn render_edit_button(&self) -> Div {
        let button = Button::new("yaml-edit").xsmall().ghost().label("Edit");
        let button = match self.edit_blocked() {
            Some(why) => button.disabled(true).tooltip(why),
            None => button
                .tooltip("Edit this object's YAML")
                .on_click(registry::on_click(EditYaml)),
        };
        h_flex()
            .gap_1()
            .flex_none()
            .child(button)
            .when(self.protected, |el| el.child(self.render_edit_unlock()))
    }

    /// The YAML tab's bar: the managedFields toggle, and Edit or the edit's buttons.
    fn render_yaml_bar(
        &self,
        detail: &ObjectDetail,
        theme: &Theme,
        _cx: &Context<Self>,
    ) -> impl IntoElement {
        let has_managed = detail.managed_fields.is_some();
        h_flex()
            .px_3()
            .py_1()
            .gap_2()
            .flex_none()
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(if self.edit.is_some() {
                        "Editing"
                    } else if !has_managed {
                        "No managedFields on this object."
                    } else if self.show_managed_fields {
                        "Showing metadata.managedFields."
                    } else {
                        "metadata.managedFields is hidden."
                    }),
            )
            .when(has_managed && self.edit.is_none(), |el| {
                el.child(
                    Button::new("toggle-managed-fields")
                        .xsmall()
                        .ghost()
                        .label(if self.show_managed_fields {
                            "Hide managedFields"
                        } else {
                            "Show managedFields"
                        })
                        .tooltip("Which field manager (kubectl, Helm, a controller) set each field")
                        .on_click(registry::on_click(ToggleManagedFields)),
                )
            })
            .map(|el| match &self.edit {
                Some(edit) => el.child(self.render_edit_buttons(edit)),
                None => el.child(self.render_edit_button()),
            })
    }

    fn render_overview(
        &self,
        detail: &ObjectDetail,
        o: &Overview,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let now = self.age_reference();
        let mut body = v_flex().gap_3().p_3().pb_6();

        // Metadata. Names and identifiers are monospace.
        let mut meta = Vec::new();
        meta.push(("Name", o.name.clone(), true));
        if let Some(ns) = &o.namespace {
            meta.push(("Namespace", ns.clone(), true));
        }
        meta.push(("Kind", format!("{} ({})", o.kind, o.api_version), false));
        if let Some(created) = &o.created {
            let age = timestamp_age(created, now)
                .map(|a| format!(" ({a} ago)"))
                .unwrap_or_default();
            meta.push(("Created", format!("{created}{age}"), false));
        }
        meta.push(("UID", o.uid.clone(), true));
        meta.push(("Resource version", o.resource_version.clone(), true));
        if let Some(generation) = o.generation {
            meta.push(("Generation", generation.to_string(), false));
        }
        if !o.finalizers.is_empty() {
            meta.push(("Finalizers", o.finalizers.join(", "), true));
        }
        body = body.child(section(
            "Metadata",
            None,
            v_flex().children(
                meta.into_iter()
                    .enumerate()
                    .map(|(i, (k, v, mono))| key_value(("meta", i), k, v, mono, theme)),
            ),
            theme,
        ));

        match self.usage.panel(o.namespace.as_deref(), &o.name) {
            Some(usage::UsagePanel::Hint(hint)) => {
                body = body.child(section(
                    "Usage",
                    None,
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(hint),
                    theme,
                ));
            }
            Some(usage::UsagePanel::Lines { cpu, memory }) => {
                body = body.child(section(
                    "Usage",
                    None,
                    v_flex()
                        .child(key_value(("usage", 0), "CPU", cpu, false, theme))
                        .child(key_value(("usage", 1), "Memory", memory, false, theme)),
                    theme,
                ));
            }
            None => {}
        }

        // Labels and annotations, collapsible.
        body = body.child(self.render_pairs(
            "Labels",
            &o.labels,
            self.show_labels,
            Box::new(ToggleLabels),
            theme,
        ));
        body = body.child(self.render_pairs(
            "Annotations",
            &o.annotations,
            self.show_annotations,
            Box::new(ToggleAnnotations),
            theme,
        ));

        if !o.owners.is_empty() {
            body = body.child(section(
                "Owners",
                Some(o.owners.len()),
                v_flex().gap_0p5().children(o.owners.iter().map(|owner| {
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child(owner.kind.clone()),
                        )
                        .child(
                            div()
                                .font_family(theme.mono_font_family.clone())
                                .child(owner.name.clone()),
                        )
                        .when(owner.controller, |el| {
                            el.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("controller"),
                            )
                        })
                })),
                theme,
            ));
        }

        if let Some(secret) = &o.secret {
            body = body.child(self.render_secret(secret, theme, cx));
        }

        if !o.conditions.is_empty() {
            body = body.child(section(
                "Conditions",
                Some(o.conditions.len()),
                v_flex()
                    .gap_1()
                    .children(o.conditions.iter().enumerate().map(|(i, c)| {
                        let color = self.status_color(
                            match c.status.as_str() {
                                "True" => theme.success,
                                "False" => theme.danger,
                                _ => theme.muted_foreground,
                            },
                            theme,
                        );
                        let age = timestamp_age(&c.last_transition, now);
                        v_flex()
                            .id(("condition", i))
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .text_sm()
                                    .child(super::theme::status_dot(color, true).flex_none())
                                    .child(div().font_semibold().child(c.type_.clone()))
                                    .child(div().text_color(color).child(c.status.clone()))
                                    .when(!c.reason.is_empty(), |el| {
                                        el.child(
                                            div()
                                                .text_color(theme.muted_foreground)
                                                .child(c.reason.clone()),
                                        )
                                    })
                                    .child(div().flex_1())
                                    .when_some(age, |el, age| {
                                        el.child(
                                            div()
                                                .text_xs()
                                                .font_features(tabular_figures())
                                                .text_color(theme.muted_foreground)
                                                .child(age),
                                        )
                                    }),
                            )
                            .when(!c.message.is_empty(), |el| {
                                el.child(
                                    div()
                                        .pl_3p5()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(c.message.clone()),
                                )
                            })
                    })),
                theme,
            ));
        }

        // Related events, newest first.
        let events = v_flex()
            .gap_1()
            .when_some(detail.events_error.as_ref(), |el, error| {
                el.child(
                    div()
                        .id("events-error")
                        .text_xs()
                        .text_color(theme.warning)
                        .child(format!("Events could not be read: {error}")),
                )
            })
            .when(
                detail.events.is_empty() && detail.events_error.is_none(),
                |el| {
                    el.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("No recent events."),
                    )
                },
            )
            .children(detail.events.iter().enumerate().map(|(i, e)| {
                let warning = e.type_.as_deref() == Some("Warning");
                let age = event_time(e)
                    .map(|t| human_duration(now - t.as_second()))
                    .unwrap_or_default();
                let count = e.count.filter(|c| *c > 1).map(|c| format!("×{c}"));
                v_flex()
                    .id(("event", i))
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(
                                div()
                                    .text_color(if warning {
                                        self.status_color(theme.warning, theme)
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(e.type_.clone().unwrap_or_default()),
                            )
                            .child(
                                div()
                                    .font_semibold()
                                    .child(e.reason.clone().unwrap_or_default()),
                            )
                            .children(count.map(|c| {
                                div().text_xs().text_color(theme.muted_foreground).child(c)
                            }))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_xs()
                                    .font_features(tabular_figures())
                                    .text_color(theme.muted_foreground)
                                    .child(age),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(e.message.clone().unwrap_or_default()),
                    )
            }))
            .when(detail.events_truncated, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Older events not shown."),
                )
            });
        body = body.child(section("Events", Some(detail.events.len()), events, theme));

        div()
            .id("detail-overview")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(body)
    }

    fn render_pairs(
        &self,
        title: &'static str,
        pairs: &[(String, String)],
        open: bool,
        toggle: Box<dyn Action>,
        theme: &Theme,
    ) -> impl IntoElement {
        let header = h_flex()
            .id(SharedString::from(format!("toggle-{title}")))
            .gap_1()
            .cursor_pointer()
            .text_xs()
            .font_semibold()
            .text_color(theme.muted_foreground)
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall(),
            )
            .child(format!("{} ({})", title.to_uppercase(), pairs.len()))
            .on_click(move |_, window, cx| {
                if let Err(e) = registry::perform(&*toggle, window, cx) {
                    tracing::warn!("{e}");
                }
            });
        v_flex()
            .gap_1()
            .child(header)
            .when(open && pairs.is_empty(), |el| {
                el.child(
                    div()
                        .pl_4()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("None"),
                )
            })
            .when(open, |el| {
                el.children(pairs.iter().enumerate().map(|(i, (k, v))| {
                    let full = format!("{k}: {v}");
                    h_flex()
                        .id(SharedString::from(format!("{title}-{i}")))
                        .pl_4()
                        .gap_2()
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .child(
                            div()
                                .flex_none()
                                .max_w(relative(0.5))
                                .truncate()
                                .text_color(theme.muted_foreground)
                                .child(k.clone()),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(v.clone()))
                        .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
                }))
            })
    }

    fn render_secret(
        &self,
        secret: &object::SecretData,
        theme: &Theme,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rows = secret.entries.iter().enumerate().map(|(i, entry)| {
            let revealed = self.revealed.contains(&entry.key);
            let size = entry
                .value
                .len()
                .map(|n| format!("{n} bytes"))
                .unwrap_or_else(|| "not base64".to_owned());
            let value: SharedString = match (&entry.value, revealed) {
                (_, false) => MASK.into(),
                (SecretValue::Text(text), true) => text.clone().into(),
                (SecretValue::Binary(n), true) => format!("<binary, {n} bytes>").into(),
                (SecretValue::Invalid, true) => "<not valid base64>".into(),
            };
            let key = entry.key.clone();
            h_flex()
                .id(("secret", i))
                .gap_2()
                .items_start()
                .text_sm()
                .child(
                    div()
                        .w(px(160.))
                        .flex_none()
                        .truncate()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(entry.key.clone()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("secret-value-{}", entry.key)))
                        .flex_1()
                        .min_w_0()
                        .font_family(theme.mono_font_family.clone())
                        .when(!revealed, |el| el.text_color(theme.muted_foreground))
                        .child(value),
                )
                .child(
                    div()
                        .text_xs()
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child(size),
                )
                .child(
                    Button::new(SharedString::from(format!("reveal-{}", entry.key)))
                        .icon(if revealed {
                            IconName::EyeOff
                        } else {
                            IconName::Eye
                        })
                        .xsmall()
                        .ghost()
                        .tooltip(if revealed {
                            "Hide value"
                        } else {
                            "Reveal value"
                        })
                        .on_click(registry::on_click(ToggleSecretValue { key })),
                )
        });
        section(
            "Data",
            Some(secret.entries.len()),
            v_flex()
                .gap_1()
                .when(!secret.type_.is_empty(), |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("Type: {}", secret.type_)),
                    )
                })
                .children(rows),
            theme,
        )
    }
}

/// A titled block of the Overview.
fn section(
    title: &str,
    count: Option<usize>,
    content: impl IntoElement,
    theme: &Theme,
) -> impl IntoElement {
    let title = match count {
        Some(n) => format!("{} ({n})", title.to_uppercase()),
        None => title.to_uppercase(),
    };
    v_flex()
        .gap_1p5()
        .child(
            div()
                .text_xs()
                .font_semibold()
                .text_color(theme.muted_foreground)
                .child(title),
        )
        .child(content)
}

/// One metadata line; `mono` for names and identifiers.
fn key_value(
    id: (&'static str, usize),
    key: &str,
    value: String,
    mono: bool,
    theme: &Theme,
) -> AnyElement {
    let full = value.clone();
    h_flex()
        .id(id)
        .h(px(22.))
        .gap_2()
        .text_sm()
        .child(
            div()
                .w(px(120.))
                .flex_none()
                .text_color(theme.muted_foreground)
                .child(key.to_owned()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_features(tabular_figures())
                .when(mono, |el| el.font_family(theme.mono_font_family.clone()))
                .child(value),
        )
        .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
        .into_any_element()
}

/// "5m" for an RFC 3339 timestamp five minutes ago.
/// A container as a picker lists it: `app (default)`, `migrate (init)`. The default one is
/// what the keys open (⌘L, ⇧⌘L, ⇧⌘E), so the picker says which it is.
pub(in crate::ui) fn container_label(name: &str, init: bool, default: bool) -> String {
    match (init, default) {
        (true, _) => format!("{name} (init)"),
        (false, true) => format!("{name} (default)"),
        (false, false) => name.to_owned(),
    }
}

/// A Shell picker's entry: a container that is not running (waiting, crash-looping,
/// terminated) is listed disabled, saying so.
pub(in crate::ui) fn shell_label(name: &str, default: bool, running: bool) -> String {
    match (default, running) {
        (true, true) => format!("{name} (default)"),
        (false, true) => name.to_owned(),
        (true, false) => format!("{name} (default, not running)"),
        (false, false) => format!("{name} (not running)"),
    }
}

/// A Logs picker's entry for `c` (its previous instance with `previous`) and the action it
/// runs. The default container's entry carries Logs (or Previous Logs) too, so the menu shows
/// its key beside it: the key opens that container. The caller adds the click.
pub(in crate::ui) fn log_item(
    c: &PodContainer,
    previous: bool,
) -> (PopupMenuItem, OpenContainerLogs) {
    let key: Option<Box<dyn Action>> = match (c.default, previous) {
        (true, false) => Some(Box::new(OpenLogs)),
        (true, true) => Some(Box::new(OpenPreviousLogs)),
        (false, _) => None,
    };
    let item = picker_item(container_label(&c.name, c.init, c.default), key);
    let action = OpenContainerLogs {
        container: c.name.clone(),
        previous,
    };
    (item, action)
}

/// A Shell picker's entry and the action it runs; the default container's carries Shell, so
/// its key shows. The caller adds the click.
pub(in crate::ui) fn shell_item(
    name: &str,
    default: bool,
    running: bool,
) -> (PopupMenuItem, OpenContainerShell) {
    let key: Option<Box<dyn Action>> = default.then(|| Box::new(OpenShell) as Box<dyn Action>);
    let item = picker_item(shell_label(name, default, running), key).disabled(!running);
    let action = OpenContainerShell {
        container: name.to_owned(),
    };
    (item, action)
}

fn with_dropdown_chevron(button: Button) -> Button {
    button.dropdown_caret(true)
}

/// The tooltip of a button that picks a container: the key skips the picker and opens the
/// default container.
fn picker_hint(default: Option<&str>, keys: &str) -> String {
    match default {
        Some(name) => format!("Choose a container ({keys} opens {name}, the default)"),
        None => "Choose a container".to_owned(),
    }
}

fn timestamp_age(text: &str, now: i64) -> Option<String> {
    let seconds = text
        .parse::<k8s_openapi::jiff::Timestamp>()
        .ok()?
        .as_second();
    Some(human_duration(now - seconds))
}

impl Render for DetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.follow_access(cx);
        self.finish_focus_return(window, cx);
        let theme = cx.theme().clone();
        // The pane itself has focus (Tab between panes, UP-33): ringed.
        let focused = self.focus.is_focused(window);
        let cx: &mut Context<Self> = cx;
        let content: AnyElement = match (&self.fetch, self.tab) {
            // Nothing to show from before the outage. It fetches again when the kind is back.
            (Fetch::Loading | Fetch::Failed(_), _)
                if self.freshness.is_reconnecting() && !self.gone =>
            {
                h_flex()
                    .id("detail-reconnecting")
                    .flex_1()
                    .justify_center()
                    .items_center()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Reconnecting")
                    .test_support()
                    .into_any_element()
            }
            (Fetch::Loading, _) => h_flex()
                .flex_1()
                .justify_center()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(Spinner::new().small())
                .child("Loading…")
                .into_any_element(),
            (Fetch::Failed(error), _) => v_flex()
                .flex_1()
                .min_h_0()
                .p_4()
                .gap_1()
                .items_start()
                .child(
                    div()
                        .flex_none()
                        .text_sm()
                        .text_color(theme.danger)
                        .child("Could not fetch this object"),
                )
                .child(
                    div()
                        .id("detail-fetch-error")
                        .flex_initial()
                        .min_h_0()
                        .self_stretch()
                        .overflow_y_scroll()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(error.clone()),
                )
                .child(self.render_retry())
                .into_any_element(),
            (Fetch::Loaded { detail, overview }, DetailTab::Overview) => {
                // Arc clones: the render borrows the shared data, it does not copy it.
                let (detail, overview) = (detail.clone(), overview.clone());
                self.render_overview(&detail, &overview, &theme, cx)
                    .into_any_element()
            }
            (Fetch::Loaded { detail, .. }, DetailTab::Yaml) => v_flex()
                .id("detail-yaml")
                .flex_1()
                .min_h_0()
                .child(self.render_yaml_bar(detail, &theme, cx))
                .children(self.render_edit_message(&theme))
                .child(div().flex_1().min_h_0().child(self.render_yaml_body()))
                .into_any_element(),
        };
        v_flex()
            .id("detail")
            .track_focus(&self.focus)
            .map(|el| Self::register_actions(el, cx))
            .test_support()
            .size_full()
            .min_w_0()
            .bg(theme.background)
            .relative()
            .child(self.render_header(&theme, cx))
            .children(self.render_notices(&theme))
            .child(content)
            .when(focused, |el| {
                el.child(
                    super::theme::pane_focus_ring(&theme)
                        .id("detail-focus-ring")
                        .test_support(),
                )
            })
    }
}

#[path = "detail_access.rs"]
pub(in crate::ui) mod access;

#[path = "detail_actions.rs"]
mod actions;

#[path = "detail_edit.rs"]
mod edit;

#[path = "detail_pods.rs"]
mod pods;

#[path = "detail_sync.rs"]
mod sync;

#[path = "detail_usage.rs"]
mod usage;

pub(in crate::ui) use actions::{
    Opening, PROTECTED_NOTE, ShellPod, protected_reason, reason, refused,
};
pub(in crate::ui) use pods::Pods;
#[cfg(test)]
pub(in crate::ui) use pods::RELIST_DELAY;
#[cfg(test)]
pub(in crate::ui) use pods::SHOW_DELAY as SHOW_PODS_DELAY;
#[cfg(test)]
pub(in crate::ui) use pods::TestPodLister;

#[cfg(any(test, feature = "automation"))]
#[path = "detail_state.rs"]
mod state;

#[cfg(test)]
mod tests {
    use super::{container_label, picker_hint, shell_label, timestamp_age};

    #[test]
    fn pickers_mark_the_container_the_keys_open() {
        assert_eq!(container_label("app", false, true), "app (default)");
        assert_eq!(container_label("sidecar", false, false), "sidecar");
        assert_eq!(container_label("migrate", true, false), "migrate (init)");
        assert_eq!(shell_label("app", true, true), "app (default)");
        assert_eq!(shell_label("worker", false, false), "worker (not running)");
        assert_eq!(
            picker_hint(Some("app"), "⌘L"),
            "Choose a container (⌘L opens app, the default)"
        );
    }

    #[test]
    fn ages_of_rfc3339_timestamps() {
        assert_eq!(
            timestamp_age("2026-01-01T00:00:00Z", 1_767_225_900).as_deref(),
            Some("5m")
        );
        assert_eq!(timestamp_age("not a time", 0), None);
    }
}
