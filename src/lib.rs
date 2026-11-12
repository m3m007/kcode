#![allow(
    unknown_lints,
    clippy::collapsible_match,
    clippy::manual_checked_ops,
    clippy::unnecessary_sort_by,
    clippy::useless_conversion
)]
// Tests hold the process-wide env lock across awaits on purpose.
#![cfg_attr(test, allow(clippy::await_holding_lock))]

//! Root `kcode` crate: the entrypoint + cli layer on top of the `kcode-tui`
//! presentation crate (which in turn re-exports `kcode-app-core` and
//! `kcode-base`).
//!
//! The presentation modules (`tui`, `video_export`) live in `kcode-tui` and the
//! non-presentation modules live in `kcode-app-core`; both are re-exported here
//! via `pub use kcode_tui::*`, so existing `crate::<module>` paths (e.g.
//! `crate::config`, `crate::server`, `crate::tui`) keep resolving unchanged
//! across the cli code that was not moved.

// Re-export the presentation layer (and, transitively, the application core)
// so `crate::tui`, `crate::video_export`, and `crate::<app-core module>` paths
// resolve.
pub use kcode_tui::*;

// Cli + entrypoint layer (kept in the root crate).
pub mod cli;

use anyhow::Result;

fn register_bundled_docs() {
    tool::register_bundled_docs(kcode_docs_data::documents());
}

pub async fn run() -> Result<()> {
    cli::startup::run().await
}

#[cfg(test)]
mod bundled_docs_tests {
    #[test]
    fn executable_registers_its_version_matched_corpus() {
        // app-core is a normal dependency here, so this checks the production
        // lookup path, not app-core's cfg(test) corpus provider.
        super::register_bundled_docs();
        let actual = super::tool::bundled_docs().unwrap();
        assert_eq!(actual, kcode_docs_data::documents());
        assert!(actual.iter().any(|(path, _)| *path == "README.md"));
        super::register_bundled_docs();
        assert_eq!(super::tool::bundled_docs().unwrap(), actual);
    }
}
