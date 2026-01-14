# Release Guide

This repo releases via GitHub Actions (see `.github/workflows/release.yml` and `.github/workflows/publish.yml`).

- Ensure CI is green on `main`.
- Confirm secrets are set in GitHub repo secrets:
  - `CARGO_REGISTRY_TOKEN` (for crates.io publish via `publish.yml`)
  - `NPM_TOKEN` (a **granular** npm automation token for the initial npm publish via `release.yml`)

1. Update versions (must match):
   - `ra/Cargo.toml` (`version = "X.Y.Z"`)
   - `ra/npm-package/package.json` (`"version": "X.Y.Z"`)
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

## Publish to npm (first publish / backfill for an existing tag)

If the GitHub Release already exists for a tag (e.g. `v0.1.7`) and you want to publish the npm wrapper package for that version, run the `npm-publish` workflow with an explicit tag input:

```sh
gh workflow run npm-publish -f tag=vX.Y.Z
```

Note: if you have a branch named `vX.Y.Z`, `gh` may resolve `--ref vX.Y.Z` to the branch rather than the tag. Prefer fully-qualified refs if you need `--ref`:

```sh
gh workflow run release --ref refs/tags/vX.Y.Z
```

## Verify

- Confirm the GitHub Release has `ra-<target>.tar.gz` assets.
- Confirm crates.io publish succeeded (`ra-cli`).
- Confirm npm publish succeeded (`ra-cli`).
- Smoke test installs:

```sh
# GitHub release installer
curl -fsSL https://raw.githubusercontent.com/justinwangx/ra-cli/main/install.sh | sh
ra --help
```

```sh
# npm (installs ra by downloading a matching GitHub Release binary)
npm i -g ra-cli
ra --help
```
