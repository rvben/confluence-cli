# confluence-cli

Markdown-sync-first Confluence CLI in Rust.

`confluence-cli` is built around a safe local workflow:

1. `pull` Confluence content into Markdown plus sidecar metadata.
2. `plan` exactly what would change remotely.
3. `apply` only when the diff is correct.

It also exposes direct page, blog, search, attachment, label, comment, and property commands for non-sync use cases.

## Status

Early release, but already live-verified against both Confluence Cloud and Confluence Data Center.

| Area | Cloud | Data Center | Notes |
| --- | --- | --- | --- |
| Auth, spaces, search, page/blog CRUD | Verified | Verified | Live e2e |
| Attachments, labels, properties, comments | Verified | Verified | Live e2e |
| `pull -> plan -> apply` sync flow | Verified | Verified | Includes drift refusal and noop checks |
| `doctor` environment/profile validation | Verified | Verified | Live checked |
| Markdown round-trip for common built-in macros | Verified | Verified | Unsupported cases preserve storage safely |

## Installation

From crates.io:

```bash
cargo install confluence-cli
```

From Homebrew:

```bash
brew tap rvben/tap
brew install rvben/tap/confluence-cli
```

Prebuilt macOS and Linux archives are published on the [GitHub releases page](https://github.com/rvben/confluence-cli/releases).

## Quick Start

Cloud profile:

```bash
confluence-cli auth login \
  --profile cloud \
  --provider cloud \
  --domain your-site.atlassian.net \
  --auth-type basic \
  --username you@example.com \
  --token "$CONFLUENCE_API_TOKEN" \
  --non-interactive

confluence-cli doctor --profile cloud --space SPACEKEY
```

Data Center profile:

```bash
confluence-cli auth login \
  --profile dc \
  --provider data-center \
  --domain http://localhost:8090 \
  --auth-type bearer \
  --token "$CONFLUENCE_PAT" \
  --non-interactive

confluence-cli doctor --profile dc --space TEST
```

Environment-driven mode also works without a stored profile:

```bash
export CONFLUENCE_DOMAIN=https://your-site.atlassian.net
export CONFLUENCE_PROVIDER=cloud
export CONFLUENCE_AUTH_TYPE=basic
export CONFLUENCE_EMAIL=you@example.com
export CONFLUENCE_TOKEN="$CONFLUENCE_API_TOKEN"

confluence-cli doctor --space SPACEKEY
```

## Markdown Sync Workflow

Pull a page tree:

```bash
confluence-cli pull tree SPACE:ParentPage ./docs/parent-page
```

Inspect the planned changes:

```bash
confluence-cli plan ./docs/parent-page
```

Apply the diff:

```bash
confluence-cli apply ./docs/parent-page
```

Local content is stored as:

- `<slug>/index.md`
- `<slug>/.confluence.json`
- `<slug>/attachments/*`

The frontmatter carries editable metadata like `title`, `type`, `labels`, `status`, `parent`, and `properties`. The sidecar stores remote ids, versions, hashes, and attachment mappings used for safe sync and drift detection.

## `doctor`

Use `doctor` before a first sync, in CI, or when a profile behaves unexpectedly.

```bash
confluence-cli doctor --profile cloud --space SPACEKEY --path ./docs/parent-page
```

It checks:

- config loading and profile resolution
- base URL and auth shape
- provider reachability
- optional space access
- optional local sync path planning

`doctor` exits non-zero on failures and supports `--json` for machine-readable checks.

## Commands

Top-level command groups:

- `auth login|status|logout`
- `profile add|list|use|remove`
- `space list|get`
- `search`
- `page get|tree|create|update|delete`
- `blog get|create|update|delete`
- `pull page|tree|space`
- `plan`
- `apply`
- `attachment list|download|upload|delete`
- `label list|add|remove`
- `comment list|add|delete`
- `property list|get|set|delete`
- `doctor`
- `completions`

All major commands accept `--json`.

## Auth And Environment Overrides

Stored profiles live under the local config directory used by `directories::ProjectDirs`.

Supported environment overrides:

- `CONFLUENCE_PROFILE`
- `CONFLUENCE_DOMAIN`
- `CONFLUENCE_PROVIDER`
- `CONFLUENCE_API_PATH`
- `CONFLUENCE_AUTH_TYPE`
- `CONFLUENCE_EMAIL` or `CONFLUENCE_USERNAME`
- `CONFLUENCE_API_TOKEN`, `CONFLUENCE_TOKEN`, `CONFLUENCE_PASSWORD`, or `CONFLUENCE_BEARER_TOKEN`
- `CONFLUENCE_READ_ONLY`

`CONFLUENCE_PROVIDER` must be `cloud` or `data-center`. `CONFLUENCE_AUTH_TYPE` must be `basic` or `bearer`.

## Shell Completions

```bash
confluence-cli completions bash > /usr/local/etc/bash_completion.d/confluence-cli
confluence-cli completions zsh > ~/.zsh/completions/_confluence-cli
confluence-cli completions fish > ~/.config/fish/completions/confluence-cli.fish
```

## Local Data Center

The repo includes a local Confluence Data Center stack for integration testing.

```bash
make confluence-start
make confluence-wait
make test-e2e
```

The default e2e path targets the local `local-dc` profile and the `TEST` space.

Available helpers:

- `make confluence-backup`
- `make confluence-restore`
- `make confluence-reset`
- `make confluence-logs`

Backups are written to:

- `docker/backup/confluence-data.tar.gz`
- `docker/backup/postgres-data.tar.gz`

The first boot after `make confluence-restore` can take several minutes before HTTP responds.

To point the e2e suite at another instance:

```bash
CONFLUENCE_E2E_PROFILE=other-profile CONFLUENCE_E2E_SPACE=SPACE make test-e2e
```

Or run fully env-driven:

```bash
CONFLUENCE_E2E_PROFILE= \
CONFLUENCE_E2E_BASE_URL=http://localhost:8090 \
CONFLUENCE_E2E_TOKEN="$CONFLUENCE_PAT" \
CONFLUENCE_E2E_PROVIDER=data-center \
CONFLUENCE_E2E_SPACE=TEST \
make test-e2e
```

## Release And CI

Local release gate:

```bash
make release-check
```

That runs formatting, clippy, tests, CLI smoke checks, and `cargo package`.

Local versioned releases use `vership`, matching the other CLI projects:

```bash
make release-patch
make release-minor
make release-major
```

`vership` uses `vership.toml` in this repo so `vership preflight` runs the stricter `make release-check` gate rather than the default Rust-only lint/test commands.

GitHub Actions is set up to:

- run CI on pushes and pull requests
- build tagged macOS and Linux release archives
- attach release archives and checksum files to GitHub releases
- update `rvben/tap` automatically on tagged releases when `HOMEBREW_TAP_TOKEN` is configured

## Markdown Fidelity

Remote canonical content stays in Confluence storage format. Markdown is the editable local representation.

The converter already handles a large set of common Confluence constructs directly, including:

- headings, lists, tables, code blocks, task lists, links, and attachments
- page links and typed page/user/space resource parameters
- layouts, panels, expand blocks, status, TOC-family macros, and search/navigation macros
- excerpt, excerpt-include, include-page, page-tree, page-tree-search, and page-index
- label/reporting/content-property/report-table/task-report families
- attachment preview and other common built-in macros

When a construct is unsupported or would be lossy, `confluence-cli` preserves the Confluence storage fragment instead of flattening the whole page.

## Known Limits

- Storage fidelity is strongest for supported built-in macros and generic resource-aware macro fallback. Unknown provider-specific macro behavior can still vary between Cloud and Data Center.
- CI does not run live tenant e2e by default. Use `make test-e2e` against a real instance for provider validation.
- `apply` refuses remote version drift unless `--force` is used.

## License

MIT
