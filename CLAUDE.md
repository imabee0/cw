# cw

Rust CLI: fuzzy-pick a GitHub repo (personal + every org, most-recent-first), clone/pull it, create-or-resume a git worktree, launch an agent CLI inside it.

## Commands

```bash
cargo build                          # debug build
cargo install --path . --locked      # install the real binary onto PATH — use this, not `cargo run --`, for anything that must reflect a real install (doctor, completions, --dry-run)
scripts/check.sh                     # fmt --check, clippy -D warnings, cargo test, cargo deny check — run before every push
cw --help
cw doctor                            # exits nonzero on any failed check; needs a real controlling tty (`script -q /dev/null cw doctor` if testing from a non-tty harness — plain `cw doctor` there fails the terminal check, not a bug)
cw completions zsh                   # source only after `autoload -U compinit && compinit` has run, same as any zsh completion script
cw --repo OWNER/NAME --root PATH --dry-run SLUG   # only non-interactive way to preview clone/pull + worktree + agent decision with zero mutation
```

`cargo test` alone does not need `cargo deny`; `scripts/check.sh` is the full local gate CI mirrors.

## File map

Flat `src/*.rs` plus one `src/tui/` submodule, binary-only crate, no `lib.rs`.

| File | Responsibility |
|---|---|
| `main.rs` | CLI dispatch, default-flow orchestration, `needs_dashboard` fast-path gate, tracing/log init |
| `cli.rs` | clap-derive `Cli`/`Cmd` (`resume`, `clean`, `scratch`, `doctor`, `completions`) |
| `config.rs` | `Config` TOML load/save, `~/.config/cw`/`~/.cache/cw` path resolution, `resolve_agent()` |
| `github.rs` | `gh` subprocess wrappers: repo/org discovery, per-org failure isolation |
| `cache.rs` | `RepoCache`, atomic save (tmp+rename), corrupt-file-tolerant load |
| `dashboard.rs` | the single `tui::run()` entry point: builds the initial `DashboardModel` per `Entry` (bare `cw`/`resume`/`clean`/`scratch`), spawns every background thread (repo discovery, one-shot worktree scan, the reactive clone/pull thread), and drives the suspend/resume loop around hook runs and agent launches |
| `tui/mod.rs` | `Screen` trait, `run()` event loop returning `(Screen, Outcome)` so a screen survives a suspend/resume round trip, terminal lifecycle (raw mode/alt screen/mouse capture on stderr), panic-hook restore, `TUI_ACTIVE`, `is_interactive()` |
| `tui/msg.rs` | `Msg` enum, `RepoLoad`/`WorktreesLoad`/`CloneOutcome` (background-thread results) |
| `tui/model.rs` | `DashboardModel` (composite split-pane state: repo pane, worktree pane, agent footer, the checked-for-removal set, the clone/hook/create/launch `PendingLaunch` pipeline), shared `ListState<T>`, idle/relative-time formatting |
| `tui/update.rs` | pure `update_dashboard` — terminal-free, unit tested directly |
| `tui/view.rs` | pure `draw_dashboard` — `ratatui::widgets::Table`/`Paragraph` split-pane rendering |
| `tui/event.rs` | crossterm event polling + tick cadence |
| `tui/widgets.rs` | `row_at` mouse hit-testing, frizbee-backed `filter_indices` |
| `gitstatus.rs` | `is_dirty`/`is_dirty_repo` — shared by sync.rs's pull guard and clean.rs |
| `sync.rs` | clone/pull (`clone_or_pull_ex`'s `CloneStdio::Capture` variant backs the dashboard's background clone thread), `gh`-credential-helper wiring, dirty-tree guard before force-checkout |
| `worktree.rs` | slug validation/flattening/`unflatten_slug`, worktree create-or-resume, `scan_worktrees`, `remove_worktree`, `display_repo_label`, `WorktreeSelection`/`CleanCandidate`, `generate_timestamp_slug` |
| `worktreeinclude.rs` | `.worktreeinclude` copy: symlink-preserving, CRLF/BOM-tolerant, continue-on-error |
| `hooks.rs` | `post_clone_hook`/`post_create_hook` execution, confirm-once-per-repo consent |
| `agent.rs` | launches the resolved agent CLI in the worktree, distinguishes exit outcomes |
| `clean.rs` | `cw clean`: thin `dashboard::run(Entry::Clean)` entry point; `remove_one` (git2 prune + branch delete), shared with the dashboard's in-TUI delete flow |
| `doctor.rs` | `cw doctor`: gh auth, credential helper, terminal, each configured agent on PATH |
| `selfupdate.rs` | background self-update check (`spawn_check`) + apply (`apply_update`, the dashboard's `u` key), also backing `cw doctor`'s stale-binary diagnostic; every failure (no install receipt, offline, rate-limited) degrades silently, never an error |

## Conventions

- No `async`/tokio — plain sync `fn main() -> anyhow::Result<()>`; the `tui` event loop is a plain synchronous poll, and background repo discovery is one `std::thread::spawn` + `mpsc::channel`, nothing heavier.
- `#[serde(deny_unknown_fields)]` on `Config` — a typo'd key fails loudly, never silently ignored.
- Every home-dir-dependent test takes an injected path or `HOME` override, never asserts a literal `/Users/...` path.
- New CLI flags: mirror the existing `#[arg(long, global = true)]` pattern for anything `resume`/`clean`/`scratch` should inherit.

## Invariants and gotchas

- **Worktree/branch naming exactly mirrors Claude Code's own scheme** (`flatten_slug`, `.claude/worktrees/<flat>`, branch `worktree-<flat>`) — required for interop with `EnterWorktree`/subagent tooling. Do not change this shape without checking `worktree.ts` first.
- **`cw` always creates worktrees itself** via git2, never shells out to the agent CLI's own worktree flag — this is what makes resume/clean work identically across every configured agent.
- **`create_or_resume_worktree`** appends `/.claude/worktrees/` to the cloned repo's `.git/info/exclude` on first create — removing this reopens a dirty-tree false-positive that silently blocks every future pull on that repo.
- **Hooks (`post_clone_hook`/`post_create_hook`) default to unset.** They execute code the cloned repo supplies, not code the user wrote — never flip either default on, never skip the confirm-once-per-repo consent gate in `hooks.rs`.
- **`gh repo list` must always carry `--limit 1000`** (`github.rs::repo_list_args`) — `gh`'s own default of 30 silently truncates discovery.
- **`--org` only takes effect on a cache miss or with `--refresh`** — `dashboard.rs`'s `spawn_repo_thread` serves a warm `repos.json` (age < `cache_ttl_minutes`) before the org-filter closure ever runs, so `--org` alone against a warm cache silently returns the unfiltered full list. Pair it with `--refresh` when testing or scripting against a specific org.
- **Dashboard non-interactivity is gated on `/dev/tty` + `stderr().is_terminal()`, never on `stdin().is_terminal()`** — crossterm's unix backend opens `/dev/tty` directly for its event source (`tty_fd()`, falling back to a std fd only if `/dev/tty` is unavailable — confirmed in crossterm 0.29's source, not assumed), and `tui::mod`'s `CrosstermBackend` renders to stderr, not stdout, so a piped stdin over a real controlling terminal must still reach the dashboard, and `cw | cat` must never receive UI escape codes. `script -q /dev/null cw` allocates a real pty for local testing of this path — no automated pass has driven the TUI interactively to confirm it against this backend (no controlling terminal in that environment either), so this still wants a real-terminal check before being trusted.
- **Any `tracing::warn!`/`error!` firing while a `tui::run` session owns the screen would corrupt the frame** — `main.rs::init_logging` gates the stderr half of its tee behind `tui::TUI_ACTIVE` (an `AtomicBool` set for the duration of every `run()` call) so log lines still reach the day log file but never interleave with in-progress terminal escape codes. Route anything the dashboard needs the user to see into `DashboardModel::status` instead of logging it directly (`dashboard.rs::spawn_repo_thread`'s background thread does this for per-org discovery warnings and a stale-cache notice).
- **`tracing_appender`'s `WorkerGuard` must stay a named local bound in `main()`** — dropping it early silently loses buffered log lines.
- Config/cache paths are fixed at `~/.config/cw/config.toml` and `~/.cache/cw/{repos.json,cw.log.*}` on both macOS and Linux — not `directories::ProjectDirs`, deliberately, to avoid diverging per platform.
- `cw` shells out to `gh` for all GitHub API access (repo/org listing, clone auth) — `gh auth status` must be green; `cw doctor` checks this.
- **`RELEASE_PLZ_TOKEN` (a PAT, not the default `GITHUB_TOKEN`) backs every `release-plz` step in `release-plz.yml`** — GitHub's anti-recursion rule means a tag push or PR authored with the default `GITHUB_TOKEN` never triggers other workflows' listeners, so without it `release.yml`'s tag-push trigger and the release PR's own CI checks would silently never fire.
- **`release-plz.toml` sets `git_release_enable = false`** — `release.yml`'s cargo-dist host job already creates the GitHub Release for each tag; two creators racing on the same release would duplicate or fail it.
- **`release-verify.yml` is a separate workflow file, not folded into `release.yml`** — `release.yml` is cargo-dist-generated (`dist generate`) and must never be hand-edited; it triggers on the Release workflow's completion instead.
- **Self-update degrades completely silently when no install receipt is present** — a `cargo install --path .` (from-source) build has no `~/.config/cw/cw-receipt.json`, so `selfupdate`'s check and apply fail quietly to a `tracing::debug!` line and the dashboard's `u` key never activates; by design, not a bug, but a from-source install never learns about updates.
- **A mouse click in `tui` only ever focuses a row (`TableState::select`), never activates or marks it** — a click and a keyboard arrow key are the same operation, both funneling through Enter to commit and Space to mark. `tui::update`'s mouse handlers never return an `Outcome` from a `MouseEventKind::Down`.
- **Any single-char key binding that doubles as filter text must be gated on the *focused pane's* `query.is_empty()`**, same as `q`-quit. There is no delete-mode toggle: `Space` always marks/unmarks the focused worktree row and never types (`toggle_checked_focused`), and `d` always opens the removal confirm for the checked set, or the focused row alone when nothing is checked — both gated in `tui/update.rs` because an ungated destructive binding is worse than an ungated quit. `r` (rescan) and `u` (apply a pending self-update) are gated the same way. Each pane (repo `ListState`, worktree `ListState`) owns its own query, and gating always checks whichever pane `DashboardModel::focus` currently points at, never a single global query. `j`/`k` are the deliberate exception: always navigation, never typable, so they're never gated.

## Standards exceptions

- **This repo's own source is hosted on GitHub** (`imabee0/cw`, public, ruleset + auto-merge on `main`) instead of the house Gitea-only default (global CLAUDE.md § Structure and forge). Explicit, repeated user direction for this one project. Do not migrate this to Gitea without the same explicit direction.
- **`.github/workflows/release.yml` is generated and owned by `cargo-dist`** (`dist generate`, config in `dist-workspace.toml`) — do not hand-edit it; change `dist-workspace.toml` and regenerate. It's exempt from `ci.yml`'s manual SHA-pin-with-`# vX.Y.Z`-comment convention: dist 0.32.0's generated output pins its four `actions/*` steps (`checkout`, `upload-artifact`, `download-artifact`, `attest`) to floating major-version tags (`@v6`/`@v7`/`@v8`/`@v4`), not commit SHAs — confirmed by inspecting the actual generated file, not assumed from dist's docs. Accepted rather than hand-patched because (a) all four are first-party `actions/*`, the lower-risk category the SHA-pinning policy exists to distinguish from third-party actions like `dtolnay/rust-toolchain`, and (b) hand-patched pins would be silently lost on every `dist generate`. `dist` has no config flag for this (checked `dist generate --help`); revisit if a future dist version adds one.
