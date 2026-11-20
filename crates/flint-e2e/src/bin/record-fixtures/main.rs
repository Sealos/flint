//! Records the fake API server's fixtures from the kind dev cluster into
//! `crates/flint-e2e/fixtures/`, or into the directory given as the only argument, which must be
//! new or empty. Run it through `scripts/record-fixtures.sh`. It refuses to run unless
//! `KUBECONFIG` names the dev cluster's kubeconfig and its current context is `kind-flint-dev`.

use std::path::PathBuf;

use anyhow::Context as _;
use flint_e2e::fake_api::fixtures::FIXTURES_DIR;
use flint_e2e::record;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let target = record::check(std::env::var_os("KUBECONFIG").as_deref())?;
    let out = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from(FIXTURES_DIR), PathBuf::from);
    record::check_destination(&out, std::path::Path::new(FIXTURES_DIR))?;
    let client = flint_core::cluster::connect(&target.loaded.merged, record::DEV_CONTEXT)
        .await
        .context("connecting to the dev cluster")?;
    eprintln!(
        "recording from {} ({}) into {}",
        record::DEV_CONTEXT,
        target.path.display(),
        out.display()
    );
    let summary = record::record(&client, out).await?;
    let total: usize = summary.files.values().sum();
    for (path, size) in &summary.files {
        println!("{size:>9}  {}", path.display());
    }
    println!("{total:>9}  total in {} files", summary.files.len());
    Ok(())
}
