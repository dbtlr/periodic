//! `periodic self-update` — in-binary updater backed by axoupdater, fetching
//! from the GitHub release channel the dist pipeline publishes. Stable by
//! default, the `-next` prerelease channel with `--next`, or a specific tag.
//! axoupdater downloads the matching installer, verifies it, and swaps the
//! running binary atomically (via self-replace).

use anyhow::{Context, Result};
use axoupdater::{AxoUpdater, UpdateRequest};

/// Run the self-update. `next` opts into the prerelease channel; `tag` pins a
/// specific release tag (mutually exclusive with `next`).
pub(crate) fn run(next: bool, tag: Option<String>) -> Result<()> {
    let mut updater = AxoUpdater::new_for("periodic");
    updater.load_receipt().context(
        "reading the install receipt; periodic must be installed via the official installer to self-update",
    )?;

    let specifier = match tag {
        Some(tag) => UpdateRequest::SpecificTag(tag),
        None if next => UpdateRequest::LatestMaybePrerelease,
        None => UpdateRequest::Latest,
    };
    updater.configure_version_specifier(specifier);

    match updater.run_sync().context("performing the update")? {
        Some(result) => {
            println!("updated periodic to {}", result.new_version);
            tracing::info!(version = %result.new_version, "self-update complete");
        }
        None => println!("periodic is already up to date"),
    }
    Ok(())
}
