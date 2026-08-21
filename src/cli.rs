use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;

/// cw — fuzzy-pick a repo (or resume a worktree), clone/pull it, and launch
/// an agent CLI inside a git worktree.
#[derive(Debug, Parser)]
#[command(name = "cw", version, about)]
pub struct Cli {
    /// Worktree slug — root command only. Omitted: auto-generate a timestamp
    /// slug, or offer to resume an existing worktree of the picked repo.
    pub slug: Option<String>,

    /// Agent to launch (overrides config's default_agent)
    #[arg(long, global = true)]
    pub agent: Option<String>,

    /// Root directory repos are cloned under (overrides config's root)
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,

    /// Restrict discovery to this org (repeatable); skips org enumeration
    #[arg(long)]
    pub org: Vec<String>,

    /// Bypass cache_ttl_minutes and force a live `gh` fetch
    #[arg(long)]
    pub refresh: bool,

    /// On an already-cloned repo, skip fetch/fast-forward entirely
    #[arg(long)]
    pub no_pull: bool,

    /// Print the would-be actions without performing them (requires --repo and SLUG)
    #[arg(long)]
    pub dry_run: bool,

    /// Skip repo discovery/picker, act directly on OWNER/NAME
    #[arg(long)]
    pub repo: Option<String>,

    /// Answer confirmation prompts (e.g. first-run hook consent) affirmatively
    #[arg(long)]
    pub yes: bool,

    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Pick from all known worktrees across every repo, sorted most-recent-first
    Resume,

    /// Scan and remove worktrees interactively
    Clean {
        /// Allow removing a worktree with uncommitted changes
        #[arg(long)]
        force: bool,
    },

    /// Open a repo-less worktree for quick work with no project attached
    Scratch {
        /// Worktree slug — same auto-generate/resume rules as the default flow
        slug: Option<String>,

        /// Print the would-be actions without performing them
        #[arg(long)]
        dry_run: bool,
    },

    /// Run environment/config sanity checks (gh auth, git credential helper,
    /// terminal compat, each configured agent's binary on PATH)
    Doctor,

    /// Emit a shell completion script
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
}
