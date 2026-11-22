//! Flint's Kubernetes core: kubeconfig, connections, reads, and the runtime bridge to front ends.
//!
//! This crate must never depend on GPUI (ADR-0001). Front ends talk to the long-running core
//! through [`CoreHandle`] (ADR-0002); one-shot callers such as the MCP server use the functions
//! in [`cluster`] and [`read`] directly on their own tokio runtime.

pub mod access;
pub mod activity;
pub mod alerts;
pub mod api_error;
mod auth_plugin;
pub mod bootstrap;
pub mod cache;
pub mod cluster;
mod connection;
mod credential;
pub mod discovery;
pub mod display;
pub mod env_path;
pub mod events;
pub mod exec;
pub mod gitops;
pub mod handle;
pub mod kubeconfig;
mod logging;
pub mod logs;
mod manager;
pub mod metrics;
pub mod paging;
pub mod problems;
pub mod protection;
pub mod read;
mod read_only;
mod recheck;
pub mod redact;
pub mod settings;
pub mod store;
pub mod table;
pub mod terminal;
pub mod thresholds;
pub mod view;
mod watch;
pub mod write;
mod write_actions;

pub use alerts::{
    Alert, AlertConfig, AlertEngine, AlertEvent, AlertKey, AlertOwner, AlertSubject, AlertSummary,
    ClusterLimitReached, ClusterSnapshot, Observation, OpenAlert, OpenAlertState,
};
pub use cluster::{ClusterId, SafeErrorText, ServerVersion};
pub use discovery::{
    ClusterDiscovery, NavEntry, NavEntryError, NavSection, Navigation, SectionKind,
};
pub use exec::{
    ExecFailure, ExecFailureKind, ExecRequest, ExecSession, ExecSnapshot, ExecState, ExecUpdates,
};
pub use handle::{
    ClusterSummary, ClustersSnapshot, Command, ConnectionState, CoreConfig, CoreHandle, Event,
    LogSession, LogUpdates, ReloadError, Subscription, WatchTiming,
};
pub use logs::{
    FilterSpec, Freshness, LineFormat, LineId, LineSelection, LineSource, LogFilter, LogLine,
    LogLines, LogOptions, LogSearch, LogSince, LogSnapshot, LogState, LogSubject, LogTarget,
    PodSet, PodSetStatus, SearchQuery, Severity, StreamStatus, WorkloadKind, WorkloadPod,
    WorkloadTarget,
};
pub use metrics::{MetricsError, MetricsList, MetricsPoll, NodeMetrics, PodMetrics};
pub use problems::{
    Classifier, Problem, ProblemId, ProblemKind, ProblemReason, ProblemSeverity, ProblemSubject,
};
pub use protection::{Protection, SessionUnlocks, WriteRefusal};
pub use read::{KindId, KindInfo, ObjectDetail};
pub use settings::{Appearance, Settings, WatchMode};
pub use store::{Feed, KindSnapshot, NamespaceLimit, RefusalCause, SyncState};
pub use table::{Cell, Column, ColumnRole, ColumnType, ListFormat, Row};
pub use terminal::{TerminalSize, TerminalSnapshot};
pub use thresholds::{
    ThresholdError, ThresholdIssue, ThresholdLevel, ThresholdMetric, ThresholdOverrides,
    ThresholdSettings, Thresholds,
};
pub use view::{SortBy, ViewQuery};
