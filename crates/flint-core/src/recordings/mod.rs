//! Recordings (ADR-0018): a workload's logs, events and metric samples, kept in SQLite.
//!
//! This is the storage core. [`Store`] owns `recordings.sqlite`: typed tables, owner-only files,
//! a byte budget and retention. [`Recorder`] runs a store on a thread of its own, so a caller on
//! the UI or the core's event loop only ever queues a batch, and a full queue drops it.
//!
//! Recording is off for every cluster until it is turned on for that cluster's
//! [`ConnectionIdentity`], which covers the whole connection definition. A context that is
//! re-pointed has a new identity, so it is off again and can never add to the old recording.
//!
//! Credentials are replaced before a row is written, in [`Store::append`], the one way in.

mod identity;
mod schema;
mod scrub;
mod store;
#[cfg(test)]
mod store_tests;
mod writer;

pub use identity::ConnectionIdentity;
pub use store::{
    Appended, Batch, DATABASE_FILE, EventKind, EventRow, GapReason, LogRow, RecordingError,
    RecordingId, RecordingInfo, RecordingTarget, Retention, SampleRow, Store,
};
pub use writer::{FlushError, Recorder, RecorderError, Submitted};
