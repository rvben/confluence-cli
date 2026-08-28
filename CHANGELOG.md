# Changelog

All notable changes to this project will be documented in this file.

## [0.1.20](https://github.com/rvben/confluence-cli/compare/v0.1.19...v0.1.20) - 2026-08-28

### Fixed

- **cli**: harden human and automation contracts ([1e6303d](https://github.com/rvben/confluence-cli/commit/1e6303d4863fef57d51f6f475c098374dfd4673a))

## Unreleased

### Added

- Complete human-readable help for every command and public argument.
- Executable CLI contract tests for help, parser errors, sync paths, output modes, and schemas.
- Scheduled PyPI installation smoke tests and Linux ARM release artifacts.

### Changed

- Make pagination, JSON success output, destructive confirmations, and agent schemas consistent.
- Use typed runtime errors instead of classifying failures from English substrings.
- Stage pulls as complete local snapshots, preflight whole-tree applies before writing, and retry only safe read requests.
- Return fully hydrated list and tree content, including labels and properties.

### Fixed

- Prevent body-writing commands from panicking on a Clap argument-ID collision.
- Reject missing or unreadable sync paths instead of reporting a successful no-op plan.
- Correct E2E binary discovery, auth table columns, schema output fields, and short output flags.
- Refuse dirty pull destinations unless `--force` is explicit, reject unsafe attachment names, and propagate attachment failures.
- Paginate labels, properties, attachments, comments, and `pull space --since` results beyond 200 items.
- Preserve transport error types, avoid replaying mutations, scope searches server-side, and report partial mutations structurally.
- Compute real plan diffs, validate destructive targets before prompting, and remove page-only parenting options from blog commands.
- Harden release ordering, GitHub SSH host verification, Homebrew metadata, and PyPI installation monitoring.

## [0.1.19](https://github.com/rvben/confluence-cli/compare/v0.1.18...v0.1.19) - 2026-08-28

### Added

- **packaging**: add PyPI distribution ([c99efe4](https://github.com/rvben/confluence-cli/commit/c99efe48d484d4d0ac08519301ebe561a0ec58a8))

### Fixed

- **release**: keep Homebrew tap loadable on ARM ([2690190](https://github.com/rvben/confluence-cli/commit/2690190275cfeac9e866e38b1889b0adc742f598))

## [0.1.18](https://github.com/rvben/confluence-cli/compare/v0.1.17...v0.1.18) - 2026-08-26

### Added

- **auth**: unify secure onboarding ([712fa5a](https://github.com/rvben/confluence-cli/commit/712fa5ad001aab3374cc07671511c63ed955f5c4))
- **onboarding**: protect interactive credentials ([080e865](https://github.com/rvben/confluence-cli/commit/080e86511a1d239b08f1f6990aa7d6a8c803f6f5))
- **cli**: unify interaction and safety contract ([3acb608](https://github.com/rvben/confluence-cli/commit/3acb608db0526317d64b04023db31da64f3cfdab))

### Fixed

- **auth**: open personal token page ([1bf46f3](https://github.com/rvben/confluence-cli/commit/1bf46f3d3d4ed587f135ff91d4cfe06b619b9664))
- **ci**: install pinned Rust components ([862f844](https://github.com/rvben/confluence-cli/commit/862f844f4261c49c524c64007ceac2443c873734))

## [0.1.17](https://github.com/rvben/confluence-cli/compare/v0.1.16...v0.1.17) - 2026-08-22

### Fixed

- **cli**: resolve clap output argument collision ([c221a97](https://github.com/rvben/confluence-cli/commit/c221a97df474ddfd30fe9bf810aa36b6dea602e7))

## [0.1.15](https://github.com/rvben/confluence-cli/compare/v0.1.14...v0.1.15) - 2026-06-20

### Added

- **schema**: fill missing output_fields for 14 commands ([9f84a3a](https://github.com/rvben/confluence-cli/commit/9f84a3a05871705f1abb641150eb9016924ab5cb))

## [0.1.14](https://github.com/rvben/confluence-cli/compare/v0.1.13...v0.1.14) - 2026-06-20

## [0.1.13](https://github.com/rvben/confluence-cli/compare/v0.1.12...v0.1.13) - 2026-06-11

### Added

- **clispec**: implement v0.2 compliance, score 28% -> 100% ([1556d16](https://github.com/rvben/confluence-cli/commit/1556d16057f91efb404bb8d3293ff1f11f573026))

### Fixed

- restore --json as hidden alias for --output json ([717eec0](https://github.com/rvben/confluence-cli/commit/717eec02b425cbefc3de81cb0f50b48906d2a871))

## [0.1.12](https://github.com/rvben/confluence-cli/compare/v0.1.11...v0.1.12) - 2026-04-03

## [0.1.11](https://github.com/rvben/confluence-cli/compare/v0.1.10...v0.1.11) - 2026-04-03

### Added

- add schema command, standardize CI, and bump rust-version ([f2cb5d5](https://github.com/rvben/confluence-cli/commit/f2cb5d50a3ddd663ce72f1e224a9184a0a7b77b9))

## [0.1.10](https://github.com/rvben/confluence-cli/compare/v0.1.9...v0.1.10) - 2026-04-02

### Fixed

- **page tree**: paginate through all children in list_children ([362e794](https://github.com/rvben/confluence-cli/commit/362e79458ccccf0a0710cc1585d046c2bfdb2f7f))

## [0.1.9](https://github.com/rvben/confluence-cli/compare/v0.1.8...v0.1.9) - 2026-04-01

### Fixed

- **space list**: paginate through all spaces when --all is passed ([369189e](https://github.com/rvben/confluence-cli/commit/369189eed49e871db5ad52d63a4dab27769b2282))

## [0.1.8](https://github.com/rvben/confluence-cli/compare/v0.1.7...v0.1.8) - 2026-04-01

### Added

- add --all flag and pagination hint to space/page/blog list ([c7574e1](https://github.com/rvben/confluence-cli/commit/c7574e1b0cf4413564d5bdbb00e993ec770de107))

## [0.1.7](https://github.com/rvben/confluence-cli/compare/v0.1.6...v0.1.7) - 2026-04-01

## [0.1.6](https://github.com/rvben/confluence-cli/compare/v0.1.5...v0.1.6) - 2026-04-01

### Added

- install binary as `confluence` instead of `confluence-cli` ([2f98eba](https://github.com/rvben/confluence-cli/commit/2f98eba4f1880e287d95b81852522f5fa54ccdd8))

## [0.1.5](https://github.com/rvben/confluence-cli/compare/v0.1.4...v0.1.5) - 2026-04-01

### Added

- **init**: show token in plain text during setup ([cdafbe0](https://github.com/rvben/confluence-cli/commit/cdafbe0298e7089bb5e98ab8274658233834f7e7))

## [0.1.4](https://github.com/rvben/confluence-cli/compare/v0.1.3...v0.1.4) - 2026-04-01

### Added

- **init**: rewrite interactive setup wizard with custom prompt helpers ([5b85c44](https://github.com/rvben/confluence-cli/commit/5b85c4484e2b70b408a17c58b65416aad0136da3))

## [0.1.3](https://github.com/rvben/confluence-cli/compare/v0.1.2...v0.1.3) - 2026-04-01

### Added

- add interactive init wizard ([b13bfb4](https://github.com/rvben/confluence-cli/commit/b13bfb46f6268cc7ea4d361f7f82a76ad4c4c3bf))

## [0.1.2](https://github.com/rvben/confluence-cli/compare/v0.1.1...v0.1.2) - 2026-03-31

### Added

- add page list, blog list, page move, page copy, comment update, search improvements ([f72c4d1](https://github.com/rvben/confluence-cli/commit/f72c4d1a0448040d5d8340ca27fb195e814e6e7d))

### Fixed

- replace all homebrew version placeholders ([eacd090](https://github.com/rvben/confluence-cli/commit/eacd090766ae11047b71545059a51e3f827ced63))

## [0.1.1](https://github.com/rvben/confluence-cli/compare/v0.1.0...v0.1.1) - 2026-03-31

### Fixed

- generate audit-clean homebrew formula ([4e1894f](https://github.com/rvben/confluence-cli/commit/4e1894f4564fd0bb6b2f83702a527f4db09e7811))

## [0.1.0] - 2026-03-31

First proper public release.

### Added

- Confluence Cloud and Data Center provider support.
- Markdown-first `pull`, `plan`, and `apply` workflow with frontmatter and sidecar metadata.
- Direct CLI commands for spaces, search, pages, blogs, attachments, labels, comments, and properties.
- `doctor` command for profile, auth, connectivity, and local sync-path validation.
- Shell completions and JSON output across the major command surface.
- Local Confluence Data Center Docker setup, backup/restore helpers, and live end-to-end test coverage.
- GitHub Actions CI and tagged release packaging for Linux and macOS.

### Improved

- Confluence-aware Markdown conversion for a broad set of built-in macros, typed resource parameters, attachments, layouts, and page-link cases.
- Remote version drift protection, noop stability after `pull` and `apply`, and safer link reconciliation for local Markdown paths.
- Runtime resiliency with HTTP retry and backoff handling for transient provider failures.
- Release packaging now excludes local Docker backup archives and other non-crate files.

## [0.0.1] - 2026-03-30

Reserved the crate name on crates.io.
