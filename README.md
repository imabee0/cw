# cw

`cw` fuzzy-picks a GitHub repo — your personal account plus every org you belong to, most-recently-updated first — fetches it, creates (or resumes) a session worktree on the latest default branch, and launches an agent CLI inside it. One command instead of remembering the clone path, pulling, inventing a worktree slug, and picking the right binary.

## Install

**macOS or Linux, one line** (installs the latest release to `~/.cargo/bin`, verifying a checksum first):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/imabee0/cw/releases/latest/download/cw-installer.sh | sh
```

**From source** (an unreleased change, or a platform without a prebuilt binary):

```bash
git clone https://github.com/imabee0/cw.git
cd cw
cargo install --path . --locked
```

Either way requires the [`gh`](https://cli.github.com) CLI, already authenticated (`gh auth login`) — `cw` shells out to it for all GitHub access.

## Config

`~/.config/cw/config.toml` (created empty on first run; all fields optional — shown here at their defaults):

```toml
root = "~/.local/share/cw/sessions"   # $XDG_DATA_HOME/cw/sessions if set
cache_ttl_minutes = 15
default_agent = "claude"
idle_threshold_days = 14

# Dirs symlinked from the repo's directory under `root` into every new
# worktree instead of reinstalling (repos are bare stores, so only dirs you
# put there yourself are linked). Empty by default — a shared node_modules is mutable state
# across worktrees. Opt in per-repo:
symlink_dirs = []

# Both hooks run code the CLONED REPO supplies — off by default, confirmed
# once per repo before the first run (or --yes to skip the prompt):
# post_clone_hook = "scripts/setup.sh"       # fresh clones only; runs in the first session worktree
# post_create_hook = "auto"                  # every new worktree; or a script path; "auto" sniffs
#                                             # Cargo.lock/package-lock.json/pnpm-lock.yaml

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

## Usage

**Default flow** — pick a repo, fetch it, create or resume a worktree, launch the selected agent. `cw` (bare), `cw resume`, and `cw clean` all open on a worktree pane listing every worktree across every repo. Click a row to focus it, double-click (or Enter) to open it; click an agent in the footer (or `←`/`→`) to choose claude/codex/grok/opencode/shell. Every pane supports arrow keys/`j`/`k`, live fuzzy filtering, and mouse click/scroll. In the worktree pane: `Space` marks a row, `d` opens a removal confirm (marked rows, or the focused row if none are marked). Confirm with `y`; worktrees with changed files or unpushed commits are kept unless you press `f` first (or started with `cw clean --force`). `r` rescans, `u` applies a pending self-update, `Esc` clears the status, then the filter, then the marked set, then cancels. Clone/pull and launch failures open an error modal (also written to `~/.cache/cw/cw.log.<date>`):

```bash
cw
cw my-feature-slug          # explicit slug, skips the resume picker
cw --agent grok my-slug      # launch grok instead of default_agent
cw --repo owner/name my-slug # skip the repo picker, act on a named repo directly
```

- `cw resume` — open straight on the worktree pane (see above), skipping the repo pane entirely.
- `cw scratch [SLUG]` — a real worktree with no project attached, for quick work that doesn't belong to a repo.
- `cw finish owner/name SLUG` — finish one session: removes its worktree once everything is committed and pushed (refuses otherwise, and while its agent is still running), and deletes the repo's local store when that was its last session.
- `cw clean` / `cw clean --force` — remove finished worktrees (STATUS shows changed files / unpushed commits; `--force` starts the confirm including those). Same `Space`/`d` flow as any worktree pane.
- `cw doctor` — sanity-check the environment (`gh` auth, git credential helper, terminal, each configured agent on `PATH`); only a missing `default_agent` fails the exit code.
- `cw --repo owner/name --dry-run my-slug` — preview the clone/fetch + worktree + agent decision without mutating anything.
- `--yolo` (alias `--skip-permissions`) — launch Codex/Claude with their permission prompts skipped (`--dangerously-bypass-approvals-and-sandbox` / `--dangerously-skip-permissions`) for this invocation only; other agents are unaffected.
- `cw completions zsh` — emit a shell completion script; install via a directory on `fpath` (e.g. `mkdir -p ~/.zfunc && cw completions zsh > ~/.zfunc/_cw`, with `fpath=(~/.zfunc $fpath)` placed *before* `compinit` in `~/.zshrc`). Appending straight into `~/.zshrc` also works only if that file calls `compinit` earlier in the same file — `fpath` avoids the ordering trap.

## Self-update

`cw` checks for a newer release in the background automatically (at most once a day) and, once it finds one, shows an "update available" notice in the dashboard footer — press `u` to download and apply it in place. Only works for a binary installed via the one-line install script above; a `cargo install --path .` build has no install receipt to update against, so the check silently finds nothing to do.

## License

MIT — see [LICENSE](LICENSE).

## Search

Typing in a pane filters it, except for single-letter shortcuts (`j`, `k`, `q`, `d`, `r`, `u`, `n`, Space) while the filter is empty. Press `/` first to search with every letter; Enter or Tab keeps the filter, Esc clears it. Click focuses a row; a second click on the same row within 400ms opens it — any key or scroll in between resets that.

## Storage

Nothing is cloned ahead of time — browsing only caches repo *metadata* (`~/.cache/cw/repos.json`). The first session in a repo clones it as a **bare store** (git history only, no checkout of its own) under `root/owner/name`; each session is a worktree of that store, branched from the freshly fetched remote default branch. Concurrent sessions in one repo share one store.

Removing a session — `cw finish`, or `d` in any worktree pane — deletes its worktree and branch, and deletes the store too once no sessions are left. So the disk holds exactly the repos with open sessions, nothing more. Removal re-checks the worktree at that moment and refuses changed files, unpushed commits (unless forced with `f` / `cw clean --force`), and a worktree whose agent is still running.

`root` is disk-backed under `~/.local/share` rather than `/tmp`, which is often RAM-backed and wiped on reboot — a session must survive both. Upgrading from a version that used `~/repos`: finish your sessions there first; `cw` no longer looks in, or touches, `~/repos`.

Inside a session, git behaves like a normal clone: `origin/*` is fetched, and a plain `git push` publishes the `worktree-<slug>` branch. A pushed branch keeps counting as saved even after the remote deletes it on merge. Fetching uses system `git` (SSH remotes and credential helpers work) and never prompts; `--no-pull` skips it and branches from what's already fetched. An agent exiting nonzero (Ctrl-C, a failed last shell command) is reported as a note, not an error — the session is kept either way.
