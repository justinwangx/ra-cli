# Release Guide

This repo releases via GitHub Actions (see `.github/workflows/release.yml` and `.github/workflows/publish.yml`).

- Ensure CI is green on `main`.
- Confirm `CARGO_REGISTRY_TOKEN` is set in GitHub repo secrets for crates.io publish.

1. Update the version in `ra/Cargo.toml`.
2. Run local checks:

```sh
./scripts/release_prep.sh
```

3. Commit the version bump (and any resulting `Cargo.lock` change).
4. Tag and push:

```sh
git tag vX.Y.Z
git push origin --tags
```

## Verify

- Confirm the GitHub Release has `ra-<target>.tar.gz` assets.
- Smoke test install:

```sh
curl -fsSL https://raw.githubusercontent.com/justinwangx/ra-cli/main/install.sh | sh
ra --help
```
