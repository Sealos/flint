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
pub mod cluster_sets;
pub mod combined;
pub mod compare;
mod connection;
mod credential;
pub mod dashboard;
pub mod discovery;
pub mod display;
pub mod env_path;
pub mod events;
pub mod exec;
pub mod fleet;
pub mod gitops;
pub mod handle;
pub mod keep_watching;
pub mod kubeconfig;
mod logging;
pub mod logs;
mod manager;
pub mod metrics;
pub mod metrics_trend;
pub mod paging;
pub mod port_forward;
pub mod problems;
pub mod protection;
pub mod read;
mod read_only;
mod recheck;
pub mod redact;
pub mod settings;
pub mod store;
pub mod table;
pub mod telemetry;
pub mod terminal;
pub mod thresholds;
pub mod view;
mod watch;
pub mod write;
mod write_actions;

pub use alerts::{
    Alert, AlertConfig, AlertEngine, AlertEvent, AlertKey, AlertSubject, AlertSummary,
    ClusterLimitReached, ClusterSnapshot, OpenAlert, OpenAlertState,
};
pub use cluster::{ClusterId, SafeErrorText, ServerVersion};
pub use discovery::{
    ClusterDiscovery, NavEntry, NavEntryError, NavSection, Navigation, SectionKind,
};
pub use exec::{
    ExecFailure, ExecFailureKind, ExecRequest, ExecSession, ExecSnapshot, ExecState, ExecUpdates,
};
pub use handle::{
    ClusterSummary, ClustersSnapshot, Command, ConnectionProgress, ConnectionState, CoreConfig,
    CoreHandle, Event, LogSession, LogUpdates, OverviewInterest, ReloadError, Subscription,
    WatchTiming,
};
pub use logs::{
    FilterSpec, Freshness, LineFormat, LineId, LineSelection, LineSource, LogFilter, LogLine,
    LogLines, LogOptions, LogSearch, LogSince, LogSnapshot, LogState, LogSubject, LogTarget,
    PodSet, PodSetStatus, SearchQuery, Severity, StreamStatus, WorkloadKind, WorkloadPod,
    WorkloadTarget,
};
pub use metrics::{MetricsError, MetricsList, MetricsPoll, NodeMetrics, PodMetrics};
pub use problems::{
    Classifier, Problem, ProblemId, ProblemKind, ProblemOwner, ProblemReason, ProblemSeverity,
    ProblemSubject, ReplicaSetIndex,
};
pub use protection::{Protection, SessionUnlocks, WriteRefusal};
pub use read::{KindId, KindInfo, ObjectDetail};
pub use recheck::DefinitionHolds;
pub use settings::{Appearance, Settings, WatchMode};
pub use store::{Feed, KindSnapshot, NamespaceLimit, RefusalCause, SyncState};
pub use table::{Cell, Column, ColumnRole, ColumnType, ListFormat, Row};
pub use terminal::{TerminalSize, TerminalSnapshot};
pub use thresholds::{
    ThresholdError, ThresholdIssue, ThresholdLevel, ThresholdMetric, ThresholdOverrides,
    ThresholdSettings, Thresholds,
};
pub use view::{SortBy, ViewQuery};
