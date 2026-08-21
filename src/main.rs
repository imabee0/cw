mod agent;
mod cache;
mod clean;
mod cli;
mod config;
mod doctor;
mod github;
mod gitstatus;
mod hooks;
mod picker;
mod sync;
mod worktree;
mod worktreeinclude;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Cmd};

// Real dispatch logic (default-flow orchestration, §0a's same-repo resume
// check, §5l's interactive agent picker, --dry-run short-circuit, log-file
// init) lands in a later phase — this is just enough wiring to prove the
// full CLI surface parses correctly end to end.
fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Some(Cmd::Resume) => todo!("cw resume — implemented in a later phase"),
        Some(Cmd::Clean { .. }) => todo!("cw clean — implemented in a later phase"),
        Some(Cmd::Scratch { .. }) => todo!("cw scratch — implemented in a later phase"),
        Some(Cmd::Doctor) => todo!("cw doctor — implemented in a later phase"),
        Some(Cmd::Completions { .. }) => todo!("cw completions — implemented in a later phase"),
        None => todo!("cw default flow — implemented in a later phase"),
    }
}
