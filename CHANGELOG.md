# Changelog

All notable changes to this project will be documented in this file.





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
