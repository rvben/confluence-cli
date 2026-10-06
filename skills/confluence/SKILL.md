---
name: confluence
description: Read, search, create, or edit Confluence pages and comments with the confluence CLI, preserving storage macros and validating bodies before writes.
---

# Confluence

Use the installed `confluence` CLI. Inspect only the command contracts needed
for the task with `confluence schema --command 'page update'` or command help.
Honor the user's selected account, space, page, and existing write authorization.

## Capabilities

Check `confluence --version` once per session. The CLI package is
`confluence-cli-rs` on PyPI and `confluence-cli` on crates.io; the executable is
`confluence`. Storage validation, conversion, and templates require v0.1.36 or
later. Verify unfamiliar capabilities with `validate --help`, `convert --help`,
or `template --help`, rather than repeatedly checking version or authentication.

If those commands are unavailable, prepare changes locally and report that the
write safeguards need an upgrade. Do not discover support by attempting a write.
For ordinary reads, an older CLI can still be used within its known contract.

## Editing a body

1. Fetch `confluence page get PAGE --show-body -o json`. Preserve the full
   response and the original `items[0].body_storage` locally. Record
   `items[0].version`; this is the base version for the eventual update.
2. Prefer a small edit of existing storage. Do not replace unrelated page
   content or flatten macros by converting a whole existing page to Markdown
   just to change one section. Unspecified title, parent, labels, and properties
   are preserved by `page update`.
3. Explicitly choose the input representation. Files ending in `.html` or `.xml`
   still default to Markdown unless `--format storage` is passed. Validate the
   candidate with `confluence validate --format storage --body-file candidate.xml`.
   Markdown candidates use `confluence validate --body-file candidate.md`.
4. For an authorized page update, use
   `confluence page update PAGE --version BASE_VERSION --format storage --body-file candidate.xml -o json`.
   `--version` is the version read in step 1, not the next version; it prevents
   an intervening edit from being silently overwritten. Stop on a conflict,
   fetch the latest body, and reconcile with the intended edit.
5. Read the successful update result. Fetch the body once for verification only
   if that result lacks the evidence needed to verify the requested change.
   Server normalization can change XML formatting, so verify the intended content
   and macro structure rather than requiring byte equality.

The same representation and validation rules apply to blog and comment writes.
Do not assume page-only flags, including `--version`, exist on comment commands;
inspect their command contract.

## Storage format

Confluence storage bodies are XML fragments, not browser HTML. Keep `ac:` macros
and `ri:` resource links intact. Multiple top-level elements are valid. Use
matching opening and closing elements, escape text outside CDATA, and close every
CDATA section. Local validation checks structure and known macro body types;
Confluence still decides entity resolution and custom macro semantics.

For new macro content, start with `confluence template code`, `template expand`,
or `template noformat`. They print storage XML without any API calls. The same
examples are bundled in [assets/code.xml](assets/code.xml),
[assets/expand.xml](assets/expand.xml), and [assets/noformat.xml](assets/noformat.xml).
Code/noformat bodies use `ac:plain-text-body`; expand/panel bodies use
`ac:rich-text-body`. A literal `]]>` in code must be split across CDATA sections:
`]]]]><![CDATA[>`.

If a Markdown candidate fails validation, save its generated XML with
`confluence convert --body-file candidate.md --output-file generated.xml`.
Validation failure returns exit 2 but retains that local artifact for inspecting
the reported line and column. Conversion does not contact Confluence. Existing
outputs are preserved unless `--force` is explicitly passed; input files are
never overwritten. `--allow-lossy` does not bypass write validation.

## Failure recovery

- `invalid_input` / exit 2: fix the local input. Generated-storage coordinates
  refer to the conversion artifact, not the Markdown source file.
- `api_error` / exit 5 with `Error parsing xhtml`: treat the rejection as a body
  defect, not a connectivity problem. Preserve the complete diagnostic, inspect
  its row/column, and correct the candidate locally. Allow at most one corrective
  remote attempt after local checks pass; if it still fails, stop and report the
  diagnostic, saved candidate, and unresolved question.
- A timeout or an error after a possible partial write: inspect structured error
  details and read current remote state before deciding whether another write is
  appropriate. Do not assume nothing changed.
- `conflict` / exit 7: reconcile the latest remote version before writing again.

Never submit the same parser-rejected body again unchanged. Do not write a
connectivity probe into an existing page: that replaces content and creates a
version. Use read-only commands for connectivity diagnostics when needed.

Never pipe a write command through `head`, `tail`, or `grep`. Capture full stdout
and stderr to local files, then immediately capture the process status. Inspect
shortened copies afterward if useful:

```sh
if confluence page update PAGE --version BASE_VERSION --format storage --body-file candidate.xml -o json >result.json 2>error.json; then
  write_exit_code=0
else
  write_exit_code=$?
fi
```

Branch on `write_exit_code`; structured `error.exit_code` corroborates it. The
`if`/`else` block also works with `errexit`, preserving status capture on failure.
Do not truncate parser diagnostics or
interpret a pipeline's final command status as the CLI status.
