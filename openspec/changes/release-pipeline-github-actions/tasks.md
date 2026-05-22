## 1. Workflow file

- [ ] 1.1 Create `.github/workflows/release.yml` with a `on: push: tags: ['v*']` trigger. Single workflow with three logical stages: `test` (job), `build` (matrix job with `needs: test`), `publish` (job with `needs: build`).
- [ ] 1.2 `test` job: `runs-on: ubuntu-latest`. Steps: checkout, install stable Rust via `dtolnay/rust-toolchain@stable`, `cd autocoder && cargo test --release --all-features`. Any test failure halts the workflow before any binaries are built.
- [ ] 1.3 `build` matrix job: `strategy.matrix.target` covers the three triples. For Linux x86_64: native build on ubuntu-latest. For Linux aarch64: ubuntu-latest with `cross` (via `taiki-e/setup-cross-toolchain-action` or installing `cross` directly). For darwin-aarch64: `runs-on: macos-latest` with native cargo build (Apple Silicon runners are default macos-latest as of 2024+).
- [ ] 1.4 Each matrix leg: after `cargo build --release --target <triple>`, run `strip` on the resulting binary (path: `autocoder/target/<triple>/release/autocoder`), copy to a deterministically-named artifact path `autocoder-${{ github.ref_name }}-${{ matrix.target }}`, compute SHA-256 via `shasum -a 256` (or `sha256sum` on Linux), write to `<binary-name>.sha256` in the format `<hex-digest>  <binary-name>` (single space matches what `sha256sum -c` expects). Upload both files as actions artifacts via `actions/upload-artifact@v4`.
- [ ] 1.5 `publish` job: `runs-on: ubuntu-latest`, `needs: [test, build]`. Steps: download all artifacts via `actions/download-artifact@v4`. Create GitHub Release using `softprops/action-gh-release@v2` with `tag_name: ${{ github.ref_name }}`, `files: <glob to all binaries + .sha256>`, `prerelease: ${{ contains(github.ref_name, '-') }}` (SemVer dash-suffix → pre-release). Set `generate_release_notes: true` so the release body has the auto-generated changelog as a starting point.
- [ ] 1.6 `permissions:` block at the top of the workflow: `contents: write` for the publish job (needed by `action-gh-release`). Default for the rest is `read`.

## 2. Release procedure doc

- [ ] 2.1 Create `RELEASING.md` at the repo root. Short doc (≤ 50 lines). Sections:
  - **Pre-flight**: tests must be green on `main`; `Cargo.toml` version bumped to the new vX.Y.Z; CHANGELOG.md updated if one exists.
  - **Cut the release**: `git tag vX.Y.Z`, `git push --tags`. Workflow auto-publishes.
  - **Pre-release naming**: `vX.Y.Z-rc1`, `vX.Y.Z-dev`, `vX.Y.Z-beta.2`, etc. The dash auto-flags as pre-release.
  - **After publish**: edit the release notes on GitHub if the auto-generated changelog needs annotation.
  - **Verification**: cite the install-script's checksum-verification step as the consumer of the `.sha256` files.

## 3. Documentation update (folded under existing project-documentation rule)

- [ ] 3.1 Update README "Deployment" section (currently § "Deployment" near the bottom). Add a short subsection at the top of Deployment: **"Recommended: install from a binary release"** with a one-line summary ("see [install-script-and-wizard] for the curl-and-run install path; this section covers source builds and manual installs for operators who need them"). The install-script-and-wizard companion spec adds the actual "Recommended" content; this spec's README change just frames the existing source-build content as the manual/advanced path.
- [ ] 3.2 Add a short note in README explaining how releases are versioned and how to find them on the GitHub Releases page.

## 4. Spec delta

- [ ] 4.1 Author the ADDED requirement "Tagged releases produce architecture-specific binaries on GitHub Releases" under `project-documentation` per the proposal.

## 5. Verification

- [ ] 5.1 `openspec validate release-pipeline-github-actions --strict` passes.
- [ ] 5.2 Lint the workflow file with `actionlint` (if available): zero errors. If `actionlint` isn't on the maintainer's PATH, install via `brew install actionlint` (mac) or download the prebuilt binary from its repo.
- [ ] 5.3 Smoke test BEFORE merging: push a `v0.0.0-spec-smoke-test` tag (deletable) to verify the workflow runs end-to-end without errors and produces three binaries + three checksum files marked as pre-release. Delete the tag and the test release afterward. Document this smoke-test result in the change's PR description.
