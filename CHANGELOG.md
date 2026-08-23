# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
