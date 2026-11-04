use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;

/// cw — fuzzy-pick a repo (or resume a session), fetch its latest default
/// branch into an on-demand store, and launch an agent CLI in a fresh git
/// worktree. Finishing a repo's last session deletes the store.
#[derive(Debug, Parser)]
#[command(name = "cw", version, about)]
pub struct Cli {
    /// Worktree slug — root command only. Omitted: auto-generate a timestamp
    /// slug, or offer to resume an existing worktree of the picked repo.
    pub slug: Option<String>,

    /// Agent to launch (overrides config's default_agent)
    #[arg(long, global = true)]
    pub agent: Option<String>,

    /// Directory session stores live under (overrides config's root)
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,

    /// Show only repos owned by this org or user (repeatable)
    #[arg(long)]
    pub org: Vec<String>,

    /// Bypass cache_ttl_minutes and force a live `gh` fetch
    #[arg(long)]
    pub refresh: bool,

    /// Skip fetching an existing store; new sessions branch from what's already fetched
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

    /// Skip agent permission checks for Codex and Claude
    #[arg(long, global = true, alias = "skip-permissions")]
    pub yolo: bool,

    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Pick from all known worktrees across every repo, sorted most-recent-first
    Resume,

    /// Remove one session whose work is committed and pushed; deletes the
    /// repo's local store when it was the last session
    Finish {
        /// Repository in OWNER/NAME form
        repo: String,
        /// Session slug
        slug: String,
    },

    /// Review every session and remove finished ones interactively
    Clean {
        /// Start with sessions holding changed files or unpushed commits included
        /// in the removal (toggle with `f` in the confirm)
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

    /// Run environment/config sanity checks (gh auth, git on PATH, git
    /// credential helper, terminal compat, install source/update status,
    /// each configured agent's binary on PATH — only default_agent fails the
    /// exit code if missing)
    Doctor,

    /// Emit a shell completion script
    Completions {
        /// Shell to generate completions for
        shell: Shell,
    },
}
