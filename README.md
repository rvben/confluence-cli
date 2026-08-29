<div align="center">
  <a href="https://github.com/rvben/confluence-cli">
    <img src="https://raw.githubusercontent.com/rvben/confluence-cli/main/assets/logo.svg" width="128" alt="Prompt Leaf: a document containing a command prompt, the confluence-cli logo">
  </a>
  <h1>confluence-cli</h1>
  <p><strong>Confluence, readable and scriptable.</strong></p>
  <p>Read, search, browse, automate, and edit Confluence Cloud or Data Center—without leaving the terminal.</p>
  <p>
    <a href="https://github.com/rvben/confluence-cli/actions/workflows/ci.yml"><img src="https://github.com/rvben/confluence-cli/actions/workflows/ci.yml/badge.svg" alt="CI status"></a>
    <a href="https://codecov.io/gh/rvben/confluence-cli"><img src="https://codecov.io/gh/rvben/confluence-cli/graph/badge.svg" alt="Code coverage"></a>
    <a href="https://crates.io/crates/confluence-cli"><img src="https://img.shields.io/crates/v/confluence-cli.svg" alt="crates.io version"></a>
    <a href="https://pypi.org/project/confluence-cli-rs/"><img src="https://img.shields.io/pypi/v/confluence-cli-rs.svg" alt="PyPI version"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-2e4f9b.svg" alt="MIT license"></a>
  </p>
  <p>
    <a href="#install">Install</a> ·
    <a href="#quick-start">Quick start</a> ·
    <a href="#read-and-explore">Read and explore</a> ·
    <a href="#proof-desk-tui">Proof Desk</a> ·
    <a href="#command-surface">Commands</a>
  </p>
</div>

`confluence-cli` makes Confluence directly useful from the shell. Read and browse without creating a local sync directory. Pipe stable JSON into scripts and agents. When content needs to change, work in Markdown and publish through an explicit plan-and-apply boundary.

| Job | Workflow | Result |
| --- | --- | --- |
| Read and browse | `space`, `search`, `page`, `blog`, `tui` | Scannable terminal output and a keyboard-first reader |
| Query and automate | JSON output, `--fields`, `schema` | Stable, token-efficient data for scripts and agents |
| Edit and publish | `pull → edit → plan → apply` | Reviewable Markdown changes with remote-drift protection |

## Install

Choose the package manager that fits your environment. Every distribution installs the `confluence` executable.

With [`uv`](https://docs.astral.sh/uv/) from PyPI:

```bash
uv tool install confluence-cli-rs
```

With Homebrew:

```bash
brew install rvben/tap/confluence-cli
```

With Cargo:

```bash
cargo install confluence-cli
```

The PyPI distribution is named `confluence-cli-rs`; the crate is named `confluence-cli`. Prebuilt macOS and Linux archives for Intel and ARM are available from [GitHub Releases](https://github.com/rvben/confluence-cli/releases).

## Quick start

Connect a profile and check access:

```bash
confluence init
confluence doctor --space SPACEKEY
```

Then read, search, or browse immediately—no pull required:

```bash
confluence search 'release notes' --space DOCS
confluence page tree 'DOCS:Handbook'
confluence tui --space DOCS
```

The same commands become structured input when piped:

```bash
confluence search 'release notes' --space DOCS \
  | jq '.items[] | {title, web_url}'
```

<p align="center">
  <a href="https://raw.githubusercontent.com/rvben/confluence-cli/main/assets/proof-desk.png">
    <img src="https://raw.githubusercontent.com/rvben/confluence-cli/main/assets/proof-desk.png" width="1020" alt="Proof Desk showing a Confluence page tree beside a readable page galley">
  </a>
</p>
<p align="center"><sub>Proof Desk in its medium-width Browse layout, rendered from the deterministic test fixture. <a href="https://raw.githubusercontent.com/rvben/confluence-cli/main/assets/proof-desk.png">Open full resolution.</a></sub></p>

When you want to edit, pull a page tree to Markdown and inspect the local plan:

```bash
confluence pull tree 'SPACEKEY:Parent Page' ./docs/parent-page
$EDITOR ./docs/parent-page/parent-page--123/index.md
confluence plan ./docs/parent-page --diff
```

When the plan is correct, apply it:

```bash
confluence apply ./docs/parent-page
```

`plan` reads only the Markdown and sidecar state on disk. `apply` validates the complete local tree and checks every remote version before writing. If Confluence changed after the pull, the apply is refused unless you explicitly choose `--force`.

## Read and explore

The direct commands cover everyday Confluence retrieval without requiring a sync directory:

```bash
confluence space list
confluence search 'on-call rotation' --space OPS
confluence page get 'OPS:Incident handbook' --show-body
confluence page tree 'OPS:Runbooks'
confluence blog list ENG
```

References accept a numeric content ID, a Confluence URL, or `SPACE:Title`. Search accepts plain text by default and full Confluence Query Language with `--cql`.

On a terminal, search results, lists, and page metadata are formatted for scanning. `page get --show-body` includes the canonical storage-format body for agents and transformations; Proof Desk turns that content into a readable page for humans.

When stdout is piped, data commands emit one JSON document:

```bash
confluence search 'release notes' --space DOCS \
  | jq '.items[] | {title, web_url}'
```

For an interactive reading experience, `confluence tui` opens Proof Desk on the first visible space. The `--space` option starts in a specific space, and `o` opens the selected page in Confluence.

## Proof Desk TUI

Proof Desk is a keyboard-first, read-only workspace for reading and browsing Confluence. Add a local path when you also want to review a sync plan:

```bash
confluence tui
confluence tui --space DOCS
confluence tui --space DOCS --path ./docs/handbook
```

Browse mode combines the page hierarchy, a readable Markdown proof, and an outer margin for metadata, labels, attachments, comments, and content properties. Review mode presents the local plan and unified body diffs in the same layout.

The TUI never applies changes and does not contact Confluence to detect drift. `confluence apply` remains the separate, remote-aware write boundary. Passing `--delete-remote` only includes attachment deletions in the local review plan.

| Key | Action |
| --- | --- |
| Arrow keys or `j` / `k` | Move through content |
| `Enter` | Unfold the selected proof |
| `Tab` | Switch between Browse and Review |
| `1`–`4` | Change the margin evidence |
| `s` | Choose a space |
| `p` | Choose a local sync directory |
| `o` | Open the selected page in Confluence |
| `?` | Show the complete keyboard map |

Wide terminals show all three regions. Compact terminals reveal complete proofs and margins on demand. Proof Desk requires interactive stdin and stdout and honors `--no-color`.

## Edit and publish with Markdown

### Pull

Export one page, a page tree, or an entire space:

```bash
confluence pull page 'DOCS:Getting Started' ./docs/getting-started
confluence pull tree 'DOCS:Handbook' ./docs/handbook
confluence pull space DOCS ./docs
```

Pulls are staged beside the destination and installed as one snapshot. A pull refuses to replace local Markdown changes or unmanaged files. Use `--force` only when the remote snapshot should replace the entire destination.

Filtered `pull space --since ...` exports require a new or empty destination because a partial result cannot safely replace a complete snapshot. Attachment names are confined to their page's `attachments/` directory.

### Edit

Each page has a predictable local layout:

```text
<slug>--<content-id>/
├── index.md
├── .confluence.json
└── attachments/
```

The Markdown frontmatter contains editable metadata such as `title`, `type`, `labels`, `status`, and `properties`. Move a page directory beneath its desired local parent to reparent it; the `parent` frontmatter field is informational.

The sidecar records remote IDs, versions, hashes, and attachment mappings. It is the baseline that makes offline planning and drift detection possible.

### Plan

```bash
confluence plan ./docs/handbook --diff
```

`plan` validates the complete tree and compares it with the state captured by `pull`. It never contacts Confluence and never writes remotely.

For an interactive review, open the same directory in the Proof Desk:

```bash
confluence tui --space DOCS --path ./docs/handbook
```

### Apply

```bash
confluence apply ./docs/handbook
```

Before its first mutation, `apply` validates every local document and preflights every remote version. Versioned updates continue to reject drift that occurs during the apply. If an API failure leaves a partial remote mutation, the CLI reports the completed actions in structured error details so automation can reconcile safely.

Remote attachment deletions are opt-in with `--delete-remote`. Conditional overwrites require an explicit `--force` or `--replace` where supported.

## Authentication

`confluence init` starts the guided login flow. It opens the appropriate token
page, discovers scoped-token details, verifies access, and stores credentials in
the operating-system keychain. Cloud and Data Center profiles share the same
workflow.

Scripts and headless machines can use explicit profiles or environment-only
credentials. See the [authentication guide](docs/authentication.md) for Cloud,
Data Center, CI, keychain, and environment examples.

## Command surface

Reading, browsing, direct updates, and Markdown publishing are all first-class:

| Area | Commands |
| --- | --- |
| Accounts | `auth login\|status\|logout\|migrate`, `profile add\|list\|use\|remove` |
| Discovery | `space list\|get`, `search`, `page list\|get\|tree`, `blog list\|get` |
| Content | `page move\|create\|update\|delete`, `blog create\|update\|delete` |
| Page data | `attachment`, `label`, `comment`, and `property` command groups |
| Markdown | `pull page\|tree\|space`, `plan`, `tui`, `apply` |
| Tooling | `doctor`, `completions`, `schema` |

On a terminal, `--output auto` produces readable text. When piped, data
commands produce one JSON document. Agents can request one token-efficient,
versioned command contract without loading the complete CLI surface:

```bash
confluence schema --command 'page get'
```

See the [CLI reference](docs/cli-reference.md) for output modes, exit codes,
shell completions, Markdown fidelity, and the complete command map.

## Status and compatibility

The project is an early release, live-verified against Confluence Cloud and Data
Center.

| Area | Cloud | Data Center | Evidence |
| --- | --- | --- | --- |
| Reading, search, and content CRUD | Verified | Verified | Live end-to-end lifecycle |
| Attachments, labels, properties, comments | Verified | Verified | Live end-to-end lifecycle |
| `pull → plan → apply` | Verified | Verified | Drift refusal and no-op checks |
| Proof Desk | Shared APIs | Shared APIs | Deterministic responsive render tests |

The deterministic simulator runs on every pull request. Protected live checks
exercise real provider lifecycles. The [testing guide](docs/testing.md) explains
the confidence model and automation.

### Known limits

- Unknown provider-specific macros can behave differently between Cloud and
  Data Center; unsupported storage is preserved instead of silently flattened.
- `apply` refuses remote-version drift unless `--force` is explicit.

## Documentation

- [Authentication and profiles](docs/authentication.md)
- [CLI, output, schema, and Markdown reference](docs/cli-reference.md)
- [Testing and live-provider confidence](docs/testing.md)
- [Release and recovery runbook](docs/releases.md)

## Development and releases

```bash
make test           # deterministic unit, contract, and simulator tests
make release-check  # formatting, clippy, tests, smoke checks, and packaging
```

Live-provider tests and the local Data Center stack are documented in the
[testing guide](docs/testing.md).

## License

[MIT](LICENSE)
