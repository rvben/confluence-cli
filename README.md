# confluence-cli

Markdown-sync-first Confluence CLI in Rust.

## Status

Usable early release with the core CLI surface in place and live e2e coverage verified against both Confluence Cloud and Data Center.

Current capabilities include:

- Confluence Cloud and Data Center profile support
- Space, search, page, blog, attachment, label, comment, and property commands
- Markdown-backed `pull`, `plan`, and `apply` workflows
- Local frontmatter plus sidecar metadata for sync safety
- Confluence-aware Markdown conversion for headings, lists, tables, task lists, attachment links/images, page links, user mentions, status macros, layouts, excerpt macros, excerpt-include macros, include-page macros, page-tree macros, page-tree-search macros, page-index macros, spaces-list macros, space-details macros, space-attachments macros, attachment preview macros (`view-file`, `viewdoc`, `viewxls`, `viewppt`), Live Search macros, TOC zone macros, content-properties macros, content-properties-report macros, content-report-table macros, task-report macros, attachments macros, blog-posts macros, contributors macros, contributors-summary macros, recently-updated-dashboard macros, content-by-label macros, recently-updated macros, labels-list macros, popular-labels macros, related-labels macros, recently-used-labels macros, gallery macros, favorite-pages macros, change-history macros, profile macros, status-list macros, network macros, children macros, TOC macros, panel macros, expand macros, code macros, noformat macros, and generic `confluence-macro <name>` blocks for unsupported plain-parameter macros

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
- Markdown conversion now covers common editable storage content directly, including page links, user mentions, status macros, layouts, excerpt macros, excerpt-include macros, include-page macros, page-tree macros, page-tree-search macros, page-index macros, spaces-list macros, space-details macros, space-attachments macros, attachment preview macros (`view-file`, `viewdoc`, `viewxls`, `viewppt`), Live Search macros, TOC zone macros, content-properties macros, content-properties-report macros, content-report-table macros, task-report macros, attachments macros, blog-posts macros, contributors macros, contributors-summary macros, recently-updated-dashboard macros, content-by-label macros, recently-updated macros, labels-list macros, popular-labels macros, related-labels macros, recently-used-labels macros, gallery macros, favorite-pages macros, change-history macros, profile macros, status-list macros, network macros, children macros, TOC macros, panel macros, expand macros, code macros, noformat macros, and generic `confluence-macro <name>` blocks for unsupported plain-parameter macros; unsupported Confluence XML is preserved as raw storage blocks instead of forcing whole-page raw exports.
- The ignored e2e lifecycle suite now runs against real Cloud and Data Center instances, including untouched `pull -> plan`, untouched `pull -> apply`, and fresh local-link reconciliation cases.
- The main remaining fidelity gap is richer Confluence-aware conversion for advanced macros beyond the currently supported page-link, mention, status, layout, excerpt, excerpt-include, include-page, page-tree, page-tree-search, page-index, spaces-list, space-details, space-attachments, attachment preview (`view-file`, `viewdoc`, `viewxls`, `viewppt`), Live Search, TOC zone, content-properties, content-properties-report, content-report-table, task-report, attachments, blog-posts, contributors, contributors-summary, recently-updated-dashboard, content-by-label, recently-updated, labels-list, popular-labels, related-labels, recently-used-labels, gallery, favorite-pages, change-history, profile, status-list, network, children, TOC, panel, expand, code, noformat, and generic plain-parameter macro cases; unsupported nodes are preserved safely rather than rendered into editable Markdown.

## License

MIT
