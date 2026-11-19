//! Pod logs (UP-40, UP-42): sessions that stream a container's log into a bounded ring buffer,
//! and the models a log view needs on top of it: severity, search, and line selection.
//!
//! A front end opens a session with [`crate::CoreHandle::open_logs`] and reads coalesced
//! [`LogSnapshot`]s from the returned [`crate::LogSession`]; dropping it stops the session.
//! Sessions do not depend on which cluster is active, so they keep running across cluster
//! switches until closed.

mod buffer;
mod line;
mod search;
mod selection;
mod session;
mod source;

pub use buffer::{CHUNK_LINES, LogBuffer, LogLines};
pub use line::{LineId, LogLine, MAX_LINE_BYTES, Severity, classify};
pub use search::{LogSearch, SearchQuery};
pub use selection::LineSelection;
pub use session::{LogOptions, LogSince, LogSnapshot, LogState, LogTarget};

pub(crate) use session::{SessionConfig, run};
pub(crate) use source::{ClientLookup, KubeLogSource, LogError};
