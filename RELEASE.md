# Release Guide

## Prereqs

- Ensure CI is green on `main`.
- Confirm `CARGO_REGISTRY_TOKEN` is set in GitHub repo secrets for crates.io publish.
- Confirm the default repo in `install.sh` matches the GitHub repo name.

## Version Bump

1. Update version in `ra/Cargo.toml`.
2. Run a quick build:
   ```sh
   cargo build --release --manifest-path ra/Cargo.toml
   ```
3. Commit the version bump.

## Tag and Push

```sh
git tag vX.Y.Z
git push origin main --tags
```

## What Happens Next

- `.github/workflows/release.yml` builds and uploads binaries for:
  - `x86_64-unknown-linux-musl`
  - `aarch64-unknown-linux-musl`
  - `x86_64-apple-darwin`
  - `aarch64-apple-darwin`
- `.github/workflows/publish.yml` publishes to crates.io using `ra/Cargo.toml`.

## Verify the Release

- Check the GitHub Release assets include:
  - `ra-<target>.tar.gz` for each target
- Test install:
  ```sh
  curl -fsSL https://raw.githubusercontent.com/justinwangx/ra-cli/main/install.sh | sh
  ra --help
  ```

## Hotfix Release

- Repeat the version bump + tag steps with a new patch version.
