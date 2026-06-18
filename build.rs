//! Generates shell completion scripts and the roff man page as a side effect of
//! building `periodic`. Outputs land under the workspace `target/` directory so
//! cargo-dist's `include` directive can pick them up without a separate
//! generation step in the release pipeline.
//!
//! The CLI surface is reused via `#[path = "src/cli.rs"]`, so the artifacts
//! track the real `clap` definitions automatically. `cli.rs` must stay free of
//! intra-crate (`crate::…`) dependencies for this include to compile here; its
//! external crates (clap, anyhow) are mirrored in `[build-dependencies]`.

use std::env;
use std::path::PathBuf;

use clap::CommandFactory;
use clap_complete::{Shell, generate_to};
use clap_mangen::Man;

#[path = "src/cli.rs"]
#[allow(dead_code)]
mod cli;

fn main() -> std::io::Result<()> {
    // CARGO_MANIFEST_DIR is the repo root, so the build-script outputs and
    // cargo-dist's `include` entries share the same base directory.
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR must be set by cargo when running build.rs"),
    );

    let completions_dir = manifest_dir.join("target").join("completions");
    let man_dir = manifest_dir.join("target").join("man");
    std::fs::create_dir_all(&completions_dir)?;
    std::fs::create_dir_all(&man_dir)?;

    let mut cmd = cli::Cli::command();
    for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
        generate_to(shell, &mut cmd, "periodic", &completions_dir)?;
    }

    let mut buffer = Vec::new();
    Man::new(cmd).render(&mut buffer)?;
    std::fs::write(man_dir.join("periodic.1"), buffer)?;

    println!("cargo:rerun-if-changed=src/cli.rs");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
