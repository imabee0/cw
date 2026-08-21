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

Flat `src/*.rs`, binary-only crate, no `lib.rs`.

| File | Responsibility |
|---|---|
| `main.rs` | CLI dispatch, default-flow orchestration, tracing/log init |
| `cli.rs` | clap-derive `Cli`/`Cmd` (`resume`, `clean`, `scratch`, `doctor`, `completions`) |
| `config.rs` | `Config` TOML load/save, `~/.config/cw`/`~/.cache/cw` path resolution, `resolve_agent()` |
| `github.rs` | `gh` subprocess wrappers: repo/org discovery, per-org failure isolation |
| `cache.rs` | `RepoCache`, atomic save (tmp+rename), corrupt-file-tolerant load |
| `picker.rs` | skim `SkimItem` impls, repo/worktree/agent pickers, non-interactive/empty-list prechecks |
| `gitstatus.rs` | `is_dirty`/`is_dirty_repo` — shared by sync.rs's pull guard and clean.rs |
| `sync.rs` | clone/pull, `gh`-credential-helper wiring, dirty-tree guard before force-checkout |
| `worktree.rs` | slug validation/flattening, worktree create-or-resume, `scan_worktrees`, `remove_worktree` |
| `worktreeinclude.rs` | `.worktreeinclude` copy: symlink-preserving, CRLF/BOM-tolerant, continue-on-error |
| `hooks.rs` | `post_clone_hook`/`post_create_hook` execution, confirm-once-per-repo consent |
| `agent.rs` | launches the resolved agent CLI in the worktree, distinguishes exit outcomes |
| `clean.rs` | `cw clean`: scan, dirty/idle annotate, multi-select, prune |
| `doctor.rs` | `cw doctor`: gh auth, credential helper, terminal, each configured agent on PATH |

## Conventions

- No `async`/tokio — plain sync `fn main() -> anyhow::Result<()>`; skim owns its own runtime.
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
- **Picker non-interactivity is gated on `/dev/tty` + `stderr().is_terminal()`, never on `stdin().is_terminal()`** — skim reads keystrokes from `/dev/tty` directly, so a piped stdin over a real controlling terminal must still reach the picker, not error. Verified empirically: `script -q /dev/null` allocates a real pty for local testing of this path.
- **`tracing_appender`'s `WorkerGuard` must stay a named local bound in `main()`** — dropping it early silently loses buffered log lines.
- Config/cache paths are fixed at `~/.config/cw/config.toml` and `~/.cache/cw/{repos.json,cw.log.*}` on both macOS and Linux — not `directories::ProjectDirs`, deliberately, to avoid diverging per platform.
- `cw` shells out to `gh` for all GitHub API access (repo/org listing, clone auth) — `gh auth status` must be green; `cw doctor` checks this.
