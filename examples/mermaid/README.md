# Publish Mermaid diagrams from Markdown

[Open the sample page](page/index.md) on GitHub to see its Mermaid diagrams.
Set `confluence-cli.render-mermaid: true` in the page's front matter
`properties` to render its Mermaid fences when planning or applying. Use
top-level `mermaid` fences. Give diagrams an `accTitle` for image alt text and
describe the important steps in surrounding prose.

## Preview offline

Install the [Mermaid CLI](https://github.com/mermaid-js/mermaid-cli) with
its browser runtime and put `mmdc` on your PATH. Pin the same renderer and
browser versions in preview and publish jobs. This example was verified
with Mermaid CLI 12.2.0.

```sh
confluence plan examples/mermaid/page
```

The plan shows a new page and two PNG uploads. Diagrams render into a
temporary directory. Rendering errors name the Markdown file and the
opening line of the diagram.

## Publish to a test space

Copy the example page to a directory you use for testing. Replace
`EXAMPLE` in the copy with a space key where your existing Confluence
profile can create pages and add attachments.

```sh
confluence --profile my-profile plan ./order-platform-example --check-remote
confluence --profile my-profile apply ./order-platform-example
```

Confluence displays each PNG as a page attachment, with its editable
Markdown fence in a collapsed **Mermaid source (confluence-cli)** block.
Keep that block with the image so a pull can restore the fence. Edit the
fence in Git and apply again to regenerate the image.

The CLI assigns generated PNG names such as `confluence-cli-mermaid-1.png`
and keeps them in the page's sync state. It skips names already used by
ordinary attachments. Reordering unchanged fences keeps their image
attachments. Use a distinct `accTitle` on each diagram to keep its image
history when you move and edit it in the same change. To remove attachments
for deleted diagrams, review the deletion list with `plan --delete-remote`
before applying with the same flag.

If a diagram's source is unchanged and the published image is in the
sync baseline, planning and applying reuse that image without starting
the renderer. An edit to the source block in Confluence invalidates that
image on the next pull. Use `--refresh-mermaid` with `plan` and `apply`
to regenerate every diagram after changing the renderer or its fonts.

If an image upload is interrupted during a local apply, keep the page's
`.confluence.json` and retry from the same directory and profile. The CLI
checks whether Confluence received the attempted image before replacing it.

Follow the [CI publishing guide](../../docs/ci-publishing.md) to keep
the deployment state across checkouts, including recovery after an interrupted
CI publish.
