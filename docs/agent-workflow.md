# Confluence editing workflow

## Upgrade and adopt the skill

The storage authoring workflow requires confluence-cli v0.1.36 or later. Once
that release is published, an existing uv installation can be upgraded with:

```sh
uv tool upgrade confluence-cli-rs
confluence --version
confluence validate --help
```

For a fresh install, use `uv tool install confluence-cli-rs`. The package name is
`confluence-cli-rs` on PyPI; the executable is `confluence`. Existing accounts and
profiles can be retained. Authentication does not need to be repeated to use
`validate`, `convert`, or `template`: all three work offline.

The repository includes a self-contained [Confluence skill](../skills/confluence/SKILL.md).
Copy the entire `skills/confluence` folder into your agent's skill directory
(for Codex, usually `~/.codex/skills/confluence`). For an existing skill such as
`~/.vero/skills/confluence/SKILL.md`, merge the storage, editing, and recovery
guidance into it, preserving account-specific instructions and authorization
rules. The skill's bundled assets are the same examples embedded in the CLI.

## Edit an existing storage body

Fetch the original body and its version once. Keep the response as a local
snapshot. The following example uses `jq` to extract the fields without changing
the stored body's bytes:

```sh
page_ref=123
work_dir=$(mktemp -d)
if confluence page get "$page_ref" --show-body -o json >"$work_dir/page.json" 2>"$work_dir/read-error.json"; then
  read_exit_code=0
else
  read_exit_code=$?
  cat "$work_dir/read-error.json" >&2
  exit "$read_exit_code"
fi
if ! jq -ej '.items[0].body_storage | select(type == "string")' "$work_dir/page.json" >"$work_dir/original.xml"; then
  exit 2
fi
if ! base_version=$(jq -er '.items[0].version | select(type == "number")' "$work_dir/page.json"); then
  exit 2
fi
cp "$work_dir/original.xml" "$work_dir/candidate.xml"
```

Edit the candidate locally and preserve unrelated page content and macros.
Validate before the authorized update:

```sh
if confluence validate --format storage --body-file "$work_dir/candidate.xml" -o json; then
  validate_exit_code=0
else
  validate_exit_code=$?
  exit "$validate_exit_code"
fi
```

Only proceed when validation succeeds. Use the version read in the snapshot,
not the next version or a newly fetched version with the stale candidate:

```sh
if confluence page update "$page_ref" --version "$base_version" --format storage --body-file "$work_dir/candidate.xml" -o json >"$work_dir/result.json" 2>"$work_dir/error.json"; then
  write_exit_code=0
else
  write_exit_code=$?
fi
```

This status-capture pattern also works in shells using `set -e`. Inspect the
complete result or error after the command. Do not pipe writes through `head`,
`tail`, or `grep`. On a conflict, fetch and reconcile the latest body; changing
only the version number would discard intervening edits.

The successful update result includes the new version and storage body. Use it
to verify the change before adding another read. Confluence may normalize XML
formatting, so check intended content and macro structure rather than byte
equality. The original snapshot is recovery material, not permission to restore
it over subsequent edits.

## Start new content or inspect a Markdown failure

Use tested templates for new macros:

```sh
confluence template code > code.xml
confluence template expand > expand.xml
confluence template noformat > noformat.xml
```

Each command prints raw XML and needs no credentials. Code/noformat use
`ac:plain-text-body`; expand/panel use `ac:rich-text-body`. The code example
demonstrates how to preserve a literal `]]>` by splitting adjacent CDATA sections.

For Markdown input, storage validation is mandatory after conversion, including
embedded `confluence-storage` blocks. To inspect generated XML:

```sh
confluence convert --body-file candidate.md --output-file generated.xml -o json
```

If validation fails, the command returns exit 2 and leaves `generated.xml` for
inspection. Its error reports the saved path, line, and column. Correct the
Markdown source and convert to a new artifact, or edit the generated XML and
validate it explicitly with `--format storage`. Reusing an output path requires
`--force`; input files cannot be overwritten. Conversion failures that prevent
rendering leave no new artifact.

## Stop the parser-error loop

An API response containing `Error parsing xhtml` identifies a body defect, not
an authentication or connectivity fault. Preserve the full error and candidate,
fix the reported row/column locally, then allow at most one corrective remote
attempt after validation passes. If it still fails, surface the diagnostic and
candidate rather than repeatedly submitting variations.

Never write a test-connectivity body to an existing page. Use read-only
diagnostics if connectivity is actually uncertain. After a timeout or partial
write error, inspect current remote state before retrying. Local validation
checks XML structure and known macro body types; named entity resolution and
custom macro semantics remain server-side decisions.
