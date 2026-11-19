//! Flint's Kubernetes core: kubeconfig, connections, reads, and the runtime bridge to front ends.
//!
//! This crate must never depend on GPUI (ADR-0001). Front ends talk to the long-running core
//! through [`CoreHandle`] (ADR-0002); one-shot callers such as the MCP server use the functions
//! in [`cluster`] and [`read`] directly on their own tokio runtime.

pub mod bootstrap;
pub mod cache;
pub mod cluster;
pub mod discovery;
pub mod display;
pub mod env_path;
pub mod exec;
pub mod handle;
pub mod kubeconfig;
pub mod logs;
mod manager;
pub mod paging;
pub mod read;
pub mod redact;
pub mod settings;
pub mod store;
pub mod table;
pub mod terminal;
pub mod view;
mod watch;

pub use cluster::{ClusterId, ServerVersion};
pub use discovery::{
    ClusterDiscovery, NavEntry, NavEntryError, NavSection, Navigation, SectionKind,
};
pub use exec::{ExecRequest, ExecSession, ExecSnapshot, ExecState};
pub use handle::{
    ClusterSummary, ClustersSnapshot, Command, ConnectionState, CoreConfig, CoreHandle, Event,
    LogSession, LogUpdates, Subscription, WatchTiming,
};
pub use logs::{
    LineFormat, LineId, LineSelection, LineSource, LogLine, LogLines, LogOptions, LogSearch,
    LogSince, LogSnapshot, LogState, LogSubject, LogTarget, PodSet, PodSetStatus, SearchQuery,
    Severity, StreamStatus, WorkloadKind, WorkloadTarget,
};
pub use read::{KindId, KindInfo, ObjectDetail};
pub use settings::{Appearance, Settings, WatchMode};
pub use store::{Feed, KindSnapshot, NamespaceLimit, SyncState};
pub use table::{Cell, Column, ColumnRole, ColumnType, ListFormat, Row};
pub use terminal::{TerminalSize, TerminalSnapshot};
pub use view::{SortBy, ViewQuery};
