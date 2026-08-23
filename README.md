# cw

`cw` fuzzy-picks a GitHub repo — your personal account plus every org you belong to, most-recently-updated first — clones or pulls it, creates (or resumes) a git worktree, and launches an agent CLI inside it. One command instead of remembering the clone path, pulling, inventing a worktree slug, and picking the right binary.

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
root = "~/repos"
cache_ttl_minutes = 15
default_agent = "claude"
idle_threshold_days = 14

# Dirs symlinked from the main checkout into every new worktree instead of
# reinstalling. Empty by default — a shared node_modules is mutable state
# across worktrees. Opt in per-repo:
symlink_dirs = []

# Both hooks run code the CLONED REPO supplies — off by default, confirmed
# once per repo before the first run (or --yes to skip the prompt):
# post_clone_hook = "scripts/setup.sh"       # cwd = repo root, fresh clones only
# post_create_hook = "auto"                  # or a script path; "auto" sniffs
#                                             # Cargo.lock/package-lock.json/pnpm-lock.yaml

[agents.claude]
cmd = "claude"
args = []

[agents.grok]
cmd = "grok"
args = []

[agents.shell]
cmd = "$SHELL"
args = []
```

## Usage

**Default flow** — pick a repo, clone/pull, create or resume a worktree, launch the default agent. `cw` (bare), `cw resume`, and `cw clean` all open on a worktree pane listing every worktree across every repo — no need to pick a repo first; picking one in the repo pane only targets it for clone/pull or a new worktree. Every pane supports arrow keys/`j`/`k`, live fuzzy filtering, and mouse click/scroll (a click focuses a row; Enter commits it). In the worktree pane: `Space` marks/unmarks a row for removal, `d` opens a removal confirm for the marked rows (or just the focused row if none are marked), `r` rescans, `u` applies a pending self-update, `Esc` clears the filter, then the marked set, then cancels:

```bash
cw
cw my-feature-slug          # explicit slug, skips the resume picker
cw --agent grok my-slug      # launch grok instead of default_agent
cw --repo owner/name my-slug # skip the repo picker, act on a named repo directly
```

- `cw resume` — open straight on the worktree pane (see above), skipping the repo pane entirely.
- `cw scratch [SLUG]` — a real worktree with no project attached, for quick work that doesn't belong to a repo.
- `cw clean` / `cw clean --force` — remove finished worktrees (annotated dirty/clean and idle-days; dirty entries need `--force`); same `Space`/`d` flow as any worktree pane.
- `cw doctor` — sanity-check the environment (`gh` auth, git credential helper, terminal, each configured agent on `PATH`); exits nonzero if anything fails.
- `cw --repo owner/name --dry-run my-slug` — preview the clone/pull + worktree + agent decision without mutating anything.
- `cw completions zsh` — emit a shell completion script; install via a directory on `fpath` (e.g. `mkdir -p ~/.zfunc && cw completions zsh > ~/.zfunc/_cw`, with `fpath=(~/.zfunc $fpath)` placed *before* `compinit` in `~/.zshrc`). Appending straight into `~/.zshrc` also works only if that file calls `compinit` earlier in the same file — `fpath` avoids the ordering trap.

## Self-update

`cw` checks for a newer release in the background automatically (at most once a day) and, once it finds one, shows an "update available" notice in the dashboard footer — press `u` to download and apply it in place. Only works for a binary installed via the one-line install script above; a `cargo install --path .` build has no install receipt to update against, so the check silently finds nothing to do.

## License

MIT — see [LICENSE](LICENSE).
