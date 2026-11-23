# Publishing Markdown from CI

A CI publish needs the sidecars from the last successful deployment. They identify the remote pages and attachments and carry the baseline needed to detect independent edits. Keep that baseline in a persistent state file so each checkout can begin with the current deployment state.

## Bootstrap once

1. Pull the full publishing tree from the target site. Review it, then commit the Markdown, attachments, and initial `.confluence.json` files. Keep the path of each page stable across checkouts.
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

State follows relative document paths. Removing or moving a previously published document requires reconciling the baseline first. A change to a page title can leave its document path in place.

## Recover a publish

If preflight finds remote drift, the file still contains the last published baseline. If an apply stops after preflight, the file may record an unfinished publish. Retain that file and the failed workspace. Use the reported completed actions and the page IDs saved in the pending state to investigate. Check the current remote pages if a create response was lost or a state write failed.

To adopt remote edits or recover from an interrupted publish or lost state, pause publishing jobs for this tree and pull a fresh snapshot into a separate directory. Reconcile the desired changes onto that snapshot, preserving the existing remote page IDs and the source paths. For pages that CI created, start from the pulled front matter so the source carries current metadata and drops the original `space_key` bootstrap hint. Commit the reconciled Markdown, attachments, and sidecars together and integrate that commit into the branch CI publishes. Run the bootstrap commands from a checkout of that branch at the reconciled revision with a new state file. After bootstrap succeeds, point CI at the new file, cancel any queued publishes of older revisions, and resume publishing. Reinitializing from stale sidecars can cause CI to create duplicate pages.
