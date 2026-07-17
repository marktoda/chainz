# Releasing chainz

Releases are intentionally tag-gated. Pushing a version tag publishes the
crate and creates a GitHub release only after the release candidate passes its
tests and package-content check.

## One-time repository setup

1. Create a protected GitHub environment named `release` with required
   reviewers.
2. Add a crates.io API token as the `CARGO_REGISTRY_TOKEN` environment secret.
3. Restrict tag creation for `v*` tags to maintainers.

## Release checklist

1. Confirm CI is green on `main`.
2. Replace `Unreleased` in `CHANGELOG.md` with the release date.
3. Set the intended version in `Cargo.toml`, then run `cargo update -w` so
   `Cargo.lock` records the same package version.
4. Run `cargo fmt --all -- --check`,
   `cargo clippy --locked --all-targets -- -D warnings`,
   `cargo test --locked --all-targets`, and `cargo package --locked`.
5. Merge the release-preparation change to `main`.
6. Create and push an annotated tag, for example:

   ```console
   git tag -a v0.4.0 -m "chainz 0.4.0"
   git push origin v0.4.0
   ```

The release workflow rejects a tag that does not exactly match the package
version or whose commit is not on `main`. Never reuse or move a published
version tag; prepare a new patch version instead.
