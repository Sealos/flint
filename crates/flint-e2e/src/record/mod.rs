//! Recording the fake API server's fixtures from the kind dev cluster
//! (`scripts/record-fixtures.sh`), and comparing a fresh recording's structure with the
//! committed one ([`shape`], `fixture-drift`).

pub mod normalize;
pub mod recorder;
pub mod shape;
pub mod target;

pub use recorder::{Summary, check_destination, record};
pub use target::{DEV_CONTEXT, Target, check};
