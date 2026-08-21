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

**Default flow** — pick a repo, clone/pull, create or resume a worktree, launch the default agent:

```bash
cw
cw my-feature-slug          # explicit slug, skips the resume picker
cw --agent grok my-slug      # launch grok instead of default_agent
cw --repo owner/name my-slug # skip the repo picker, act on a named repo directly
```

- `cw resume` — pick from every known worktree across every repo, sorted by most recently touched.
- `cw scratch [SLUG]` — a real worktree with no project attached, for quick work that doesn't belong to a repo.
- `cw clean` / `cw clean --force` — remove finished worktrees (annotated dirty/clean and idle-days; dirty entries need `--force`).
- `cw doctor` — sanity-check the environment (`gh` auth, git credential helper, terminal, each configured agent on `PATH`); exits nonzero if anything fails.
- `cw --repo owner/name --dry-run my-slug` — preview the clone/pull + worktree + agent decision without mutating anything.
- `cw completions zsh` — emit a shell completion script; install via a directory on `fpath` (e.g. `mkdir -p ~/.zfunc && cw completions zsh > ~/.zfunc/_cw`, with `fpath=(~/.zfunc $fpath)` placed *before* `compinit` in `~/.zshrc`). Appending straight into `~/.zshrc` also works only if that file calls `compinit` earlier in the same file — `fpath` avoids the ordering trap.

## License

MIT — see [LICENSE](LICENSE).
