## ADDED Requirements

### Requirement: DEPLOYMENT.md documents switching from source-build to binary upgrades
`docs/DEPLOYMENT.md` SHALL include a section titled `Switching from source-build to binary updates` that targets operators whose existing deployment was built from source — typically a hand-written systemd unit pointing at a config under the operator's home directory — and who want to switch to the released-binary upgrade path. The section SHALL document two paths: the `install.sh --config-dir <existing-config-dir>` invocation that leverages the systemd-probe detection AND a manual `curl + sha256sum + install -m 755` sequence for operators who prefer to skip the bash wrapper entirely.

#### Scenario: Section exists and names both upgrade paths
- **WHEN** an operator reads `docs/DEPLOYMENT.md`
- **THEN** a section titled `Switching from source-build to binary updates` appears between `Recommended: install from a binary release` and `## 1. Install the binary`
- **AND** the section names the `install.sh --config-dir <path>` invocation
- **AND** the section names the manual-download alternative using the contractual asset name pattern `autocoder-<tag>-<triple>`
- **AND** the section names the post-update step `sudo systemctl restart autocoder`

#### Scenario: Section explains why a bare install.sh re-run is unsafe pre-systemd-probe
- **WHEN** the operator reads the section
- **THEN** the text explains that an unqualified `install.sh` re-run on a source-built deployment would have overwritten the systemd unit AND lost any custom `Environment="PATH=..."` entries the operator added (a common case is the openspec CLI living under `~/.nvm/versions/node/<v>/bin/`)
- **AND** the text explains that the install wizard's systemd probe now prevents that outcome — but only when the operator passes `--config-dir <existing-config-dir>` OR the existing unit can be detected via `systemctl show autocoder.service`

#### Scenario: Cross-link forward to unattended-update story is correct when it lands
- **WHEN** the section completes its source-to-binary content
- **THEN** the closing paragraph cross-links to `Unattended updates via cron` (the anchor lands when `update.sh` ships under a later stacked change)
- **AND** until that change merges, the cross-link is a dead anchor within the same file — acceptable for a stacked-dependency change and resolves automatically when the dependent change merges
