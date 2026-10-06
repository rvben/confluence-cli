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
| Tooling | `config show\|path`, `doctor`, `completions`, `schema` |

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

## Writing storage-format bodies

Body input defaults to Markdown, regardless of the filename. To submit raw
Confluence storage XML, specify the representation explicitly:

```bash
confluence page get 123 --show-body -o json
confluence validate --format storage --body-file body.xml -o json
confluence page update 123 --version 7 --format storage --body-file body.xml -o json
```

Use the returned `body_storage` as a starting point and keep a local copy before
editing. Replace `7` with the base version returned by that read; `--version`
uses the current version, not the next version. A concurrent edit causes a
conflict rather than allowing the stale candidate to overwrite it.
Storage bodies are XML fragments with Confluence `ac:` and `ri:`
elements. Code and noformat macros use `ac:plain-text-body`, normally containing
CDATA; expand and panel macros use `ac:rich-text-body`. Close every element and
CDATA section, escape text outside CDATA, and split literal `]]>` inside code
across adjacent CDATA sections.

`validate` is entirely offline and uses the same body checks as writes. It needs
no profile or credentials, accepts `--body`, `--body-file`, or stdin, and returns
`valid`, `input_format`, and `storage_bytes` in JSON mode. Input still defaults
to Markdown: `confluence validate --body-file page.md` checks the converted
storage, including any embedded `confluence-storage` blocks.

Page, blog, and comment body writes validate storage and converted Markdown locally
before profile resolution or API requests. Malformed XML and known macro body
type mismatches return `invalid_input` (exit 2), with body line and column in the
error details. Multiple top-level elements and implicit Confluence namespaces
are supported. Validation leaves named entity resolution and custom macro
semantics to Confluence; passing these checks does not guarantee server acceptance.
`--body-file -` also supports storage input from stdin.

Duplicate macro bodies and XML elements inside plain-text macro bodies are
also rejected. Empty bodies and custom macros are supported. Markdown errors
identify coordinates in the generated storage XML; they are not Markdown
source coordinates. `--allow-lossy` never bypasses XML validation. Local sync
`plan` and `apply` check generated storage too.

Start new macro content from the built-in examples:

```bash
confluence template code > code.xml
confluence template expand > expand.xml
confluence template noformat > noformat.xml
```

`template` always prints raw XML, even with `-o json`. These examples are checked
by the test suite and are available without network access.

Inspect generated storage when Markdown validation fails:

```bash
confluence convert --body-file page.md --output-file generated.xml -o json
```

`convert` is offline and saves the XML atomically before checking its structure.
On validation failure, it exits 2, prints no success document, and retains the
artifact. Structured error details include `artifact_saved: true` and `path`,
along with line and column. On success, JSON includes `valid`, `input_format`,
`storage_bytes`, and `path`. Existing output files require `--force` to replace;
input files and their symlink aliases cannot be used as the output. The output's
parent directory must exist. Conversion errors that prevent rendering do not
create an artifact. None of these commands bypass validation on remote writes.

If Confluence still returns `Error parsing xhtml`, fix the body locally before
retrying. Preserve full diagnostics and the CLI status; piping a write through
`head` can hide failure and truncate the useful parser error:

```bash
if confluence page update 123 --version 7 --format storage --body-file body.xml -o json >result.json 2>error.json; then
  write_exit_code=0
else
  write_exit_code=$?
fi
# Inspect result.json and error.json; write_exit_code contains the CLI's process exit code.
```

Use read-only commands for connectivity checks. A test body written to an
existing page replaces its contents and creates a version.

See the [agent workflow](agent-workflow.md) for upgrading, preserving a snapshot,
and applying a version-safe body edit.

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
