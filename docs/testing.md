# Testing Confluence CLI

The test strategy uses three layers with different jobs:

1. Unit and provider contract tests cover parsing, rendering, retries, pagination, error mapping, version drift, and request details.
2. A stateful localhost simulator runs the compiled `confluence` process through complete Cloud and Data Center lifecycles on every pull request.
3. Protected live Cloud tests detect upstream API, authentication, and tenant-behavior changes that a compatibility simulator cannot predict.

The simulator is deliberately a focused compatibility boundary, not a general Confluence clone. It implements only routes the public CLI exercises and derives responses from mutable state. Unknown routes return `501 Not Implemented` with the method and path, making missing coverage explicit.

The Proof Desk TUI keeps rendering tests deterministic with Ratatui's in-memory
terminal backend. The suite verifies the wide three-region workspace, medium
two-region workspace, compact unfold interaction, minimum-size recovery,
local-plan diffs, no-color output, keyboard state transitions, control-character
sanitization, and secret-like property redaction. Process-level contract tests
also verify TTY refusal and the explicit JSON recovery path. Provider unit tests
verify the TUI's bounded Cloud and Data Center collection requests; the shared
underlying APIs have broader simulator and protected-tenant coverage. Terminal
rendering itself does not need a real tenant.

## Local commands

Run the deterministic suite:

```bash
make test
```

Run the complete ignored lifecycle against a configured profile:

```bash
CONFLUENCE_E2E_PROFILE=my-profile \
CONFLUENCE_E2E_SPACE=TEST \
make test-e2e
```

The cleanup guard deletes created content after success and performs best-effort cleanup during failures. Cloud page deletion can still leave items in the space trash, so use a dedicated automation space.

## Protected Cloud automation

Create a GitHub Environment named `confluence-cloud-e2e`. Give it only a dedicated automation identity and a disposable space; do not reuse a personal token or production space.

Configure these environment secrets:

- `CONFLUENCE_BASE_URL`: the tenant base URL
- `CONFLUENCE_USERNAME`: the automation account email or username
- `CONFLUENCE_API_TOKEN`: the dedicated API token
- `CONFLUENCE_CLOUD_ID`: required only for a scoped token

Configure these environment variables:

- `CONFLUENCE_SPACE`: the dedicated space key
- `CONFLUENCE_TOKEN_KIND`: `classic` or `scoped`
- `CONFLUENCE_TOKEN_EXPIRES_AT`: token expiration as `YYYY-MM-DD`

The workflow validates every required setting without printing its value. It fails after expiration and emits a warning during the final 30 days. GitHub Environment reviewers can be added if manual full-suite runs should require approval.

The schedule is intentionally split:

- Monday through Saturday: minimal canary
- Sunday: complete lifecycle
- Manual dispatch: choose canary or full

Use the smallest Confluence scopes that support the lifecycle operations. Rotate the token before the warning window ends, update the expiration variable with it, and revoke the previous token after the next successful canary.
