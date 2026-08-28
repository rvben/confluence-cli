# Releases

Vership is the release control plane for `confluence-cli`. GitHub Actions is the
execution plane for cross-platform builds, publication, provenance, and
downstream packaging.

Tagged releases publish the Rust crate as `confluence-cli` on crates.io and
native wheels as `confluence-cli-rs` on PyPI. The PyPI package installs the
same `confluence` executable and uses the `pypi` GitHub environment with PyPI
Trusted Publishing.

## Create a release

Start from a clean, up-to-date `main` branch and run:

```sh
vership preflight
vership bump patch # or minor/major
tarry cmd --timeout 20m -- vership verify
```

Vership runs `make check`, updates the package version and changelog, creates
the Conventional Commit release commit and tag, and pushes both. The tag starts
the release workflow. Use `vership release` only when the on-disk version was
intentionally set in advance.

## Failure policy

If a release fails before any release, artifact, package, checksum, or
attestation becomes public, delete and recreate the brief tag and retry the
same version. Once anything was published, preserve the tag and issue a patch
release for release-content corrections. Registry and downstream-packaging
jobs are separate recovery domains; retry only the failed job when possible.

## Dry runs

The Release workflow can be dispatched from `main` with `dry_run` enabled. It
uses the package version unless an explicit `version` is supplied, builds the
real artifacts, and exercises registry validation without publishing packages,
creating a GitHub release, generating attestations, or updating downstream
packaging.
