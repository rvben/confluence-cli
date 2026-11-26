# Publishing Markdown from CI

A CI publish needs the sidecars from the last successful deployment. They identify the remote pages and attachments and carry the baseline needed to detect independent edits. Keep that baseline in a persistent state file so each checkout can begin with the current deployment state.

For pages authored with Mermaid fences, the [Mermaid example](../examples/mermaid/README.md) shows how to render and publish them as attachments. Install the same pinned Mermaid CLI and browser in preview and publish jobs. When upgrading the renderer, use `--refresh-mermaid` with both commands to regenerate unchanged diagrams.

## Bootstrap once

1. Pull the full publishing tree from the target site. Review it, then commit the Markdown, attachments, and initial `.confluence.json` files. Keep each sidecar with its page.
2. Provision a private, durable directory outside the checkout. Use one state file per site and publishing tree, on a filesystem that supports file locks and atomic replacement. Choose a filename such as `handbook.json`. Confluence CLI uses the adjacent `.lock` file to coordinate publishers. The state file contains page metadata and the previous Markdown used for diffs. Allow state writes only from the publish job.
3. From a trusted checkout, run the first preview with the committed sidecars. Initialize the persistent file with the first publish.

```sh
confluence plan ./handbook --check-remote --diff
confluence apply ./handbook --state-file "$CONFLUENCE_STATE_FILE" --init-state
```

Use the same destination profile for both commands. See the [authentication guide](authentication.md) for headless configuration. Reserve write credentials for the publish job and use read-only credentials in trusted preview jobs. The state file is private to the publishing account. Run trusted preview jobs as that account or provide them with a private, read-only copy of the current file.

## Preview and publish

Use the full publishing tree and the same state file across jobs, even when the checkout path changes. Put the preview and publish commands in local make targets so CI and operators run the same commands. For a merge request, run the preview with read-only credentials:

```sh
confluence plan ./handbook --state-file "$CONFLUENCE_STATE_FILE" --check-remote --diff
```

If the job has access to the state file only, omit `--check-remote` to preview offline. A remote check reports conflicts that exist when the command runs. The publish job repeats the check before writing. If the preview reports that a publish is in progress, retry it after that job finishes.

After merge, serialize publishing jobs for each destination and tree. Publish the approved revision with:

```sh
confluence apply ./handbook --state-file "$CONFLUENCE_STATE_FILE"
```

The state file advances on success. Apply also writes sidecars into its working checkout. Keep those generated changes out of Git. A fresh checkout uses the persistent baseline for previously published pages and the checked-in sidecar for newly imported pages. A new Markdown page needs the usual `space_key` in its front matter properties. Once CI creates it, the file records its Confluence ID so future jobs update the same page.

State normally follows relative document paths. To rename or move a published folder, keep its checked-in `.confluence.json` beside `index.md` and update any relative links. If the sidecar matches a page ID and site in the publish state, the baseline follows the folder. This also carries state for descendants that keep their paths relative to that folder, including pages CI created without checked-in sidecars. A read-only preview uses the new paths in memory. A successful publish persists them and still checks for remote drift.

Renaming a sidecar-free page itself requires first pulling that page's sidecar from the target site. If a later pull puts a new page at a path CI still associates with a moved page, give the new page its own folder before publishing. Keep its sidecar with it and update relative links. For removals or overlapping renames between published pages, reconcile the baseline first.

## Recover a publish

If preflight finds remote drift, the file still contains the last published baseline. If an apply stops after preflight, the file may record an unfinished publish. Retain that file and the failed workspace. Use the reported completed actions and the page IDs saved in the pending state to investigate. Check the current remote pages if a create response was lost or a state write failed.

To adopt remote edits or recover from an interrupted publish or lost state, pause publishing jobs for this tree and pull a fresh snapshot into a separate directory. Reconcile the desired changes onto that snapshot, preserving the existing remote page IDs and the source paths. For pages that CI created, start from the pulled front matter so the source carries current metadata and drops the original `space_key` bootstrap hint. Commit the reconciled Markdown, attachments, and sidecars together and integrate that commit into the branch CI publishes. Run the bootstrap commands from a checkout of that branch at the reconciled revision with a new state file. After bootstrap succeeds, point CI at the new file, cancel any queued publishes of older revisions, and resume publishing. Reinitializing from stale sidecars can cause CI to create duplicate pages.

An interrupted Mermaid publish can leave a PNG attached before its source block
is written to the page. Reconcile the Markdown source and review the pulled
attachments before initializing the replacement state file. A PNG without a
matching image and source block is pulled as an ordinary attachment.
