use std::collections::HashMap;
use std::io::IsTerminal;
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use skim::prelude::*;

use crate::config::AgentConfig;
use crate::github::Repo;
use crate::gitstatus;
use crate::worktree::{self, WorktreeEntry as ScannedEntry};

/// Outcome of any interactive pick: a real selection, an empty source list
/// (message already printed, skim never invoked — §5j), or the user
/// cancelling out of skim (Esc/Ctrl-C). Distinguishing `Empty` from
/// `Cancelled` lets callers skip printing their own redundant message for
/// the former while still reacting distinctly if they ever need to.
pub enum Pick<T> {
    Selected(T),
    Empty,
    Cancelled,
}

/// One of the two rows `pick_worktree`/`pick_worktrees_multi` can return:
/// a real, previously-created worktree, or the synthetic "+ new worktree"
/// row (§0a) offered only when the caller passes `include_new: true`.
#[derive(Debug, Clone)]
pub enum WorktreeSelection {
    Existing(ScannedEntry),
    New,
}

/// One worktree selected out of `cw clean`'s multi-select picker, plus the
/// dirty flag `picker.rs` already computed while building the annotated
/// list — so `clean.rs` never needs to re-open the repo itself just to
/// decide whether `--force` is required.
#[derive(Debug, Clone)]
pub struct CleanCandidate {
    pub entry: ScannedEntry,
    pub dirty: bool,
}

/// Gate on `/dev/tty`, not `stdin().is_terminal()` (F35 — skim renders
/// straight to `/dev/tty`, not through stdin/stdout, so a piped stdin
/// plausibly still reaches a real controlling terminal; the case that
/// actually matters is no controlling terminal at all, e.g. a workflow
/// agent). `stderr().is_terminal()` is checked too: skim's own status/error
/// output goes there.
pub fn is_interactive() -> bool {
    std::fs::File::open("/dev/tty").is_ok() && std::io::stderr().is_terminal()
}

/// Formats an idle duration as `"idle Nd"`, or `None` when under
/// `threshold_days` — informational only, baked directly into a
/// `WorktreeEntry`'s `text()` (F36), never computed inside a `display()`
/// override. `now` is a parameter (not `SystemTime::now()` inline) so this
/// stays testable without real elapsed time.
pub fn humanize(mtime: SystemTime, now: SystemTime, threshold_days: u64) -> Option<String> {
    let days = now
        .duration_since(mtime)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    (days >= threshold_days).then(|| format!("idle {days}d"))
}

/// `.scratch/workspace` (§5n) displays as `scratch` — display-label only,
/// never affects the underlying repo/slug values `remove_worktree`/
/// `create_or_resume_worktree` operate on.
fn display_repo_label(repo: &str) -> String {
    if repo == format!("{}/{}", worktree::SCRATCH_OWNER, worktree::SCRATCH_REPO) {
        "scratch".to_string()
    } else {
        repo.to_string()
    }
}

/// A repo, ready for the fuzzy-picker. No annotation needed — recency
/// ordering is already applied by the caller before this is built.
struct RepoEntry {
    repo: Repo,
    text: String,
}

impl RepoEntry {
    fn new(repo: Repo) -> Self {
        let text = repo.full_name();
        Self { repo, text }
    }
}

impl SkimItem for RepoEntry {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.text)
    }
}

/// A worktree (or the synthetic "+ new worktree" row), annotated with
/// idle/dirty state directly in `text()` — e.g. `owner/repo/slug   idle 12d
/// [dirty]` — per §5l. Shared by `pick_worktree` (single-select) and
/// `pick_worktrees_multi` (`cw clean`'s multi-select) so the annotation
/// logic exists in exactly one place.
struct WorktreeEntry {
    selection: WorktreeSelection,
    dirty: bool,
    text: String,
}

impl WorktreeEntry {
    fn existing(entry: ScannedEntry, idle_threshold_days: u64, now: SystemTime) -> Self {
        // Best-effort: a worktree whose status can't be read (permissions, a
        // race with concurrent deletion) is annotated as clean rather than
        // aborting the whole picker over one entry — same tolerance idiom
        // `scan_worktrees` itself uses.
        let dirty = gitstatus::is_dirty(&entry.path).unwrap_or(false);
        let mut text = format!("{}/{}", display_repo_label(&entry.repo), entry.slug);
        if let Some(idle) = humanize(entry.mtime, now, idle_threshold_days) {
            text.push_str("   ");
            text.push_str(&idle);
        }
        if dirty {
            text.push_str("   [dirty]");
        }
        Self {
            selection: WorktreeSelection::Existing(entry),
            dirty,
            text,
        }
    }

    fn new_row() -> Self {
        Self {
            selection: WorktreeSelection::New,
            dirty: false,
            text: "+ new worktree".to_string(),
        }
    }
}

impl SkimItem for WorktreeEntry {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.text)
    }
}

/// An agent, annotated with its resolved command line so the picker doubles
/// as a reminder of what each name actually launches.
struct AgentEntry {
    name: String,
    text: String,
}

impl AgentEntry {
    fn new(name: String, cfg: &AgentConfig) -> Self {
        let mut text = name.clone();
        if !cfg.cmd.is_empty() {
            text.push_str("   ");
            text.push_str(&cfg.cmd);
            for arg in &cfg.args {
                text.push(' ');
                text.push_str(arg);
            }
        }
        Self { name, text }
    }
}

impl SkimItem for AgentEntry {
    fn text(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.text)
    }
}

fn build_skim_options(multi: bool) -> Result<SkimOptions> {
    SkimOptionsBuilder::default()
        .multi(multi)
        .build()
        .map_err(|e| anyhow!("building skim options: {e}"))
}

/// `Skim::run_items` returns an `eyre::Result` internally to skim — `eyre`
/// isn't (and shouldn't become) a direct dependency of cw just to name that
/// type, so the conversion is a plain `Display`-based `map_err` rather than
/// a `From`/`?` chain.
fn run_skim<T: SkimItem>(options: SkimOptions, items: Vec<T>) -> Result<SkimOutput> {
    Skim::run_items(options, items).map_err(|e| anyhow!("running picker: {e}"))
}

pub fn pick_repo(repos: Vec<Repo>) -> Result<Pick<Repo>> {
    if repos.is_empty() {
        println!("no repos found — check `gh auth status` or your --org filter");
        return Ok(Pick::Empty);
    }
    if !is_interactive() {
        return Err(anyhow!(
            "no interactive terminal available to pick a repo — pass --repo OWNER/NAME instead"
        ));
    }

    let items: Vec<RepoEntry> = repos.into_iter().map(RepoEntry::new).collect();
    let output = run_skim(build_skim_options(false)?, items)?;
    if output.is_abort {
        return Ok(Pick::Cancelled);
    }
    let Some(matched) = output.selected_items.first() else {
        return Ok(Pick::Cancelled);
    };
    let entry = matched
        .downcast_item::<RepoEntry>()
        .ok_or_else(|| anyhow!("picker returned an unexpected item type"))?;
    Ok(Pick::Selected(entry.repo.clone()))
}

fn build_worktree_items(
    entries: Vec<ScannedEntry>,
    idle_threshold_days: u64,
) -> Vec<WorktreeEntry> {
    let now = SystemTime::now();
    entries
        .into_iter()
        .map(|e| WorktreeEntry::existing(e, idle_threshold_days, now))
        .collect()
}

/// Single-select worktree picker. `include_new: true` appends the "+ new
/// worktree" row (§0a's default-flow existing-worktrees-first check);
/// `false` (`cw resume`) offers only real worktrees.
pub fn pick_worktree(
    entries: Vec<ScannedEntry>,
    idle_threshold_days: u64,
    include_new: bool,
) -> Result<Pick<WorktreeSelection>> {
    if entries.is_empty() {
        println!("no worktrees yet");
        return Ok(Pick::Empty);
    }
    if !is_interactive() {
        return Err(anyhow!(
            "no interactive terminal available to pick a worktree"
        ));
    }

    let mut items = build_worktree_items(entries, idle_threshold_days);
    if include_new {
        items.push(WorktreeEntry::new_row());
    }
    let output = run_skim(build_skim_options(false)?, items)?;
    if output.is_abort {
        return Ok(Pick::Cancelled);
    }
    let Some(matched) = output.selected_items.first() else {
        return Ok(Pick::Cancelled);
    };
    let entry = matched
        .downcast_item::<WorktreeEntry>()
        .ok_or_else(|| anyhow!("picker returned an unexpected item type"))?;
    let selection = match &entry.selection {
        WorktreeSelection::Existing(e) => WorktreeSelection::Existing(e.clone()),
        WorktreeSelection::New => WorktreeSelection::New,
    };
    Ok(Pick::Selected(selection))
}

/// Multi-select worktree picker backing `cw clean` — never offers the
/// synthetic "+ new worktree" row (nothing to "clean" about creating one).
pub fn pick_worktrees_multi(
    entries: Vec<ScannedEntry>,
    idle_threshold_days: u64,
) -> Result<Pick<Vec<CleanCandidate>>> {
    if entries.is_empty() {
        println!("no worktrees yet");
        return Ok(Pick::Empty);
    }
    if !is_interactive() {
        return Err(anyhow!(
            "no interactive terminal available to pick worktrees to clean"
        ));
    }

    let items = build_worktree_items(entries, idle_threshold_days);
    let output = run_skim(build_skim_options(true)?, items)?;
    if output.is_abort || output.selected_items.is_empty() {
        return Ok(Pick::Cancelled);
    }

    let mut candidates = Vec::with_capacity(output.selected_items.len());
    for matched in &output.selected_items {
        let entry = matched
            .downcast_item::<WorktreeEntry>()
            .ok_or_else(|| anyhow!("picker returned an unexpected item type"))?;
        if let WorktreeSelection::Existing(scanned) = &entry.selection {
            candidates.push(CleanCandidate {
                entry: scanned.clone(),
                dirty: entry.dirty,
            });
        }
    }
    Ok(Pick::Selected(candidates))
}

/// Picks an agent by name out of `config.toml`'s `[agents]` table (§5l).
/// Names are sorted before display so output is deterministic run to run,
/// not dependent on `HashMap` iteration order.
pub fn pick_agent(agents: &HashMap<String, AgentConfig>) -> Result<Pick<String>> {
    if agents.is_empty() {
        println!("no agents configured — check [agents] in config.toml");
        return Ok(Pick::Empty);
    }
    if !is_interactive() {
        return Err(anyhow!(
            "no interactive terminal available to pick an agent — pass --agent NAME instead"
        ));
    }

    let mut names: Vec<&String> = agents.keys().collect();
    names.sort();
    let items: Vec<AgentEntry> = names
        .into_iter()
        .map(|name| AgentEntry::new(name.clone(), &agents[name]))
        .collect();

    let output = run_skim(build_skim_options(false)?, items)?;
    if output.is_abort {
        return Ok(Pick::Cancelled);
    }
    let Some(matched) = output.selected_items.first() else {
        return Ok(Pick::Cancelled);
    };
    let entry = matched
        .downcast_item::<AgentEntry>()
        .ok_or_else(|| anyhow!("picker returned an unexpected item type"))?;
    Ok(Pick::Selected(entry.name.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn idle_annotation_formatting() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(20 * 86_400);
        let recent = now - Duration::from_secs(2 * 86_400);

        assert_eq!(humanize(old, now, 14), Some("idle 20d".to_string()));
        assert_eq!(humanize(recent, now, 14), None);
        // Boundary: exactly at the threshold counts as idle ("N >= threshold").
        assert_eq!(humanize(old, now, 20), Some("idle 20d".to_string()));
        assert_eq!(humanize(old, now, 21), None);
    }

    #[test]
    fn display_repo_label_maps_scratch() {
        assert_eq!(display_repo_label(".scratch/workspace"), "scratch");
        assert_eq!(display_repo_label("imabee0/cw"), "imabee0/cw");
    }
}
