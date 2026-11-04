# cw

`cw` fuzzy-picks a GitHub repo — your personal account plus every org you belong to, most-recently-updated first — fetches its latest default branch, creates (or resumes) a session worktree, and launches an agent CLI inside it. One command instead of remembering a clone path, pulling, inventing a worktree slug, and picking the right binary. Nothing is kept on disk for a repo once its last session is finished.

## Install

**macOS (Apple silicon) or Linux (x86_64), one line** — installs the newest release to `~/.local/bin`, verifying its SHA256 first; re-run it to upgrade:

```bash
curl -fsSL https://raw.githubusercontent.com/imabee0/cw/main/install/cw.sh | bash
```

Releases are tagged `cw-vX.Y.Z`. A 0.1.x or 0.2.0 install (the old `cw-installer.sh` into `~/.cargo/bin`) can't self-update to them: run the line above once, then remove the old `~/.cargo/bin/cw`.

**From source** (an unreleased change, or a platform without a prebuilt binary):

```bash
git clone https://github.com/imabee0/cw.git
cd cw
cargo install --path . --locked
```

Either way requires the [`gh`](https://cli.github.com) CLI, already authenticated (`gh auth login`), and `git` on `PATH`. `cw` uses `gh` for GitHub access and the first clone of a repo, and `git` for every fetch after that.

## Usage

```bash
cw                           # pick a repo (or an existing session) in the dashboard
cw my-feature-slug           # explicit slug, skips the resume picker
cw --agent grok my-slug      # launch grok instead of default_agent
cw --repo owner/name my-slug # skip the repo picker, act on a named repo directly
```

`cw`, `cw resume`, and `cw clean` open the dashboard: a repo pane (bare `cw` only) and a worktree pane listing every session across every repo, with its age and whether it holds changed files or unpushed commits. Enter (or a double-click) on a session resumes it; on a repo, or `n`, starts a new session. Click an agent in the footer (or `←`/`→`, or Ctrl-A) to choose claude/codex/grok/opencode/shell. Clone/fetch and launch failures open an error modal (also written to `~/.cache/cw/cw.log.<date>`); an agent that exits nonzero (Ctrl-C, a failed last shell command) is only noted in the status line — the session is kept either way.

**Keys** — `j`/`k` or arrows move, Tab switches pane, `←`/`→` cycle the agent. Typing filters the focused pane; while its filter is empty, `q` quits, `n` starts a new session, `r` rescans, `u` applies a pending self-update, and in the worktree pane `Space` marks a row and `d` opens the removal confirm (marked rows, or the focused row if none are marked). Press `/` to search with every letter, shortcuts and `j`/`k` included; Enter or Tab keeps that filter, Esc clears it. Outside search, `Esc` clears the status, then the filter, then the marks, then quits.

**Mouse** — a click on a row focuses it; a second click on the same row within 400ms opens it (any key or scroll in between resets that). Clicks on the agent footer, the help line's keys, the mark column, and modal buttons act directly. The wheel scrolls the pane under the pointer.

**Removing sessions** — in the confirm, `y` removes; sessions with changed files or unpushed commits are kept unless you press `f` to include them (`cw clean --force` starts with them included). See [Storage](#storage) for what removal deletes.

### Commands

- `cw resume` — open straight on the worktree pane, skipping the repo pane.
- `cw finish owner/name SLUG` — finish one session non-interactively: removes it if everything is committed and pushed, refuses otherwise (or while its agent is still running), and deletes the repo's store when it was the last session.
- `cw clean` / `cw clean --force` — the dashboard, for reviewing and removing finished sessions.
- `cw scratch [SLUG]` — a session with no project attached, in a local repo at `root/.scratch/workspace` (kept; it has no remote to fetch from).
- `cw doctor` — check `gh` auth, `git` on `PATH`, the git credential helper, the terminal, install source and update status, and each configured agent; only a missing `default_agent` (or a failed environment check) fails the exit code.
- `cw completions zsh` — emit a shell completion script; install via a directory on `fpath` (e.g. `mkdir -p ~/.zfunc && cw completions zsh > ~/.zfunc/_cw`, with `fpath=(~/.zfunc $fpath)` placed *before* `compinit` in `~/.zshrc`). Appending straight into `~/.zshrc` also works only if that file calls `compinit` earlier in the same file — `fpath` avoids the ordering trap.

### Flags

- `--agent NAME` — launch this agent instead of `default_agent`.
- `--repo OWNER/NAME` — skip the repo pane and act on this repo.
- `--org ORG` (repeatable) — show only repos owned by these orgs/users. Discovery and its cache always cover everything; this only filters the view.
- `--refresh` — ignore `cache_ttl_minutes` and re-list repos from GitHub now.
- `--no-pull` — don't fetch an existing store; a new session branches from what was fetched last time.
- `--yolo` (alias `--skip-permissions`) — launch Codex/Claude with their permission prompts skipped (`--dangerously-bypass-approvals-and-sandbox` / `--dangerously-skip-permissions`) for this invocation only; other agents are unaffected. For a permanent setting, put the flag in that agent's `args`.
- `--yes` — accept hook-consent prompts.
- `--root PATH` — use this directory for session stores instead of `root`.
- `--dry-run` (with `--repo` and a SLUG) — print the clone/fetch, worktree, hook, and agent decisions without changing anything.

## Storage

Nothing is cloned ahead of time — browsing only caches repo *metadata* (`~/.cache/cw/repos.json`). The first session in a repo clones it as a **bare store** (git history only, no checkout of its own) at `root/owner/name`; each session is a worktree of that store at `root/owner/name/.claude/worktrees/<slug>`, on branch `worktree-<slug>`, started from the remote's default branch as fetched at that moment. Concurrent sessions in one repo share one store.

Removing a session — `cw finish`, or `d` in the dashboard — deletes its worktree and branch, and the store too once no sessions are left, so the disk holds exactly the repos you have open sessions in. Removal re-checks the session at that moment rather than trusting what the dashboard showed, and refuses one whose agent is still running (a lock left behind by a crashed `cw` is cleared automatically).

`root` defaults to `$XDG_DATA_HOME/cw/sessions` (`~/.local/share/cw/sessions`): on disk, not `/tmp`, which is often RAM-backed and wiped on reboot — a session must survive both. Versions before 0.2 kept full clones in `~/repos`; finish those sessions with the old version first, as `cw` no longer looks there.

Inside a session, git behaves like a normal clone: `origin/*` is fetched, `git switch <branch>` tracks `origin/<branch>`, and a plain `git push` publishes `worktree-<slug>`. A pushed branch keeps counting as saved even after the remote deletes it on merge. Fetches use system `git` (SSH remotes and credential helpers work) and never prompt.

## Config

`~/.config/cw/config.toml` — optional, never written by `cw`; every field is optional. Shown at the defaults:

```toml
root = "~/.local/share/cw/sessions"   # $XDG_DATA_HOME/cw/sessions when that's set
cache_ttl_minutes = 15                # how long the repo list is reused before re-listing
default_agent = "claude"
idle_threshold_days = 14              # sessions untouched this long are flagged idle

# Dirs shared by every session of a repo instead of reinstalled per session,
# e.g. ["node_modules"]. The shared copy lives in the repo's store, is
# populated by the first session's install, and is deleted with the store.
# Empty by default — a shared dir is mutable state across sessions.
symlink_dirs = []

# Both hooks run code the CLONED REPO supplies — off by default, confirmed
# once per repo before the first run (or --yes to skip the prompt). Both run
# inside the new session worktree.
# post_clone_hook = "scripts/setup.sh"   # first session after a fresh clone only
# post_create_hook = "auto"              # every new session; or a repo-relative script.
#                                        # "auto" runs cargo build / npm ci / pnpm install
#                                        # for Cargo.lock / package-lock.json / pnpm-lock.yaml

[agents.claude]
cmd = "claude"
args = []

[agents.codex]
cmd = "codex"
args = []

[agents.grok]
cmd = "grok"
args = []

[agents.opencode]
cmd = "opencode"
args = []

[agents.shell]
cmd = "$SHELL"
args = []
```

**`.worktreeinclude`** — a gitignore-syntax file in the repo listing untracked local files (like `.env`) to copy into each new session. They're copied from the repo's most recently used other session; a file the new session tracks is never overwritten. With no other open session there is nothing to copy from.

## Self-update

`cw` checks for a newer release in the background (at most once a day) and, once it finds one, shows an "update available" notice in the dashboard footer — press `u` to download it with `gh`, verify its SHA256, and replace the running binary in place. Only release builds (the install script above) update themselves; a from-source build silently skips the check — reinstall it from source, or switch to the install script. `cw doctor` reports which kind you have.

## Development

```bash
scripts/check          # fast: fmt, hakari, clippy and tests for the crates your change affects
scripts/check gate     # everything CI's merge queue checks that this host can: deny, commit lint, doctests, MSRV, cross-target checks
scripts/check build    # release binaries + SHA256SUMS in dist/cw/<target>/ (does not run tests)
```

`.github/workflows/ci.yml` is the whole pipeline: pull requests and the merge queue validate, and every push to `main` with a `feat`/`fix`/`perf` (or breaking) commit is released — the version is bumped from conventional commits since the last `cw-vX.Y.Z` tag, built for both targets, tagged, and published as a GitHub Release. Nothing commits back to `main`.

## License

MIT — see [LICENSE](LICENSE).
