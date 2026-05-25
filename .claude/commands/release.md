Release a new version of devme. Bumps version, commits, tags, and pushes.

Steps:
1. Read the current version from `Cargo.toml` (`workspace.package.version`)
2. Ask the user what the new version should be (suggest patch bump as default)
3. Update `version` in the root `Cargo.toml` under `[workspace.package]`
4. Run `cargo check` to update `Cargo.lock`
5. Commit: `release: v{version}`
6. Create annotated tag: `v{version}`
7. Push commit and tag to `origin main`
8. The GitHub Actions release workflow will automatically:
   - Build binaries for linux-x86_64, linux-aarch64, macos-aarch64
   - Create a GitHub Release with tarballs and checksums
   - Update the Homebrew formula in devme-sh/homebrew-tap
9. Show the Actions run URL so the user can follow progress
