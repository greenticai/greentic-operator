Release Checklist (greentic-operator)

1. Bump version in `Cargo.toml`.
2. Commit changes and push.
3. Tag the release:
   - `git tag vX.Y.Z`
   - `git push origin vX.Y.Z`
4. Ensure CI runs `release.yml` on the tag:
   - Publishes crate to crates.io.
   - Builds release binaries for all targets.
   - Creates/updates GitHub Release `vX.Y.Z` and uploads assets.
5. Verify GitHub Release assets exist and are named:
   - `greentic-operator-x86_64-unknown-linux-gnu-vX.Y.Z.tgz`
   - `greentic-operator-aarch64-unknown-linux-gnu-vX.Y.Z.tgz`
   - `greentic-operator-x86_64-apple-darwin-vX.Y.Z.tgz`
   - `greentic-operator-aarch64-apple-darwin-vX.Y.Z.tgz`
   - `greentic-operator-x86_64-pc-windows-msvc-vX.Y.Z.zip`
   - `greentic-operator-aarch64-pc-windows-msvc-vX.Y.Z.zip`
6. Validate cargo-binstall:
   - `cargo binstall greentic-operator --version X.Y.Z --force --no-confirm`
   - It must download the release asset and must not fall back to source build.

Notes:
- The GitHub Release tag must be `vX.Y.Z` to match the binstall template.
- Each archive must contain a top-level folder named `{name}-{target}-v{version}` with the binary at its root.
