# cw

Rust CLI: fuzzy-pick a GitHub repo (personal + every org, most-recent-first), clone/pull it, create-or-resume a git worktree, launch an agent CLI inside it. Full design in `~/.claude/plans/cw-rust-shiny-frog.md` — read it before any non-trivial change, it is the source of truth for every decision below.

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

`cargo test` alone does not need `cargo deny`; `scripts/check.sh` is the full local gate CI mirrors (§9 of the plan).

## File map

Flat `src/*.rs` plus one `src/tui/` submodule, binary-only crate, no `lib.rs`.

| File | Responsibility |
|---|---|
| `main.rs` | CLI dispatch, default-flow orchestration, tracing/log init |
| `cli.rs` | clap-derive `Cli`/`Cmd` (`resume`, `clean`, `scratch`, `doctor`, `completions`) |
| `config.rs` | `Config` TOML load/save, `~/.config/cw`/`~/.cache/cw` path resolution, `resolve_agent()` |
| `github.rs` | `gh` subprocess wrappers: repo/org discovery, per-org failure isolation |
| `cache.rs` | `RepoCache`, atomic save (tmp+rename), corrupt-file-tolerant load |
| `picker.rs` | assembles a `tui` screen per picker (`pick_repo`, `pick_worktree_and_agent`, `pick_worktrees_multi`, `pick_agent`), `Pick<T>`/`is_interactive` |
| `tui/mod.rs` | `Screen` trait, `run()` event loop, terminal lifecycle (raw mode/alt screen/mouse capture on stderr), panic-hook restore, `TUI_ACTIVE` |
| `tui/msg.rs` | `Msg` enum, `RepoLoad` (background repo-discovery result) |
| `tui/model.rs` | `RepoModel`/`WorktreeModel`/`AgentModel`, shared `ListState<T>`, idle/relative-time formatting |
| `tui/update.rs` | pure `update_repo`/`update_worktree`/`update_agent` — terminal-free, unit tested directly |
| `tui/view.rs` | pure `draw_repo`/`draw_worktree`/`draw_agent` — `ratatui::widgets::Table`/`Paragraph` rendering |
| `tui/event.rs` | crossterm event polling + tick cadence |
| `tui/widgets.rs` | `row_at` mouse hit-testing, frizbee-backed `filter_indices` |
| `gitstatus.rs` | `is_dirty`/`is_dirty_repo` — shared by sync.rs's pull guard and clean.rs |
| `sync.rs` | clone/pull, `gh`-credential-helper wiring, dirty-tree guard before force-checkout |
| `worktree.rs` | slug validation/flattening, worktree create-or-resume, `scan_worktrees`, `remove_worktree`, `display_repo_label` |
| `worktreeinclude.rs` | `.worktreeinclude` copy: symlink-preserving, CRLF/BOM-tolerant, continue-on-error |
| `hooks.rs` | `post_clone_hook`/`post_create_hook` execution, confirm-once-per-repo consent |
| `agent.rs` | launches the resolved agent CLI in the worktree, distinguishes exit outcomes |
| `clean.rs` | `cw clean`: scan, dirty/idle annotate, multi-select, prune |
| `doctor.rs` | `cw doctor`: gh auth, credential helper, terminal, each configured agent on PATH |

## Conventions

- No `async`/tokio — plain sync `fn main() -> anyhow::Result<()>`; the `tui` event loop is a plain synchronous poll, and background repo discovery is one `std::thread::spawn` + `mpsc::channel`, nothing heavier.
- `#[serde(deny_unknown_fields)]` on `Config` — a typo'd key fails loudly, never silently ignored.
- Every home-dir-dependent test takes an injected path or `HOME` override, never asserts a literal `/Users/...` path.
- New CLI flags: mirror the existing `#[arg(long, global = true)]` pattern for anything `resume`/`clean`/`scratch` should inherit.

## Invariants and gotchas

- **Worktree/branch naming exactly mirrors Claude Code's own scheme** (`flatten_slug`, `.claude/worktrees/<flat>`, branch `worktree-<flat>`) — required for interop with `EnterWorktree`/subagent tooling. Do not change this shape without checking `worktree.ts` first.
- **`cw` always creates worktrees itself** via git2, never shells out to the agent CLI's own worktree flag — this is what makes resume/clean work identically across every configured agent.
- **`create_or_resume_worktree`** appends `/.claude/worktrees/` to the cloned repo's `.git/info/exclude` on first create — removing this reopens a dirty-tree false-positive that silently blocks every future pull on that repo (plan §5d, the single most consequential fix in the design).
- **Hooks (`post_clone_hook`/`post_create_hook`) default to unset.** They execute code the cloned repo supplies, not code the user wrote — never flip either default on, never skip the confirm-once-per-repo consent gate in `hooks.rs`.
- **`gh repo list` must always carry `--limit 1000`** (`github.rs::repo_list_args`) — `gh`'s own default of 30 silently truncates discovery.
- **`--org` only takes effect on a cache miss or with `--refresh`** — `pick_repo_interactive` serves a warm `repos.json` (age < `cache_ttl_minutes`) before the org-filter closure ever runs, so `--org` alone against a warm cache silently returns the unfiltered full list. Pair it with `--refresh` when testing or scripting against a specific org.
- **Picker non-interactivity is gated on `/dev/tty` + `stderr().is_terminal()`, never on `stdin().is_terminal()`** — crossterm's unix backend opens `/dev/tty` directly for its event source (`tty_fd()`, falling back to a std fd only if `/dev/tty` is unavailable — confirmed in crossterm 0.29's source, not assumed), and `tui::mod`'s `CrosstermBackend` renders to stderr, not stdout, so a piped stdin over a real controlling terminal must still reach the picker, and `cw | cat` must never receive UI escape codes. `script -q /dev/null cw` allocates a real pty for local testing of this path — no automated pass has driven the TUI interactively to confirm it against this backend (no controlling terminal in that environment either), so this still wants a real-terminal check before being trusted.
- **Any `tracing::warn!`/`error!` firing while a `tui::run` session owns the screen would corrupt the frame** — `main.rs::init_logging` gates the stderr half of its tee behind `tui::TUI_ACTIVE` (an `AtomicBool` set for the duration of every `run()` call) so log lines still reach the day log file but never interleave with in-progress terminal escape codes. Route anything a screen needs the user to see into that `Model`'s status line instead of logging it directly (`pick_repo_interactive`'s background thread does this for per-org discovery warnings and a stale-cache notice).
- **`tracing_appender`'s `WorkerGuard` must stay a named local bound in `main()`** — dropping it early silently loses buffered log lines.
- Config/cache paths are fixed at `~/.config/cw/config.toml` and `~/.cache/cw/{repos.json,cw.log.*}` on both macOS and Linux — not `directories::ProjectDirs`, deliberately, to avoid diverging per platform.
- `cw` shells out to `gh` for all GitHub API access (repo/org listing, clone auth) — `gh auth status` must be green; `cw doctor` checks this.
- **A mouse click in `tui` only ever focuses a row (`TableState::select`), never activates it** — outside multi-select mode a click and a keyboard arrow key are the same operation, both funneling through Enter to actually commit. `tui::update`'s mouse handlers never return an `Outcome` from a `MouseEventKind::Down`; only multi-select mode additionally toggles a checkbox on click (still not a commit — `d` is).

## Standards exceptions

- **This repo's own source is hosted on GitHub** (`imabee0/cw`, public, ruleset + auto-merge on `main`) instead of the house Gitea-only default (global CLAUDE.md § Structure and forge). Explicit, repeated user direction for this one project — see plan §0's "GitHub via `gh`" and "CI/CD + merge policy" rows for the full decision record. Do not migrate this to Gitea without the same explicit direction.
- **`.github/workflows/release.yml` is generated and owned by `cargo-dist`** (`dist generate`, config in `dist-workspace.toml`) — do not hand-edit it; change `dist-workspace.toml` and regenerate. It's exempt from `ci.yml`'s manual SHA-pin-with-`# vX.Y.Z`-comment convention: dist 0.32.0's generated output pins its four `actions/*` steps (`checkout`, `upload-artifact`, `download-artifact`, `attest`) to floating major-version tags (`@v6`/`@v7`/`@v8`/`@v4`), not commit SHAs — confirmed by inspecting the actual generated file, not assumed from dist's docs. Accepted rather than hand-patched because (a) all four are first-party `actions/*`, the lower-risk category the SHA-pinning policy exists to distinguish from third-party actions like `dtolnay/rust-toolchain`, and (b) hand-patched pins would be silently lost on every `dist generate`. `dist` has no config flag for this (checked `dist generate --help`); revisit if a future dist version adds one.
