# confluence-cli

Markdown-sync-first Confluence CLI in Rust.

## Status

Early release intended to reserve the crate name and establish the core CLI surface.

Current capabilities include:

- Confluence Cloud and Data Center profile support
- Space, search, page, blog, attachment, label, comment, and property commands
- Markdown-backed `pull`, `plan`, and `apply` workflows
- Local frontmatter plus sidecar metadata for sync safety

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
```

## Notes

- Remote canonical content format is Confluence storage format.
- Markdown conversion is conservative; Confluence-specific XML is preserved as raw storage blocks.
- This release is intentionally minimal and will be hardened against real tenants in follow-up work.

## License

MIT
