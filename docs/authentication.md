# Authentication

`confluence init` starts the guided login flow. It is the short form of
`confluence auth login`.

The wizard can:

- link to the appropriate Atlassian token page without opening a browser
- discover the Cloud ID required by scoped tokens
- create a dedicated Data Center PAT through the official API when available
- hide credential entry and verify access
- store the token in the operating-system keychain

New profiles start read-only. The wizard asks directly before enabling commands
that can change Confluence. Returning users can refresh only the stored credential
without stepping through unchanged connection settings.

If no credential service is available, setup offers an explicit protected-file
fallback instead of silently weakening storage. Onboarding never consumes
accidental piped input as answers.

## Scripted Cloud profile

```bash
CONFLUENCE_API_TOKEN="$CONFLUENCE_API_TOKEN" confluence auth login \
  --profile cloud \
  --provider cloud \
  --domain your-site.atlassian.net \
  --auth-type basic \
  --username you@example.com \
  --non-interactive

confluence doctor --profile cloud --space SPACEKEY
```

## Scripted Data Center profile

```bash
printf '%s' "$CONFLUENCE_PAT" | confluence auth login \
  --profile dc \
  --provider data-center \
  --domain https://confluence.example.com \
  --auth-type bearer \
  --token-stdin \
  --non-interactive

confluence doctor --profile dc --space SPACEKEY
```

When automatic PAT creation is unavailable, setup links to
`https://<your-host>/plugins/personalaccesstokens/usertokens.action`. The same page
is available under **Avatar → Settings → Personal access tokens**.

## Environment-driven mode

No stored profile is required:

```bash
export CONFLUENCE_DOMAIN=https://your-site.atlassian.net
export CONFLUENCE_PROVIDER=cloud
export CONFLUENCE_AUTH_TYPE=basic
export CONFLUENCE_EMAIL=you@example.com
export CONFLUENCE_TOKEN="$CONFLUENCE_API_TOKEN"

confluence doctor --space SPACEKEY
```

For a headless machine without a credential service, prefer environment-driven
mode. If persistent storage is necessary, explicitly accept the protected
config-file fallback with `--insecure-storage`. Existing inline-token profiles
remain readable and can be moved transactionally with
`confluence auth migrate`.

## Environment reference

- `CONFLUENCE_PROFILE`
- `CONFLUENCE_DOMAIN`
- `CONFLUENCE_PROVIDER`
- `CONFLUENCE_API_PATH`
- `CONFLUENCE_AUTH_TYPE`
- `CONFLUENCE_EMAIL` or `CONFLUENCE_USERNAME`
- `CONFLUENCE_API_TOKEN`, `CONFLUENCE_TOKEN`, `CONFLUENCE_PASSWORD`, or
  `CONFLUENCE_BEARER_TOKEN`
- `CONFLUENCE_TOKEN_KIND` (`classic` or `scoped`)
- `CONFLUENCE_CLOUD_ID` (required for scoped Cloud tokens)
- `CONFLUENCE_READ_ONLY`

`CONFLUENCE_PROVIDER` must be `cloud` or `data-center`.
`CONFLUENCE_AUTH_TYPE` must be `basic` or `bearer`.

Stored profiles live under the platform config directory selected by
`directories::ProjectDirs`.
