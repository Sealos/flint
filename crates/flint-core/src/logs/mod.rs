//! Pod and workload logs (UP-40, UP-42, UP-45): sessions that stream one container's log, or
//! every container of a workload's pods merged into one stream, into a bounded ring buffer, and
//! the models a log view needs on top of it: severity, search, and line selection.
//!
//! A front end opens a session with [`crate::CoreHandle::open_logs`] and reads coalesced
//! [`LogSnapshot`]s from the returned [`crate::LogSession`]; dropping it stops the session.
//! Sessions do not depend on which cluster is active, so they keep running across cluster
//! switches until closed.

mod budget;
mod buffer;
mod filter;
mod line;
mod pattern;
mod pods;
mod search;
mod selection;
mod session;
mod source;
mod stream;
mod workload;

pub use budget::LogBudget;
pub use buffer::{BLOCKS_PER_BUDGET, CHUNK_LINES, LogBuffer, LogLines};
pub use filter::{FilterSpec, LogFilter, MAX_CONTEXT};
pub use line::{
    LineFormat, LineId, LineSource, LogLine, MAX_LINE_BYTES, Severity, classify, strip_controls,
};
pub use pattern::{
    GUARD_SCAN_BYTES, GUARD_SCAN_LINES, MAX_PATTERN_BYTES, PatternError, ScanCancel,
};
pub use pods::{Freshness, PodSet, WorkloadKind, WorkloadPod, WorkloadTarget};
pub use search::{LogSearch, SearchMatcher, SearchQuery};
pub use selection::LineSelection;
pub use session::{
    ContainerWait, FailedHop, LogOptions, LogSince, LogSnapshot, LogState, LogSubject, LogTarget,
    MAX_STREAMS, NETWORK_FAILURES, PREVIOUS_NOT_AVAILABLE,
};
pub use workload::{PodSetStatus, StreamStatus};

pub(crate) use pods::{KubePodSource, list_workload_pods};
pub(crate) use session::{Budgeted, SessionConfig, feed, run};
pub(crate) use source::{ClientLookup, KubeLogSource, LogError};
