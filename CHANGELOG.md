# Changelog

This file covers releases up to 0.2.0. From 0.2.1 on, each release's notes are generated from its conventional commits and published with it as a [GitHub Release](https://github.com/imabee0/cw/releases) tagged `cw-vX.Y.Z`; this file is no longer updated.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0](https://github.com/imabee0/cw/compare/v0.1.16...v0.2.0) - 2026-11-04

**Upgrading:** repos are no longer kept as full checkouts in `~/repos`. Finish (or push) your existing sessions there with 0.1.x before upgrading — 0.2 no longer lists or touches `~/repos`.

### Added

- [**breaking**] On-demand session stores ([#54](https://github.com/imabee0/cw/pull/54)): a repo is cloned only when its first session starts, as a bare store (history only, no checkout) under `$XDG_DATA_HOME/cw/sessions` (default `~/.local/share/cw/sessions`, previously `~/repos`). Each session is a worktree branched from the freshly fetched remote default branch; finishing a repo's last session deletes the store, so only repos with open sessions are on disk.
- `cw finish OWNER/NAME SLUG`: remove one session whose work is committed and pushed, non-interactively.
- `--yolo` (alias `--skip-permissions`): launch Codex/Claude with permission prompts skipped, for that invocation only.
- `/` in the dashboard: search with every letter, shortcut keys included.
- `cw doctor` checks that `git` is on `PATH`.

### Changed

- Removing a session (`cw finish`, `d` in the dashboard) re-checks changed files, unpushed commits and whether its agent is still running at that moment; a lock left by a crashed `cw` is cleared automatically.
- Fetches use system `git` (SSH remotes and credential helpers work) and never prompt; a pushed session branch stays "pushed" after the remote deletes it on merge.
- Inside a session, git behaves like a normal clone: `origin/*` is fetched and a plain `git push` publishes the session branch.
- Hooks resolve against the new session worktree; `post_clone_hook` runs once that worktree exists.
- `symlink_dirs` keeps one shared copy per repo in its store; `.worktreeinclude` copies from the repo's most recent other session and never overwrites a tracked file.
- `--org` filters the cached repo list too; discovery always caches every org.
- An agent exiting nonzero is a status note, not a launch-failure error.

### Fixed

- A failed update no longer silently starts a session from stale code; dirty, diverged or detached checkouts stop with recovery instructions.
- Creating a worktree never resets an existing branch's commits or prunes live/locked worktree metadata.
- Concurrent or interrupted clones can't leave a half-made repo behind.
- Worktree paths stay consistent through a symlinked root (e.g. macOS `/var`).
- Keyboard input and scrolling reset pending double-click detection.

## [0.1.16](https://github.com/imabee0/cw/compare/v0.1.15...v0.1.16) - 2026-10-16

### Other

- macos-26 runners for dist-build and verify; rustls 0.23.46 ([#48](https://github.com/imabee0/cw/pull/48))

## [0.1.15](https://github.com/imabee0/cw/compare/v0.1.14...v0.1.15) - 2026-09-02

### Other

- one event-driven workflow for validate, tag, publish, verify ([#33](https://github.com/imabee0/cw/pull/33))

## [0.1.14](https://github.com/imabee0/cw/compare/v0.1.13...v0.1.14) - 2026-09-02

### Fixed

- *(ci)* stop GitHub running github-build-setup as a workflow ([#30](https://github.com/imabee0/cw/pull/30))

## [0.1.13](https://github.com/imabee0/cw/compare/v0.1.12...v0.1.13) - 2026-09-02

### Fixed

- *(release)* rustup update stable before cargo-dist Darwin builds ([#28](https://github.com/imabee0/cw/pull/28))

## [0.1.12](https://github.com/imabee0/cw/compare/v0.1.11...v0.1.12) - 2026-09-02

### Added

- *(tui)* clickable agents, safer delete, and clone error modal ([#26](https://github.com/imabee0/cw/pull/26))

## [0.1.11](https://github.com/imabee0/cw/compare/v0.1.10...v0.1.11) - 2026-08-23

### Other

- sync CLAUDE.md/README/Cargo.toml with wave 2/3 behavior ([#23](https://github.com/imabee0/cw/pull/23))

## [0.1.10](https://github.com/imabee0/cw/compare/v0.1.9...v0.1.10) - 2026-08-23

### Added

- add cw self-update via axoupdater ([#21](https://github.com/imabee0/cw/pull/21))

## [0.1.9](https://github.com/imabee0/cw/compare/v0.1.8...v0.1.9) - 2026-08-23

### Added

- *(tui)* rework dashboard into a single worktree-first pane ([#19](https://github.com/imabee0/cw/pull/19))

## [0.1.8](https://github.com/imabee0/cw/compare/v0.1.7...v0.1.8) - 2026-08-22

### Fixed

- verify x86_64-apple-darwin via Rosetta on macos-14, drop hung macos-13 leg ([#17](https://github.com/imabee0/cw/pull/17))

## [0.1.7](https://github.com/imabee0/cw/compare/v0.1.6...v0.1.7) - 2026-08-22

### Fixed

- stop release-plz cascade and wire release-verify to fire ([#15](https://github.com/imabee0/cw/pull/15))

## [0.1.6](https://github.com/imabee0/cw/compare/v0.1.5...v0.1.6) - 2026-08-22

### Added

- add automatic release-plz tagging/PR chain and post-release verify ([#7](https://github.com/imabee0/cw/pull/7))
- *(tui)* replace three sequential picker screens with one dashboard ([#4](https://github.com/imabee0/cw/pull/4))
- *(tui)* replace skim pickers with a ratatui Model/Msg/Update/View TUI ([#3](https://github.com/imabee0/cw/pull/3))
- add cargo-dist release pipeline and one-line install script ([#2](https://github.com/imabee0/cw/pull/2))

### Fixed

- address dashboard PR review follow-ups ([#5](https://github.com/imabee0/cw/pull/5))
- serialize SHELL-env-mutating tests to eliminate a cargo-test race
- address code review findings
- cargo-deny licenses check failing on unlicensed cw package

### Other

- release v0.1.5 ([#12](https://github.com/imabee0/cw/pull/12))
- release v0.1.4 ([#11](https://github.com/imabee0/cw/pull/11))
- release v0.1.3 ([#10](https://github.com/imabee0/cw/pull/10))
- release v0.1.2 ([#9](https://github.com/imabee0/cw/pull/9))
- release v0.1.1 ([#8](https://github.com/imabee0/cw/pull/8))
- harden pipeline (locked deps, dist-check, secrets scan, smoke test) ([#6](https://github.com/imabee0/cw/pull/6))
- real clone URL in README ([#1](https://github.com/imabee0/cw/pull/1))
- record the GitHub-hosting standards exception explicitly in CLAUDE.md
- add CLAUDE.md, README, LICENSE; verify installed binary
- add CLAUDE.md, README, LICENSE; verify installed binary
- Document that the unflatten fix's regression test doesn't cover the call site
- Fix §0a resume picker feeding a flattened slug back into validate_worktree_slug
- Wire the full cw CLI: picker, doctor, clean, completions, and main dispatch
- Guard walk_all_files against sweeping a nested .git dir (submodule) into the worktree when a directory pattern matches
- Implement worktree.rs, worktreeinclude.rs, hooks.rs per plan §5d-§5h/§5n
- Narrow git2 features to https-only, add --no-pull support to clone_or_pull
- Implement sync.rs and agent.rs per plan sections 5a/5b/5c/5i
- add GitHub Actions workflow, dependabot, cargo-deny config
- cw project skeleton, config/cli types, module stubs

## [0.1.5](https://github.com/imabee0/cw/compare/v0.1.4...v0.1.5) - 2026-08-22

### Added

- add automatic release-plz tagging/PR chain and post-release verify ([#7](https://github.com/imabee0/cw/pull/7))
- *(tui)* replace three sequential picker screens with one dashboard ([#4](https://github.com/imabee0/cw/pull/4))
- *(tui)* replace skim pickers with a ratatui Model/Msg/Update/View TUI ([#3](https://github.com/imabee0/cw/pull/3))
- add cargo-dist release pipeline and one-line install script ([#2](https://github.com/imabee0/cw/pull/2))

### Fixed

- address dashboard PR review follow-ups ([#5](https://github.com/imabee0/cw/pull/5))
- serialize SHELL-env-mutating tests to eliminate a cargo-test race
- address code review findings
- cargo-deny licenses check failing on unlicensed cw package

### Other

- release v0.1.4 ([#11](https://github.com/imabee0/cw/pull/11))
- release v0.1.3 ([#10](https://github.com/imabee0/cw/pull/10))
- release v0.1.2 ([#9](https://github.com/imabee0/cw/pull/9))
- release v0.1.1 ([#8](https://github.com/imabee0/cw/pull/8))
- harden pipeline (locked deps, dist-check, secrets scan, smoke test) ([#6](https://github.com/imabee0/cw/pull/6))
- real clone URL in README ([#1](https://github.com/imabee0/cw/pull/1))
- record the GitHub-hosting standards exception explicitly in CLAUDE.md
- add CLAUDE.md, README, LICENSE; verify installed binary
- add CLAUDE.md, README, LICENSE; verify installed binary
- Document that the unflatten fix's regression test doesn't cover the call site
- Fix §0a resume picker feeding a flattened slug back into validate_worktree_slug
- Wire the full cw CLI: picker, doctor, clean, completions, and main dispatch
- Guard walk_all_files against sweeping a nested .git dir (submodule) into the worktree when a directory pattern matches
- Implement worktree.rs, worktreeinclude.rs, hooks.rs per plan §5d-§5h/§5n
- Narrow git2 features to https-only, add --no-pull support to clone_or_pull
- Implement sync.rs and agent.rs per plan sections 5a/5b/5c/5i
- add GitHub Actions workflow, dependabot, cargo-deny config
- cw project skeleton, config/cli types, module stubs

## [0.1.4](https://github.com/imabee0/cw/compare/v0.1.3...v0.1.4) - 2026-08-22

### Added

- add automatic release-plz tagging/PR chain and post-release verify ([#7](https://github.com/imabee0/cw/pull/7))
- *(tui)* replace three sequential picker screens with one dashboard ([#4](https://github.com/imabee0/cw/pull/4))
- *(tui)* replace skim pickers with a ratatui Model/Msg/Update/View TUI ([#3](https://github.com/imabee0/cw/pull/3))
- add cargo-dist release pipeline and one-line install script ([#2](https://github.com/imabee0/cw/pull/2))

### Fixed

- address dashboard PR review follow-ups ([#5](https://github.com/imabee0/cw/pull/5))
- serialize SHELL-env-mutating tests to eliminate a cargo-test race
- address code review findings
- cargo-deny licenses check failing on unlicensed cw package

### Other

- release v0.1.3 ([#10](https://github.com/imabee0/cw/pull/10))
- release v0.1.2 ([#9](https://github.com/imabee0/cw/pull/9))
- release v0.1.1 ([#8](https://github.com/imabee0/cw/pull/8))
- harden pipeline (locked deps, dist-check, secrets scan, smoke test) ([#6](https://github.com/imabee0/cw/pull/6))
- real clone URL in README ([#1](https://github.com/imabee0/cw/pull/1))
- record the GitHub-hosting standards exception explicitly in CLAUDE.md
- add CLAUDE.md, README, LICENSE; verify installed binary
- add CLAUDE.md, README, LICENSE; verify installed binary
- Document that the unflatten fix's regression test doesn't cover the call site
- Fix §0a resume picker feeding a flattened slug back into validate_worktree_slug
- Wire the full cw CLI: picker, doctor, clean, completions, and main dispatch
- Guard walk_all_files against sweeping a nested .git dir (submodule) into the worktree when a directory pattern matches
- Implement worktree.rs, worktreeinclude.rs, hooks.rs per plan §5d-§5h/§5n
- Narrow git2 features to https-only, add --no-pull support to clone_or_pull
- Implement sync.rs and agent.rs per plan sections 5a/5b/5c/5i
- add GitHub Actions workflow, dependabot, cargo-deny config
- cw project skeleton, config/cli types, module stubs

## [0.1.3](https://github.com/imabee0/cw/compare/v0.1.2...v0.1.3) - 2026-08-22

### Added

- add automatic release-plz tagging/PR chain and post-release verify ([#7](https://github.com/imabee0/cw/pull/7))
- *(tui)* replace three sequential picker screens with one dashboard ([#4](https://github.com/imabee0/cw/pull/4))
- *(tui)* replace skim pickers with a ratatui Model/Msg/Update/View TUI ([#3](https://github.com/imabee0/cw/pull/3))
- add cargo-dist release pipeline and one-line install script ([#2](https://github.com/imabee0/cw/pull/2))

### Fixed

- address dashboard PR review follow-ups ([#5](https://github.com/imabee0/cw/pull/5))
- serialize SHELL-env-mutating tests to eliminate a cargo-test race
- address code review findings
- cargo-deny licenses check failing on unlicensed cw package

### Other

- release v0.1.2 ([#9](https://github.com/imabee0/cw/pull/9))
- release v0.1.1 ([#8](https://github.com/imabee0/cw/pull/8))
- harden pipeline (locked deps, dist-check, secrets scan, smoke test) ([#6](https://github.com/imabee0/cw/pull/6))
- real clone URL in README ([#1](https://github.com/imabee0/cw/pull/1))
- record the GitHub-hosting standards exception explicitly in CLAUDE.md
- add CLAUDE.md, README, LICENSE; verify installed binary
- add CLAUDE.md, README, LICENSE; verify installed binary
- Document that the unflatten fix's regression test doesn't cover the call site
- Fix §0a resume picker feeding a flattened slug back into validate_worktree_slug
- Wire the full cw CLI: picker, doctor, clean, completions, and main dispatch
- Guard walk_all_files against sweeping a nested .git dir (submodule) into the worktree when a directory pattern matches
- Implement worktree.rs, worktreeinclude.rs, hooks.rs per plan §5d-§5h/§5n
- Narrow git2 features to https-only, add --no-pull support to clone_or_pull
- Implement sync.rs and agent.rs per plan sections 5a/5b/5c/5i
- add GitHub Actions workflow, dependabot, cargo-deny config
- cw project skeleton, config/cli types, module stubs

## [0.1.2](https://github.com/imabee0/cw/compare/v0.1.1...v0.1.2) - 2026-08-22

### Other

- update Cargo.toml dependencies

## [0.1.1](https://github.com/imabee0/cw/compare/v0.1.0...v0.1.1) - 2026-08-22

### Added

- add automatic release-plz tagging/PR chain and post-release verify ([#7](https://github.com/imabee0/cw/pull/7))
- *(tui)* replace three sequential picker screens with one dashboard ([#4](https://github.com/imabee0/cw/pull/4))
- *(tui)* replace skim pickers with a ratatui Model/Msg/Update/View TUI ([#3](https://github.com/imabee0/cw/pull/3))

### Fixed

- address dashboard PR review follow-ups ([#5](https://github.com/imabee0/cw/pull/5))

### Other

- harden pipeline (locked deps, dist-check, secrets scan, smoke test) ([#6](https://github.com/imabee0/cw/pull/6))
