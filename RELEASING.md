# Releasing chainz

Releases are tag-gated. Pushing a version tag publishes the workspace's six
crates and creates a GitHub release, but only after the release candidate
passes its tests and package check. All six share the one version in the
root `Cargo.toml`, and `cargo publish --workspace` uploads them in
dependency order (see [docs/architecture.md](docs/architecture.md#crates)).

The five library crates (`chainz-os`, `-core`, `-rpc`, `-serve`, `-tui`)
are published only because `chainz` depends on them. They have no stable
API: any release may break them, and their version always moves with
`chainz`'s.

## One-time repository setup

1. Create a protected GitHub environment named `release` with required
   reviewers.
2. Add a crates.io API token as the `CARGO_REGISTRY_TOKEN` environment secret.
3. Restrict tag creation for `v*` tags to maintainers.

## Release checklist

1. Confirm CI is green on `main`. The release job does not install anvil,
   so CI is where the fork tests run.
2. In `CHANGELOG.md`, replace `Unreleased` with the release date. The
   GitHub release notes are that version's section
   (`scripts/release-notes.sh VERSION` prints them), and the release job
   fails before publishing when it is missing.
3. Set the intended version in the root `Cargo.toml`, in both
   `[workspace.package]` and the five `[workspace.dependencies]` path
   entries, then run `cargo update -w` so `Cargo.lock` records it.
4. Run `CHAINZ_REQUIRE_TOOLS=1 scripts/check.sh` and
   `cargo package --locked --workspace`.
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
