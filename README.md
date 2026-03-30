# confluence-cli

Markdown-sync-first Confluence CLI in Rust.

## Status

Usable early release with the core CLI surface in place and live e2e coverage verified against both Confluence Cloud and Data Center.

Current capabilities include:

- Confluence Cloud and Data Center profile support
- Space, search, page, blog, attachment, label, comment, and property commands
- Markdown-backed `pull`, `plan`, and `apply` workflows
- Local frontmatter plus sidecar metadata for sync safety
- Confluence-aware Markdown conversion for headings, lists, tables, task lists, attachment links, and attachment images

## Installation

```bash
cargo install confluence-cli
```

## Usage

```bash
confluence-cli --help
confluence-cli auth login
confluence-cli page --help
confluence-cli pull --help
confluence-cli completions zsh
```

## Shell Completions

```bash
confluence-cli completions bash > /usr/local/etc/bash_completion.d/confluence-cli
confluence-cli completions zsh > ~/.zsh/completions/_confluence-cli
confluence-cli completions fish > ~/.config/fish/completions/confluence-cli.fish
```

## Local Data Center

```bash
make confluence-start
make confluence-wait
make test-e2e
```

The default e2e path targets the local `local-dc` profile and the `TEST` space.

Available local-instance helpers:

- `make confluence-backup`
- `make confluence-restore`
- `make confluence-reset`
- `make confluence-logs`

Backups are written to:

- `docker/backup/confluence-data.tar.gz`
- `docker/backup/postgres-data.tar.gz`

The first boot after `make confluence-restore` can take several minutes before HTTP starts responding.

To point the e2e suite at another instance, override:

```bash
CONFLUENCE_E2E_PROFILE=other-profile CONFLUENCE_E2E_SPACE=SPACE make test-e2e
```

Or run fully env-driven:

```bash
CONFLUENCE_E2E_PROFILE= \
CONFLUENCE_E2E_BASE_URL=http://localhost:8090 \
CONFLUENCE_E2E_TOKEN=... \
CONFLUENCE_E2E_PROVIDER=data-center \
CONFLUENCE_E2E_SPACE=TEST \
make test-e2e
```

## Notes

- Remote canonical content format is Confluence storage format.
- Markdown conversion now covers common editable storage content directly, and unsupported Confluence XML is preserved as raw storage blocks instead of forcing whole-page raw exports.
- The ignored e2e lifecycle suite now runs against real Cloud and Data Center instances, including untouched `pull -> plan`, untouched `pull -> apply`, and fresh local-link reconciliation cases.
- The main remaining fidelity gap is richer Confluence-aware conversion for page layouts, page-link macros, mentions, and other advanced macros; unsupported nodes are preserved safely rather than rendered into editable Markdown.

## License

MIT
