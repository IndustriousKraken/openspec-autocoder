## ADDED Requirements

### Requirement: Tagged releases produce architecture-specific binaries on GitHub Releases
The repository SHALL contain a GitHub Actions workflow at `.github/workflows/release.yml` triggered by tag pushes matching `v*`. The workflow SHALL gate on a green `cargo test --release` run, then build release binaries for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and `aarch64-apple-darwin`, attaching each binary and its SHA-256 checksum file to a GitHub Release named after the tag.

The asset naming convention is contractual — a downstream install script relies on it. Asset names SHALL be `autocoder-<tag>-<rust-target-triple>` for the binary and `<binary-name>.sha256` for the checksum file. The checksum file SHALL be in the `<hex-digest>  <binary-name>` format produced by `sha256sum` so `sha256sum -c` can verify it without reformatting.

#### Scenario: Production tag triggers a full release
- **WHEN** a maintainer pushes a tag matching `^v\d+\.\d+\.\d+$`
  (e.g. `v1.2.3`)
- **THEN** the workflow runs the test gate, builds three target
  binaries, attaches them and their `.sha256` files to a new
  GitHub Release at that tag
- **AND** the release is published as a normal (non-pre-release)
  release

#### Scenario: Pre-release tag triggers a pre-release
- **WHEN** a maintainer pushes a tag containing a dash suffix
  (e.g. `v1.2.3-rc1`, `v1.2.3-dev`, `v0.5.0-beta.2`)
- **THEN** the same build pipeline runs and uploads assets
- **AND** the resulting GitHub Release is marked as `prerelease:
  true` so the GitHub UI badges it visibly and the install
  script's "production releases" filter skips it by default

#### Scenario: Test gate failure halts the release
- **WHEN** the `cargo test --release` step on the test job fails
- **THEN** the matrix-build and publish jobs do NOT run
  (`needs: test` dependency chain)
- **AND** no GitHub Release is created for the tag; the failing
  workflow run is visible in the Actions tab so the maintainer
  can investigate and either delete the tag (recommended) or
  push a fix tag (e.g. `v1.2.4`) to retry

#### Scenario: Asset naming is stable
- **WHEN** the workflow runs for tag `v1.2.3`
- **THEN** the release exposes exactly six assets:
  `autocoder-v1.2.3-x86_64-unknown-linux-gnu`,
  `autocoder-v1.2.3-x86_64-unknown-linux-gnu.sha256`,
  `autocoder-v1.2.3-aarch64-unknown-linux-gnu`,
  `autocoder-v1.2.3-aarch64-unknown-linux-gnu.sha256`,
  `autocoder-v1.2.3-aarch64-apple-darwin`,
  `autocoder-v1.2.3-aarch64-apple-darwin.sha256`
- **AND** the install script's download URL pattern
  `https://github.com/<owner>/<repo>/releases/download/<tag>/<asset-name>`
  resolves directly to these files

#### Scenario: Adding a new target triple
- **WHEN** an implementing agent extends the build matrix with a
  new target (e.g. `x86_64-apple-darwin` for Intel Mac support)
- **THEN** this requirement's enumeration of target triples is
  updated in the same commit, AND the install script's
  architecture-detection logic is updated to match, so all three
  artifacts (workflow, this spec, install script) stay aligned
