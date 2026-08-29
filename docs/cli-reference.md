# CLI reference

Reading, browsing, direct updates, and Markdown publishing are all part of the
public command surface.

| Area | Commands |
| --- | --- |
| Accounts | `auth login\|status\|logout\|migrate`, `profile add\|list\|use\|remove` |
| Discovery | `space list\|get`, `search`, `page list\|get\|tree`, `blog list\|get` |
| Content | `page move\|create\|update\|delete`, `blog create\|update\|delete` |
| Page data | `attachment`, `label`, `comment`, and `property` command groups |
| Markdown | `pull page\|tree\|space`, `plan`, `tui`, `apply` |
| Tooling | `doctor`, `completions`, `schema` |

Where a command accepts `REFERENCE`, pass a numeric content ID, a Confluence
URL, or `SPACE:Title`. Destructive operations require interactive confirmation
or `--yes`.

For sensitive content, prefer `--body-file` or standard input over `--body`,
because command-line arguments may be visible to other local processes.

Use `confluence --help` or `confluence <command> --help` for the complete option
reference.

## Output contract

Data commands support `--output auto|text|json`, `--quiet`, and `--no-color`.
Auto output is readable text on a terminal and one JSON document when piped.
`--json` remains a hidden compatibility alias. `completions` emits an opaque
shell script; `schema` always emits JSON.

List commands consistently support `--limit`, `--offset`, and JSON-only
`--fields`. Search reports an exact `total` when Confluence provides one and
`null` otherwise.

Errors share a stable exit-code contract:

| Code | Meaning |
| ---: | --- |
| 2 | Invalid input |
| 3 | Authentication or authorization |
| 4 | Not found |
| 5 | API or network failure |
| 6 | Rate limited |
| 7 | Conflict or remote drift |

Agents and scripts can request one token-efficient command contract without
loading the entire CLI surface:

```bash
confluence schema --command 'page get'
```

The response contract is versioned independently in the schema.

## Markdown fidelity

Confluence storage format remains the remote canonical representation;
Markdown is the editable local representation.

The converter handles common Confluence constructs directly, including:

- headings, lists, tables, code blocks, task lists, links, and attachments
- page links and typed page, user, and space resource parameters
- layouts, panels, expand blocks, status, TOC-family, search, and navigation
  macros
- excerpt, include-page, page-tree, label, reporting, and task-report families
- attachment previews and other common built-in macros

When a construct is unsupported or would be lossy, `confluence-cli` preserves
its storage fragment instead of flattening the entire page.

## Shell completions

```bash
confluence completions bash > /usr/local/etc/bash_completion.d/confluence
confluence completions zsh > ~/.zsh/completions/_confluence
confluence completions fish > ~/.config/fish/completions/confluence.fish
```
