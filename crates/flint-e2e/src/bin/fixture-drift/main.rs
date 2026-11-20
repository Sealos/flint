//! Compares the structure of a fresh recording of the fixtures with the committed ones and fails
//! on any difference: kinds recorded, Table columns, discovery entries and fields. Values never
//! count (see `flint_e2e::record::shape`). The nightly CI job records from a fresh kind cluster
//! into a scratch directory and runs:
//!
//! ```text
//! fixture-drift <committed fixtures> <fresh recording>
//! ```
//!
//! Exit code 0: the same structure; 1: drift, with a report; 2: bad arguments or unreadable
//! fixtures.

use std::path::PathBuf;
use std::process::ExitCode;

use flint_e2e::record::shape::{Shape, report};

fn main() -> ExitCode {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let [committed, recorded] = args.as_slice() else {
        eprintln!("usage: fixture-drift <committed fixtures dir> <fresh recording dir>");
        return ExitCode::from(2);
    };
    let shapes = Shape::of(committed).and_then(|c| Ok((c, Shape::of(recorded)?)));
    let (committed_shape, recorded_shape) = match shapes {
        Ok(shapes) => shapes,
        Err(e) => {
            eprintln!("fixture-drift: {e:#}");
            return ExitCode::from(2);
        }
    };
    let lines = committed_shape.diff(&recorded_shape);
    if lines.is_empty() {
        println!(
            "no structural drift: {} matches {}",
            recorded.display(),
            committed.display()
        );
        ExitCode::SUCCESS
    } else {
        println!("{}", report(&lines));
        println!(
            "\nRe-record the fixtures (scripts/record-fixtures.sh) and update the fake API server \
             and the tests that rely on what changed (docs/TESTING.md, \"Fake API server\")."
        );
        ExitCode::from(1)
    }
}
