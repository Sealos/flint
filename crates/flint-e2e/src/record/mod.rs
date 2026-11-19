//! Recording the fake API server's fixtures from the kind dev cluster
//! (`scripts/record-fixtures.sh`).

pub mod normalize;
pub mod recorder;
pub mod target;

pub use recorder::{Summary, record};
pub use target::{DEV_CONTEXT, Target, check};
