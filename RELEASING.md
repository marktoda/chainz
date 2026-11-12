# Releasing chainz

Releases are intentionally tag-gated. Pushing a version tag publishes the
workspace's six crates and creates a GitHub release, but only after the
release candidate passes its tests and package-content check. All six share
the one version in the root `Cargo.toml`, and `cargo publish --workspace`
uploads them in dependency order:

1. `chainz-os` (no workspace dependencies)
2. `chainz-core` (os)
3. `chainz-rpc` (os; not core)
4. `chainz-serve` (os, core, rpc)
5. `chainz-tui` (os, core, rpc; serve with its `testing` feature as a
   dev-dependency, so serve goes first)
6. `chainz` (all of the above)

## One-time repository setup

1. Create a protected GitHub environment named `release` with required
   reviewers.
2. Add a crates.io API token as the `CARGO_REGISTRY_TOKEN` environment secret.
3. Restrict tag creation for `v*` tags to maintainers.

## Release checklist

1. Confirm CI is green on `main`. The release job does not install anvil,
   so CI is where the fork tests run.
2. Replace `Unreleased` in `CHANGELOG.md` with the release date.
3. Set the intended version in the root `Cargo.toml`, in both
   `[workspace.package]` and the five `[workspace.dependencies]` path
   entries, then run `cargo update -w` so `Cargo.lock` records it.
4. Run `cargo fmt --all -- --check`,
   `cargo clippy --locked --workspace --all-targets -- -D warnings`,
   `CHAINZ_REQUIRE_TOOLS=1 cargo test --locked --workspace --all-targets`,
   and `cargo package --locked --workspace`.
5. Merge the release-preparation change to `main`.
6. Create and push an annotated tag, for example:

   ```console
   git tag -a v2.0.0 -m "chainz 2.0.0"
   git push origin v2.0.0
   ```

The release workflow rejects a tag that does not exactly match the package
version or whose commit is not on `main`. If a publish fails partway, the
crates already uploaded stay published: fix the problem and release the next
patch version rather than retrying the same one. Never reuse or move a published
version tag; prepare a new patch version instead.
