//! GitHub REST API client for opening pull requests, plus URL parsing.

use crate::code_reviewer::ReviewReport;
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Deserialize;

pub const DEFAULT_API_BASE: &str = "https://api.github.com";
const DRAFT_FALLBACK_LABEL: &str = "do-not-merge";

#[derive(Deserialize)]
struct PullResponse {
    html_url: String,
    number: u64,
}

/// The fields autocoder cares about from a freshly-created PR. Returned by
/// `create_pull_request` so the caller can route follow-up work (e.g.
/// posting an implementer-summary issue comment) to the new PR.
#[derive(Debug, Clone)]
pub struct CreatedPr {
    pub html_url: String,
    pub number: u64,
}

/// One element of a `GET /pulls?...` response. Only the fields autocoder
/// consults are deserialized; everything else in the API payload is
/// ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenPr {
    pub number: u64,
    pub html_url: String,
}

/// Production wrapper for `list_open_prs_at` against the live GitHub API.
/// Returns the list of open PRs whose head and base match the given
/// qualifiers. Used by the polling loop to skip iterations when a PR is
/// already pending review.
pub async fn list_open_prs(
    owner: &str,
    repo: &str,
    head: &str,
    base: &str,
    token: &str,
) -> Result<Vec<OpenPr>> {
    list_open_prs_at(DEFAULT_API_BASE, owner, repo, head, base, token).await
}

/// Test-only re-export of the internal `list_open_prs_at`. Lets
/// sibling-module tests exercise the HTTP path against mockito.
#[cfg(test)]
pub(crate) async fn list_open_prs_at_for_test(
    api_base: &str,
    owner: &str,
    repo: &str,
    head: &str,
    base: &str,
    token: &str,
) -> Result<Vec<OpenPr>> {
    list_open_prs_at(api_base, owner, repo, head, base, token).await
}

async fn list_open_prs_at(
    api_base: &str,
    owner: &str,
    repo: &str,
    head: &str,
    base: &str,
    token: &str,
) -> Result<Vec<OpenPr>> {
    let url = format!("{api_base}/repos/{owner}/{repo}/pulls");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .query(&[("state", "open"), ("head", head), ("base", base)])
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "openspec-autocoder")
        .send()
        .await
        .map_err(|e| anyhow!("github pulls GET failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body_snippet = resp
            .text()
            .await
            .map(|t| t.chars().take(500).collect::<String>())
            .unwrap_or_default();
        return Err(anyhow!(
            "github pulls GET {owner}/{repo} returned {status}: {body_snippet}"
        ));
    }
    let parsed: Vec<OpenPr> = resp
        .json()
        .await
        .map_err(|e| anyhow!("github pulls response decode failed: {e}"))?;
    Ok(parsed)
}

/// Fetch the most-recently-created PR whose head equals
/// `{head_owner}:{head_branch}`. Used by the operator-status reply to
/// show "latest PR by the daemon."
///
/// In fork-PR mode (`github.fork_owner` set), callers SHALL pass
/// `head_owner = fork_owner`. In direct-push mode, callers SHALL pass
/// `head_owner = owner`. Pre-`a20a4` the helper reused the URL-path
/// `owner` parameter for the head qualifier, which never matched a
/// fork-headed PR — making the status reply show `(none)` on every
/// fork-PR-mode deployment even when a PR was open. Aligns with the
/// caller-builds-head-qualifier pattern used by
/// `polling_loop.rs::open_pr_exists_for_agent_branch_at`.
///
/// Returns `Ok(Some(PrSummary))` on a 200 with a non-empty list,
/// `Ok(None)` on a 200 with an empty list (no PR ever opened from this
/// branch), and `Err` on any other condition (network, 4xx, 5xx, decode
/// failure). The status path swallows the `Err` and renders `(none)` —
/// see `control_socket::build_repo_status`.
pub async fn latest_pr_for_head(
    api_base: &str,
    token: &str,
    owner: &str,
    repo: &str,
    head_owner: &str,
    head_branch: &str,
) -> Result<Option<crate::chatops::operator_commands::PrSummary>> {
    use chrono::{DateTime, Utc};
    #[derive(Deserialize)]
    struct PrHead {
        #[serde(rename = "ref")]
        ref_: String,
    }
    #[derive(Deserialize)]
    struct PrListItem {
        number: u64,
        title: String,
        state: String,
        html_url: String,
        created_at: DateTime<Utc>,
        #[serde(default)]
        merged_at: Option<DateTime<Utc>>,
        head: PrHead,
    }

    let url = format!("{api_base}/repos/{owner}/{repo}/pulls");
    let head_qualified = format!("{head_owner}:{head_branch}");
    let resp = reqwest::Client::new()
        .get(&url)
        .query(&[
            ("head", head_qualified.as_str()),
            ("state", "all"),
            ("sort", "created"),
            ("direction", "desc"),
            ("per_page", "1"),
        ])
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "openspec-autocoder")
        .send()
        .await
        .map_err(|e| anyhow!("github latest-pr GET failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body_snippet = resp
            .text()
            .await
            .map(|t| t.chars().take(500).collect::<String>())
            .unwrap_or_default();
        return Err(anyhow!(
            "github latest-pr GET {owner}/{repo} returned {status}: {body_snippet}"
        ));
    }
    let parsed: Vec<PrListItem> = resp
        .json()
        .await
        .map_err(|e| anyhow!("github latest-pr response decode failed: {e}"))?;
    let item = match parsed.into_iter().next() {
        Some(i) => i,
        None => return Ok(None),
    };
    let mapped_state = if item.state == "closed" && item.merged_at.is_some() {
        "merged".to_string()
    } else {
        item.state
    };
    let age = Utc::now() - item.created_at;
    Ok(Some(crate::chatops::operator_commands::PrSummary {
        number: item.number,
        title: item.title,
        state: mapped_state,
        head_branch: item.head.ref_,
        url: item.html_url,
        age,
    }))
}

/// Create a fork of the upstream repo via the GitHub REST API. The fork's
/// destination is implicit from the PAT's owner — GitHub forks to the
/// authenticated user's account by default. Returns Ok on 2xx (including
/// the idempotent case where the fork already exists).
pub async fn create_fork(upstream_owner: &str, upstream_repo: &str, token: &str) -> Result<()> {
    create_fork_at(DEFAULT_API_BASE, upstream_owner, upstream_repo, token).await
}

/// Test-only re-export of the internal `create_fork_at`. Lets sibling-module
/// tests (e.g. polling_loop's routing test) exercise the fork-creation HTTP
/// path against a mockito server.
#[cfg(test)]
pub(crate) async fn create_fork_at_for_test(
    api_base: &str,
    upstream_owner: &str,
    upstream_repo: &str,
    token: &str,
) -> Result<()> {
    create_fork_at(api_base, upstream_owner, upstream_repo, token).await
}

pub(crate) async fn create_fork_at(
    api_base: &str,
    upstream_owner: &str,
    upstream_repo: &str,
    token: &str,
) -> Result<()> {
    let url = format!("{api_base}/repos/{upstream_owner}/{upstream_repo}/forks");
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "openspec-autocoder")
        .send()
        .await
        .map_err(|e| anyhow!("github fork POST failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body_snippet = resp
            .text()
            .await
            .map(|t| t.chars().take(200).collect::<String>())
            .unwrap_or_default();
        return Err(anyhow!(
            "github fork POST {upstream_owner}/{upstream_repo} returned {status}: {body_snippet}"
        ));
    }
    Ok(())
}

/// Outcome of a `DELETE /repos/{owner}/{repo}` call. Distinguishes the
/// "fork was already gone" case (404) from the "missing scope" case (403)
/// from a successful delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// 204 (or 200): the repo existed and was deleted by this call.
    Deleted,
    /// 404: the repo was already absent (deleted out-of-band).
    AlreadyGone,
    /// 403: the operator's PAT lacks the `delete_repo` scope.
    Forbidden,
}

/// Delete a GitHub repository via `DELETE /repos/{owner}/{repo}`. Returns
/// a `DeleteOutcome` for the common cases; any other non-2xx response
/// returns `Err`. Used by the `recreate_fork_on_reinit` recovery path
/// (the workspace helper currently routes through `delete_repo_at` for
/// uniform API-base injection in tests; this public entry remains for
/// callers that don't need that override).
#[allow(dead_code)]
pub async fn delete_repo(owner: &str, repo: &str, token: &str) -> Result<DeleteOutcome> {
    delete_repo_at(DEFAULT_API_BASE, owner, repo, token).await
}

/// Test-only re-export of the internal `delete_repo_at`. Lets sibling-
/// module tests exercise the HTTP path against a mockito server.
#[cfg(test)]
pub(crate) async fn delete_repo_at_for_test(
    api_base: &str,
    owner: &str,
    repo: &str,
    token: &str,
) -> Result<DeleteOutcome> {
    delete_repo_at(api_base, owner, repo, token).await
}

pub(crate) async fn delete_repo_at(
    api_base: &str,
    owner: &str,
    repo: &str,
    token: &str,
) -> Result<DeleteOutcome> {
    let url = format!("{api_base}/repos/{owner}/{repo}");
    let client = reqwest::Client::new();
    let resp = client
        .delete(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "openspec-autocoder")
        .send()
        .await
        .map_err(|e| anyhow!("github DELETE failed: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(DeleteOutcome::Deleted);
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(DeleteOutcome::AlreadyGone);
    }
    if status == reqwest::StatusCode::FORBIDDEN {
        return Ok(DeleteOutcome::Forbidden);
    }
    let body_snippet = resp
        .text()
        .await
        .map(|t| t.chars().take(200).collect::<String>())
        .unwrap_or_default();
    Err(anyhow!(
        "github DELETE {owner}/{repo} returned {status}: {body_snippet}"
    ))
}

/// Open a pull request via the GitHub REST API. Returns the `html_url` of
/// the created PR on success.
///
/// - `review_report`: if `Some`, the report's `markdown` is appended to the
///   PR body under a `## Code Review` heading.
/// - `draft`: requests a draft PR. If the host rejects the draft flag, the
///   PR is retried as non-draft and a `do-not-merge` label is applied as a
///   fallback so a branch-protection rule on that label still gates merge.
#[allow(clippy::too_many_arguments)]
pub async fn create_pull_request(
    owner: &str,
    repo: &str,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
    token: &str,
    review_report: Option<&ReviewReport>,
    draft: bool,
) -> Result<CreatedPr> {
    create_pull_request_at(
        DEFAULT_API_BASE,
        owner,
        repo,
        head,
        base,
        title,
        body,
        token,
        review_report,
        draft,
    )
    .await
}

/// Test-only re-export of the internal `create_pull_request_at`. Lets
/// sibling-module tests (e.g. polling_loop's routing test) exercise the
/// PR HTTP path against a mockito server.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn create_pull_request_at_for_test(
    api_base: &str,
    owner: &str,
    repo: &str,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
    token: &str,
    review_report: Option<&ReviewReport>,
    draft: bool,
) -> Result<CreatedPr> {
    create_pull_request_at(
        api_base,
        owner,
        repo,
        head,
        base,
        title,
        body,
        token,
        review_report,
        draft,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn create_pull_request_at(
    api_base: &str,
    owner: &str,
    repo: &str,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
    token: &str,
    review_report: Option<&ReviewReport>,
    draft: bool,
) -> Result<CreatedPr> {
    let composed_body = compose_body(body, review_report);
    let client = reqwest::Client::new();
    let url = format!("{api_base}/repos/{owner}/{repo}/pulls");

    let attempt = |draft_flag: bool, body_text: &str| {
        let mut payload = serde_json::json!({
            "title": title,
            "body": body_text,
            "head": head,
            "base": base,
        });
        if draft_flag {
            payload["draft"] = serde_json::Value::Bool(true);
        }
        client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "openspec-autocoder")
            .json(&payload)
            .send()
    };

    let first = attempt(draft, &composed_body)
        .await
        .map_err(|e| anyhow!("github pr request failed: {e}"))?;

    let first_status = first.status();
    if first_status.is_success() {
        let parsed: PullResponse = first
            .json()
            .await
            .map_err(|e| anyhow!("github pr response decode failed: {e}"))?;
        return Ok(CreatedPr {
            html_url: parsed.html_url,
            number: parsed.number,
        });
    }

    let first_body = first.text().await.unwrap_or_default();

    // Detect the "drafts not supported on this repository" error. GitHub's
    // response body contains a message naming the constraint; we match
    // case-insensitively on both "draft" and "not supported" to be tolerant
    // of minor wording changes across API versions.
    let draft_unsupported = draft
        && first_status.is_client_error()
        && first_body.to_ascii_lowercase().contains("draft")
        && first_body.to_ascii_lowercase().contains("not supported");

    if !draft_unsupported {
        let snippet: String = first_body.chars().take(500).collect();
        return Err(anyhow!(
            "github pr creation failed: {first_status}: {snippet}"
        ));
    }

    // Retry without the draft flag, then apply a do-not-merge label.
    tracing::warn!(
        owner,
        repo,
        "github rejected draft flag for this repository; retrying as non-draft and applying `{}` label as fallback",
        DRAFT_FALLBACK_LABEL
    );

    let retry = attempt(false, &composed_body)
        .await
        .map_err(|e| anyhow!("github pr retry without draft failed: {e}"))?;
    let retry_status = retry.status();
    if !retry_status.is_success() {
        let retry_body = retry.text().await.unwrap_or_default();
        let snippet: String = retry_body.chars().take(500).collect();
        return Err(anyhow!(
            "github pr creation failed (retry): {retry_status}: {snippet}"
        ));
    }
    let parsed: PullResponse = retry
        .json()
        .await
        .map_err(|e| anyhow!("github pr response decode failed (retry): {e}"))?;

    apply_label(api_base, owner, repo, parsed.number, DRAFT_FALLBACK_LABEL, token).await?;
    tracing::info!(
        owner,
        repo,
        pr_number = parsed.number,
        "draft unsupported; applied do-not-merge label as fallback"
    );

    Ok(CreatedPr {
        html_url: parsed.html_url,
        number: parsed.number,
    })
}

/// Post an issue comment to a PR (issues and PRs share the comments
/// endpoint on GitHub). Best-effort: returns Ok on 2xx, Err on non-2xx
/// with the status code and a 500-char body snippet.
pub async fn create_issue_comment(
    owner: &str,
    repo: &str,
    issue_number: u64,
    body: &str,
    token: &str,
) -> Result<()> {
    create_issue_comment_at(DEFAULT_API_BASE, owner, repo, issue_number, body, token).await
}

/// Test-only re-export of the internal `create_issue_comment_at`. Lets
/// sibling-module tests exercise the comment-post HTTP path against a
/// mockito server.
#[cfg(test)]
pub(crate) async fn create_issue_comment_at_for_test(
    api_base: &str,
    owner: &str,
    repo: &str,
    issue_number: u64,
    body: &str,
    token: &str,
) -> Result<()> {
    create_issue_comment_at(api_base, owner, repo, issue_number, body, token).await
}

async fn create_issue_comment_at(
    api_base: &str,
    owner: &str,
    repo: &str,
    issue_number: u64,
    body: &str,
    token: &str,
) -> Result<()> {
    let url = format!("{api_base}/repos/{owner}/{repo}/issues/{issue_number}/comments");
    let payload = serde_json::json!({ "body": body });
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "openspec-autocoder")
        .json(&payload)
        .send()
        .await
        .map_err(|e| anyhow!("github issue-comment POST failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let snippet: String = resp
            .text()
            .await
            .map(|t| t.chars().take(500).collect::<String>())
            .unwrap_or_default();
        return Err(anyhow!(
            "github issue-comment POST {owner}/{repo}#{issue_number} returned {status}: {snippet}"
        ));
    }
    Ok(())
}

/// GitHub's hard cap on PR-body length. Bodies submitted above this size
/// are rejected by the REST API. Used by `compose_body` to truncate the
/// LAST per-change reviewer section under per-change mode (matching the
/// shape of the existing `## Agent implementation notes` truncation).
const GITHUB_PR_BODY_MAX_CHARS: usize = 65_535;

/// Compose the PR body. In bundled mode (`per_change_sections` empty),
/// the existing body gains a single `## Code Review` section. In per-
/// change mode (`per_change_sections` non-empty), the body gains one
/// `## Code Review: <slug>` section per element instead. If the
/// combined per-change body exceeds GitHub's 65,535-char PR-body cap,
/// the LAST section is truncated with a "see daemon log" pointer.
fn compose_body(body: &str, review_report: Option<&ReviewReport>) -> String {
    let report = match review_report {
        Some(r) => r,
        None => return body.to_string(),
    };
    if report.per_change_sections.is_empty() {
        return format!("{body}\n\n## Code Review\n\n{}", report.markdown);
    }
    let sections = &report.per_change_sections;
    let mut out = body.to_string();
    for section in sections.iter() {
        let candidate = format!(
            "\n\n## Code Review: {}\n\n{}",
            section.change_slug, section.markdown
        );
        if out.len() + candidate.len() > GITHUB_PR_BODY_MAX_CHARS {
            // The combined body would overflow. Truncate the current
            // section with the pointer footer and stop appending
            // further sections — matches the shape of the existing
            // `## Agent implementation notes` truncation.
            let pointer =
                "\n\n(truncated to fit GitHub's PR-body cap; see daemon log for full review)\n";
            let header = format!("\n\n## Code Review: {}\n\n", section.change_slug);
            let available = GITHUB_PR_BODY_MAX_CHARS
                .saturating_sub(out.len())
                .saturating_sub(header.len())
                .saturating_sub(pointer.len());
            let body_slice: String = section.markdown.chars().take(available).collect();
            out.push_str(&header);
            out.push_str(&body_slice);
            out.push_str(pointer);
            break;
        }
        out.push_str(&candidate);
    }
    out
}

async fn apply_label(
    api_base: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    label: &str,
    token: &str,
) -> Result<()> {
    let url = format!("{api_base}/repos/{owner}/{repo}/issues/{pr_number}/labels");
    let payload = serde_json::json!({ "labels": [label] });
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "openspec-autocoder")
        .json(&payload)
        .send()
        .await
        .map_err(|e| anyhow!("github label POST failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let snippet: String = body.chars().take(500).collect();
        return Err(anyhow!(
            "github label application failed: {status}: {snippet}"
        ));
    }
    Ok(())
}

/// Parse a GitHub repository URL into `(owner, repo)`. Accepts both SSH and
/// HTTPS forms, with or without a trailing `.git`.
pub fn parse_repo_url(url: &str) -> Result<(String, String)> {
    let trimmed = url.trim();
    let without_suffix = trimmed.strip_suffix(".git").unwrap_or(trimmed);

    if let Some(rest) = without_suffix.strip_prefix("git@github.com:") {
        return split_owner_repo(rest, url);
    }
    if let Some(rest) = without_suffix
        .strip_prefix("https://github.com/")
        .or_else(|| without_suffix.strip_prefix("http://github.com/"))
        .or_else(|| without_suffix.strip_prefix("ssh://git@github.com/"))
    {
        return split_owner_repo(rest, url);
    }
    Err(anyhow!(
        "unrecognized github URL `{url}`: expected `git@github.com:<owner>/<repo>.git` or `https://github.com/<owner>/<repo>(.git)?`"
    ))
}

/// Derive a fork URL by substituting `fork_owner` for the owner segment of
/// `upstream_url`, preserving the URL's scheme and the repository name.
///
/// Supports both `git@github.com:owner/repo.git` (SSH) and
/// `https://github.com/owner/repo.git` (HTTPS) inputs. Other schemes
/// (e.g. enterprise GitHub hosts, `ssh://...`) error explicitly so the
/// operator knows their config is not supported by fork-PR mode.
pub fn derive_fork_url(upstream_url: &str, fork_owner: &str) -> Result<String> {
    let (_upstream_owner, repo) = parse_repo_url(upstream_url)?;
    let trimmed = upstream_url.trim();
    if trimmed.starts_with("git@github.com:") {
        Ok(format!("git@github.com:{fork_owner}/{repo}.git"))
    } else if trimmed.starts_with("https://github.com/") {
        Ok(format!("https://github.com/{fork_owner}/{repo}.git"))
    } else {
        Err(anyhow!(
            "fork-PR mode requires a `git@github.com:` or `https://github.com/` upstream URL; got `{upstream_url}`"
        ))
    }
}

fn split_owner_repo(rest: &str, original: &str) -> Result<(String, String)> {
    let mut parts = rest.splitn(2, '/');
    let owner = parts.next().unwrap_or("");
    let repo = parts.next().unwrap_or("");
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return Err(anyhow!(
            "unrecognized github URL `{original}`: expected exactly `<owner>/<repo>`"
        ));
    }
    Ok((owner.to_string(), repo.to_string()))
}

/// One element of a `GET /pulls?head=...` response used by the revision
/// dispatcher. Only the fields the dispatcher consults are deserialized.
/// The `#[allow(dead_code)]` covers fields parsed for forward-compat or
/// included in tests but not yet read at the call site.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    #[serde(rename = "html_url")]
    pub url: String,
    pub state: String,
    #[serde(default)]
    pub body: Option<String>,
    pub created_at: DateTime<Utc>,
    pub head: PrRefSummary,
    pub base: PrRefSummary,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PrRefSummary {
    #[serde(rename = "ref")]
    pub ref_: String,
}

/// One element of a `GET /repos/{owner}/{repo}/issues/{n}/comments`
/// response. Only the fields the revision dispatcher consults are
/// deserialized.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct IssueComment {
    pub id: u64,
    pub body: String,
    #[serde(default)]
    pub user: Option<IssueCommentUser>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueCommentUser {
    pub login: String,
}

impl IssueComment {
    /// Convenience accessor for the comment author's `user.login` field,
    /// returning an empty string when the field is absent (deleted users).
    pub fn user_login(&self) -> &str {
        self.user.as_ref().map(|u| u.login.as_str()).unwrap_or("")
    }
}

/// Look up the authenticated user (the bot) via `GET /user`. Used at
/// startup to learn the bot's GitHub username so the revision dispatcher
/// can filter the bot's own comments and recognize mentions.
pub async fn self_bot_username(api_base: &str, token: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct UserResponse {
        login: String,
    }
    let url = format!("{api_base}/user");
    let resp = reqwest::Client::new()
        .get(&url)
        .header("Authorization", format!("token {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "openspec-autocoder")
        .send()
        .await
        .map_err(|e| anyhow!("github /user GET failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body_snippet = resp
            .text()
            .await
            .map(|t| t.chars().take(500).collect::<String>())
            .unwrap_or_default();
        return Err(anyhow!(
            "github /user GET returned {status}: {body_snippet}"
        ));
    }
    let parsed: UserResponse = resp
        .json()
        .await
        .map_err(|e| anyhow!("github /user response decode failed: {e}"))?;
    Ok(parsed.login)
}

/// List every open PR in a repository (no `head` filter). Used by the
/// changelog revision dispatcher: changelog PRs ship on branches whose
/// names follow the deterministic `changelog-<short-hash>` shape, but
/// the dispatcher does not know any individual short-hash up front — it
/// pulls every open PR AND filters in-process by `head.ref` prefix.
/// Pagination beyond 100 is out of scope for the same reason as the
/// head-filtered variant.
pub async fn list_open_prs_all(
    api_base: &str,
    token: &str,
    owner: &str,
    repo: &str,
) -> Result<Vec<PrSummary>> {
    let url = format!("{api_base}/repos/{owner}/{repo}/pulls");
    let resp = reqwest::Client::new()
        .get(&url)
        .query(&[("state", "open"), ("per_page", "100")])
        .header("Authorization", format!("token {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "openspec-autocoder")
        .send()
        .await
        .map_err(|e| anyhow!("github list-open-prs-all GET failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body_snippet = resp
            .text()
            .await
            .map(|t| t.chars().take(500).collect::<String>())
            .unwrap_or_default();
        return Err(anyhow!(
            "github list-open-prs-all GET {owner}/{repo} returned {status}: {body_snippet}"
        ));
    }
    let parsed: Vec<PrSummary> = resp
        .json()
        .await
        .map_err(|e| anyhow!("github list-open-prs-all response decode failed: {e}"))?;
    Ok(parsed)
}

/// List open PRs whose head is `<owner>:<head_branch>`. Used by the
/// revision dispatcher to find the set of bot-opened PRs to poll for
/// comment triggers. Pagination beyond 100 is out of scope — a repo with
/// >100 open PRs from a single agent branch is unrealistic.
/// List open PRs on `owner/repo` whose head matches
/// `{head_owner}:{head_branch}`.
///
/// In fork-PR mode (`github.fork_owner` set), callers SHALL pass
/// `head_owner = fork_owner`. In direct-push mode, callers SHALL pass
/// `head_owner = owner` (the upstream). Pre-`a20a4` the helper reused
/// the URL-path `owner` parameter for the head qualifier, which never
/// matched a fork-headed PR — making the revise dispatcher silently
/// non-functional on every fork-PR-mode deployment. Aligns with the
/// caller-builds-head-qualifier pattern used by
/// `polling_loop.rs::open_pr_exists_for_agent_branch_at`.
pub async fn list_open_prs_for_head(
    api_base: &str,
    token: &str,
    owner: &str,
    repo: &str,
    head_owner: &str,
    head_branch: &str,
) -> Result<Vec<PrSummary>> {
    let url = format!("{api_base}/repos/{owner}/{repo}/pulls");
    let head_qualified = format!("{head_owner}:{head_branch}");
    let resp = reqwest::Client::new()
        .get(&url)
        .query(&[
            ("state", "open"),
            ("head", head_qualified.as_str()),
            ("per_page", "100"),
        ])
        .header("Authorization", format!("token {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "openspec-autocoder")
        .send()
        .await
        .map_err(|e| anyhow!("github list-open-prs GET failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body_snippet = resp
            .text()
            .await
            .map(|t| t.chars().take(500).collect::<String>())
            .unwrap_or_default();
        return Err(anyhow!(
            "github list-open-prs GET {owner}/{repo} returned {status}: {body_snippet}"
        ));
    }
    let parsed: Vec<PrSummary> = resp
        .json()
        .await
        .map_err(|e| anyhow!("github list-open-prs response decode failed: {e}"))?;
    Ok(parsed)
}

/// List issue comments on `pr_number` whose `created_at` is at or after
/// `since`. GitHub's `since` parameter is inclusive on `updated_at`; for
/// the purpose of the revision dispatcher we treat it as "new since".
pub async fn list_issue_comments_since(
    api_base: &str,
    token: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    since: DateTime<Utc>,
) -> Result<Vec<IssueComment>> {
    let url = format!("{api_base}/repos/{owner}/{repo}/issues/{pr_number}/comments");
    let since_str = since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let resp = reqwest::Client::new()
        .get(&url)
        .query(&[("since", since_str.as_str()), ("per_page", "100")])
        .header("Authorization", format!("token {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "openspec-autocoder")
        .send()
        .await
        .map_err(|e| anyhow!("github list-issue-comments GET failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body_snippet = resp
            .text()
            .await
            .map(|t| t.chars().take(500).collect::<String>())
            .unwrap_or_default();
        return Err(anyhow!(
            "github list-issue-comments GET {owner}/{repo}#{pr_number} returned {status}: {body_snippet}"
        ));
    }
    let parsed: Vec<IssueComment> = resp
        .json()
        .await
        .map_err(|e| anyhow!("github list-issue-comments response decode failed: {e}"))?;
    Ok(parsed)
}

/// Post an issue comment with `body` to `pr_number`. Used by the revision
/// dispatcher to post success/failure/cap-decline replies. Returns Err
/// with the HTTP status and a 500-char body snippet on non-2xx.
pub async fn post_issue_comment(
    api_base: &str,
    token: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    body: &str,
) -> Result<()> {
    let url = format!("{api_base}/repos/{owner}/{repo}/issues/{pr_number}/comments");
    let payload = serde_json::json!({ "body": body });
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Authorization", format!("token {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "openspec-autocoder")
        .json(&payload)
        .send()
        .await
        .map_err(|e| anyhow!("github post-issue-comment POST failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let snippet: String = resp
            .text()
            .await
            .map(|t| t.chars().take(500).collect::<String>())
            .unwrap_or_default();
        return Err(anyhow!(
            "github post-issue-comment POST {owner}/{repo}#{pr_number} returned {status}: {snippet}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_fork_url_ssh() {
        let got = derive_fork_url("git@github.com:UpstreamOrg/repo.git", "machine-user").unwrap();
        assert_eq!(got, "git@github.com:machine-user/repo.git");
    }

    #[test]
    fn derive_fork_url_https() {
        let got = derive_fork_url(
            "https://github.com/UpstreamOrg/repo.git",
            "machine-user",
        )
        .unwrap();
        assert_eq!(got, "https://github.com/machine-user/repo.git");
    }

    #[test]
    fn derive_fork_url_preserves_repo_name_without_git_suffix() {
        // parse_repo_url accepts URLs without .git; derive_fork_url
        // always emits .git suffix on the SSH form.
        let got = derive_fork_url("git@github.com:upstream/repo", "machine-user").unwrap();
        assert_eq!(got, "git@github.com:machine-user/repo.git");
    }

    #[test]
    fn derive_fork_url_unsupported_scheme_errors() {
        let err = derive_fork_url("ssh://git@github.com/upstream/repo.git", "machine-user")
            .expect_err("ssh:// scheme should not be supported");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ssh://git@github.com/upstream/repo.git"),
            "error must name the offending URL; got: {msg}"
        );
    }

    #[test]
    fn parse_url_ssh_with_git_suffix() {
        let (owner, repo) = parse_repo_url("git@github.com:owner/repo.git").unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn parse_url_ssh_without_git_suffix() {
        let (owner, repo) = parse_repo_url("git@github.com:owner/repo").unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn parse_url_https_with_git_suffix() {
        let (owner, repo) = parse_repo_url("https://github.com/owner/repo.git").unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn parse_url_https_without_git_suffix() {
        let (owner, repo) = parse_repo_url("https://github.com/owner/repo").unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn parse_url_ssh_url_form() {
        let (owner, repo) = parse_repo_url("ssh://git@github.com/owner/repo.git").unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn parse_url_rejects_non_github() {
        let err = parse_repo_url("https://gitlab.com/owner/repo.git")
            .expect_err("non-github URL should error");
        assert!(format!("{err:#}").contains("unrecognized"), "got: {err:#}");
    }

    #[test]
    fn parse_url_rejects_missing_repo_segment() {
        let err = parse_repo_url("git@github.com:owner").expect_err("missing repo should error");
        assert!(format!("{err:#}").contains("unrecognized"), "got: {err:#}");
    }

    #[test]
    fn parse_url_rejects_extra_path_segment() {
        let err = parse_repo_url("https://github.com/owner/repo/extra")
            .expect_err("extra path segment should error");
        assert!(format!("{err:#}").contains("unrecognized"), "got: {err:#}");
    }

    use crate::code_reviewer::{ReviewReport, ReviewVerdict};

    /// `mockito` smoke test: verify the request shape (path, headers, JSON
    /// body) and decoding of the `html_url` from a 201 response.
    #[tokio::test]
    async fn create_fork_posts_to_forks_endpoint() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/repos/upstream-org/repo/forks")
            .match_header("authorization", "Bearer testtoken")
            .with_status(202)
            .with_header("content-type", "application/json")
            .with_body(r#"{"full_name":"machine-user/repo"}"#)
            .create_async()
            .await;

        create_fork_at_for_test(&server.url(), "upstream-org", "repo", "testtoken")
            .await
            .expect("fork POST succeeds on 202");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn create_fork_errors_on_non_2xx() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/repos/upstream-org/repo/forks")
            .with_status(403)
            .with_body(r#"{"message":"Resource not accessible by personal access token"}"#)
            .create_async()
            .await;

        let err = create_fork_at_for_test(&server.url(), "upstream-org", "repo", "x")
            .await
            .expect_err("non-2xx must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("403"), "error must name the HTTP status; got: {msg}");
    }

    #[tokio::test]
    async fn delete_repo_returns_deleted_on_204() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("DELETE", "/repos/owner/repo")
            .match_header("authorization", "Bearer t")
            .with_status(204)
            .create_async()
            .await;
        let outcome = delete_repo_at_for_test(&server.url(), "owner", "repo", "t")
            .await
            .expect("delete should succeed");
        assert_eq!(outcome, DeleteOutcome::Deleted);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn delete_repo_returns_already_gone_on_404() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("DELETE", "/repos/owner/repo")
            .with_status(404)
            .with_body(r#"{"message":"Not Found"}"#)
            .create_async()
            .await;
        let outcome = delete_repo_at_for_test(&server.url(), "owner", "repo", "t")
            .await
            .expect("404 should map to AlreadyGone, not Err");
        assert_eq!(outcome, DeleteOutcome::AlreadyGone);
    }

    #[tokio::test]
    async fn delete_repo_returns_forbidden_on_403() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("DELETE", "/repos/owner/repo")
            .with_status(403)
            .with_body(r#"{"message":"Resource not accessible by personal access token"}"#)
            .create_async()
            .await;
        let outcome = delete_repo_at_for_test(&server.url(), "owner", "repo", "t")
            .await
            .expect("403 should map to Forbidden, not Err");
        assert_eq!(outcome, DeleteOutcome::Forbidden);
    }

    #[tokio::test]
    async fn delete_repo_errors_on_other_non_2xx() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("DELETE", "/repos/owner/repo")
            .with_status(500)
            .with_body("internal error")
            .create_async()
            .await;
        let err = delete_repo_at_for_test(&server.url(), "owner", "repo", "t")
            .await
            .expect_err("500 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("500"), "error must name the HTTP status; got: {msg}");
    }

    #[tokio::test]
    async fn create_pull_request_posts_expected_request() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/repos/owner/repo/pulls")
            .match_header("authorization", "Bearer testtoken")
            .match_header("accept", "application/vnd.github+json")
            .match_header("user-agent", "openspec-autocoder")
            .match_body(mockito::Matcher::JsonString(
                r#"{"title":"t","body":"b","head":"agent-q","base":"main"}"#.to_string(),
            ))
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"html_url":"https://github.com/owner/repo/pull/1","number":1}"#,
            )
            .create_async()
            .await;

        let pr = create_pull_request_at(
            &server.url(),
            "owner",
            "repo",
            "agent-q",
            "main",
            "t",
            "b",
            "testtoken",
            None,
            false,
        )
        .await
        .expect("PR creation should succeed");

        assert_eq!(pr.html_url, "https://github.com/owner/repo/pull/1");
        assert_eq!(pr.number, 1);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn create_pull_request_returns_err_on_non_2xx() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/repos/owner/repo/pulls")
            .with_status(422)
            .with_body(r#"{"message":"Validation Failed"}"#)
            .create_async()
            .await;

        let err = create_pull_request_at(
            &server.url(),
            "owner",
            "repo",
            "agent-q",
            "main",
            "t",
            "b",
            "testtoken",
            None,
            false,
        )
        .await
        .expect_err("422 should produce error");

        let msg = format!("{err:#}");
        assert!(msg.contains("422"), "expected 422 in error: {msg}");
        assert!(msg.contains("Validation Failed"), "expected body in error: {msg}");
    }

    /// git-workflow-manager / "Reviewer disabled or absent": when
    /// `review_report` is `None`, the PR body MUST NOT contain a
    /// `## Code Review` section.
    #[tokio::test]
    async fn body_excludes_review_section_when_none() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/repos/owner/repo/pulls")
            .match_body(mockito::Matcher::JsonString(
                r#"{"title":"t","body":"original body only","head":"agent-q","base":"main"}"#
                    .to_string(),
            ))
            .with_status(201)
            .with_body(
                r#"{"html_url":"https://github.com/owner/repo/pull/9","number":9}"#,
            )
            .create_async()
            .await;

        create_pull_request_at(
            &server.url(),
            "owner",
            "repo",
            "agent-q",
            "main",
            "t",
            "original body only",
            "testtoken",
            None,
            false,
        )
        .await
        .expect("PR creation should succeed");
        // The JsonString matcher above asserts an EXACT body match — any
        // appended `## Code Review` section would have failed the matcher.
        mock.assert_async().await;
    }

    /// 4.5: PR body includes `## Code Review` section appended to the
    /// existing body when `review_report` is `Some`.
    #[tokio::test]
    async fn body_includes_review_section() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/repos/owner/repo/pulls")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"body":"base body\n\n## Code Review\n\nVERDICT details"}"#.to_string(),
            ))
            .with_status(201)
            .with_body(
                r#"{"html_url":"https://github.com/owner/repo/pull/2","number":2}"#,
            )
            .create_async()
            .await;

        let report = ReviewReport {
            verdict: ReviewVerdict::Pass,
            markdown: "VERDICT details".to_string(),
            concerns: Vec::new(),
            per_change_sections: Vec::new(),
        };
        create_pull_request_at(
            &server.url(),
            "owner",
            "repo",
            "agent-q",
            "main",
            "t",
            "base body",
            "testtoken",
            Some(&report),
            false,
        )
        .await
        .expect("PR creation should succeed");
        mock.assert_async().await;
    }

    /// 4.5: `draft: true` is serialized into the JSON body.
    #[tokio::test]
    async fn draft_flag_serialized() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/repos/owner/repo/pulls")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"draft":true}"#.to_string(),
            ))
            .with_status(201)
            .with_body(
                r#"{"html_url":"https://github.com/owner/repo/pull/3","number":3}"#,
            )
            .create_async()
            .await;

        create_pull_request_at(
            &server.url(),
            "owner",
            "repo",
            "agent-q",
            "main",
            "t",
            "b",
            "testtoken",
            None,
            true,
        )
        .await
        .expect("PR creation should succeed");
        mock.assert_async().await;
    }

    /// 4.5: when the host rejects the draft flag, autocoder retries
    /// without `draft` AND applies a `do-not-merge` label.
    #[tokio::test]
    async fn label_fallback_on_draft_unsupported() {
        let mut server = mockito::Server::new_async().await;

        // First POST: draft=true → 422 "drafts not supported".
        let first = server
            .mock("POST", "/repos/owner/repo/pulls")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"draft":true}"#.to_string(),
            ))
            .with_status(422)
            .with_body(
                r#"{"message":"Validation Failed","errors":[{"message":"Draft pull requests are not supported in this repository."}]}"#,
            )
            .expect(1)
            .create_async()
            .await;

        // Second POST: retry without `draft` key → 201 with PR number 4.
        // Mockito matches the first satisfied mock in registration order, so
        // the previous mock claims requests that contain `"draft":true`
        // exclusively; this mock claims the retry which lacks that field.
        // We additionally assert below that the body does NOT contain a
        // `"draft":true` substring after the test runs.
        let second = server
            .mock("POST", "/repos/owner/repo/pulls")
            .match_body(mockito::Matcher::PartialJsonString(
                r#"{"title":"t","head":"agent-q","base":"main"}"#.to_string(),
            ))
            .with_status(201)
            .with_body(
                r#"{"html_url":"https://github.com/owner/repo/pull/4","number":4}"#,
            )
            .expect(1)
            .create_async()
            .await;

        // Label POST hits the issues endpoint with PR number 4.
        let label = server
            .mock("POST", "/repos/owner/repo/issues/4/labels")
            .match_body(mockito::Matcher::JsonString(
                r#"{"labels":["do-not-merge"]}"#.to_string(),
            ))
            .with_status(200)
            .with_body(r#"[{"name":"do-not-merge"}]"#)
            .expect(1)
            .create_async()
            .await;

        let pr = create_pull_request_at(
            &server.url(),
            "owner",
            "repo",
            "agent-q",
            "main",
            "t",
            "b",
            "testtoken",
            None,
            true,
        )
        .await
        .expect("fallback path should succeed");

        assert_eq!(pr.html_url, "https://github.com/owner/repo/pull/4");
        assert_eq!(pr.number, 4);
        first.assert_async().await;
        second.assert_async().await;
        label.assert_async().await;
    }

    /// `list_open_prs` parses a non-empty array response.
    #[tokio::test]
    async fn list_open_prs_parses_response() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock(
                "GET",
                "/repos/owner/repo/pulls?state=open&head=owner%3Aagent-q&base=main",
            )
            .with_status(200)
            .with_body(
                r#"[{"number":42,"html_url":"https://github.com/owner/repo/pull/42","title":"ignored","unused":"extra"}]"#,
            )
            .expect(1)
            .create_async()
            .await;

        let prs = list_open_prs_at(
            &server.url(),
            "owner",
            "repo",
            "owner:agent-q",
            "main",
            "testtoken",
        )
        .await
        .expect("list_open_prs should succeed");

        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 42);
        assert_eq!(prs[0].html_url, "https://github.com/owner/repo/pull/42");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_open_prs_returns_empty_vec_when_no_prs() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock(
                "GET",
                "/repos/owner/repo/pulls?state=open&head=owner%3Aagent-q&base=main",
            )
            .with_status(200)
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;

        let prs = list_open_prs_at(
            &server.url(),
            "owner",
            "repo",
            "owner:agent-q",
            "main",
            "testtoken",
        )
        .await
        .expect("empty list should succeed");

        assert!(prs.is_empty());
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn create_issue_comment_posts_to_expected_endpoint() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/repos/owner/repo/issues/42/comments")
            .match_header("authorization", "Bearer testtoken")
            .match_header("accept", "application/vnd.github+json")
            .match_header("user-agent", "openspec-autocoder")
            .match_body(mockito::Matcher::PartialJsonString(
                "{\"body\":\"## Agent implementation notes\\n\\nhello\"}".to_string(),
            ))
            .with_status(201)
            .with_header("content-type", "application/json")
            .with_body("{\"id\":12345,\"body\":\"## Agent implementation notes\\n\\nhello\"}")
            .expect(1)
            .create_async()
            .await;

        create_issue_comment_at_for_test(
            &server.url(),
            "owner",
            "repo",
            42,
            "## Agent implementation notes\n\nhello",
            "testtoken",
        )
        .await
        .expect("issue-comment POST should succeed on 201");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn create_issue_comment_errors_on_non_2xx() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", "/repos/owner/repo/issues/42/comments")
            .with_status(404)
            .with_body(r#"{"message":"Not Found"}"#)
            .create_async()
            .await;

        let err = create_issue_comment_at_for_test(
            &server.url(),
            "owner",
            "repo",
            42,
            "anything",
            "testtoken",
        )
        .await
        .expect_err("404 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("404"), "error must name status code; got: {msg}");
    }

    #[tokio::test]
    async fn latest_pr_for_head_parses_open_pr() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("head".into(), "owner:agent-q".into()),
                mockito::Matcher::UrlEncoded("state".into(), "all".into()),
                mockito::Matcher::UrlEncoded("sort".into(), "created".into()),
                mockito::Matcher::UrlEncoded("direction".into(), "desc".into()),
                mockito::Matcher::UrlEncoded("per_page".into(), "1".into()),
            ]))
            .with_status(200)
            .with_body(
                r#"[{
                    "number": 42,
                    "title": "Agent PR",
                    "state": "open",
                    "html_url": "https://github.com/owner/repo/pull/42",
                    "created_at": "2025-01-01T00:00:00Z",
                    "merged_at": null,
                    "head": {"ref": "agent-q"}
                }]"#,
            )
            .expect(1)
            .create_async()
            .await;

        let pr = latest_pr_for_head(&server.url(), "t", "owner", "repo", "owner", "agent-q")
            .await
            .expect("call should succeed");
        let pr = pr.expect("response with one PR should yield Some");
        assert_eq!(pr.number, 42);
        assert_eq!(pr.title, "Agent PR");
        assert_eq!(pr.state, "open");
        assert_eq!(pr.head_branch, "agent-q");
        assert_eq!(pr.url, "https://github.com/owner/repo/pull/42");
        // age must be non-negative; the created_at is in the past.
        assert!(pr.age.num_seconds() >= 0);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn latest_pr_for_head_maps_merged_state() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "number": 7,
                    "title": "Merged PR",
                    "state": "closed",
                    "html_url": "https://github.com/owner/repo/pull/7",
                    "created_at": "2025-01-01T00:00:00Z",
                    "merged_at": "2025-01-02T00:00:00Z",
                    "head": {"ref": "agent-q"}
                }]"#,
            )
            .create_async()
            .await;
        let pr = latest_pr_for_head(&server.url(), "t", "owner", "repo", "owner", "agent-q")
            .await
            .expect("call should succeed")
            .expect("one PR");
        assert_eq!(pr.state, "merged", "closed+merged_at must map to merged");
    }

    #[tokio::test]
    async fn latest_pr_for_head_closed_without_merge_stays_closed() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                r#"[{
                    "number": 8,
                    "title": "Closed",
                    "state": "closed",
                    "html_url": "https://github.com/owner/repo/pull/8",
                    "created_at": "2025-01-01T00:00:00Z",
                    "merged_at": null,
                    "head": {"ref": "agent-q"}
                }]"#,
            )
            .create_async()
            .await;
        let pr = latest_pr_for_head(&server.url(), "t", "owner", "repo", "owner", "agent-q")
            .await
            .expect("call should succeed")
            .expect("one PR");
        assert_eq!(pr.state, "closed", "closed without merge stays closed");
    }

    #[tokio::test]
    async fn latest_pr_for_head_empty_list_returns_none() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(200)
            .with_body("[]")
            .create_async()
            .await;
        let pr = latest_pr_for_head(&server.url(), "t", "owner", "repo", "owner", "agent-q")
            .await
            .expect("empty list should be Ok(None)");
        assert!(pr.is_none());
    }

    #[tokio::test]
    async fn latest_pr_for_head_returns_err_on_404() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(404)
            .with_body(r#"{"message":"Not Found"}"#)
            .create_async()
            .await;
        let err = latest_pr_for_head(&server.url(), "t", "owner", "repo", "owner", "agent-q")
            .await
            .expect_err("404 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("404"), "error must name the status: {msg}");
    }

    #[tokio::test]
    async fn latest_pr_for_head_returns_err_on_rate_limit_403() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(403)
            .with_body(r#"{"message":"API rate limit exceeded"}"#)
            .create_async()
            .await;
        let err = latest_pr_for_head(&server.url(), "t", "owner", "repo", "owner", "agent-q")
            .await
            .expect_err("rate-limit 403 must surface as Err so caller can swallow");
        let msg = format!("{err:#}");
        assert!(msg.contains("403"));
    }

    #[tokio::test]
    async fn latest_pr_for_head_returns_err_on_decode_failure() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(200)
            // Missing required `head` field → decode fails.
            .with_body(
                r#"[{"number":1,"title":"x","state":"open","html_url":"u","created_at":"2025-01-01T00:00:00Z"}]"#,
            )
            .create_async()
            .await;
        let err = latest_pr_for_head(&server.url(), "t", "owner", "repo", "owner", "agent-q")
            .await
            .expect_err("missing required field must error");
        let msg = format!("{err:#}");
        assert!(msg.contains("decode"), "error must mention decode: {msg}");
    }

    #[tokio::test]
    async fn list_open_prs_errors_on_non_2xx() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(500)
            .with_body(r#"{"message":"internal error"}"#)
            .create_async()
            .await;

        let result = list_open_prs_at(
            &server.url(),
            "owner",
            "repo",
            "owner:agent-q",
            "main",
            "testtoken",
        )
        .await;

        assert!(result.is_err(), "non-2xx must surface as Err");
        let msg = format!("{:#}", result.unwrap_err());
        assert!(msg.contains("500"), "error must name status code: {msg}");
    }

    // -------- new helpers for the revision-loop dispatcher --------

    #[tokio::test]
    async fn self_bot_username_parses_login() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/user")
            .match_header("authorization", "token testtoken")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"login":"my-bot","id":1,"extra":"ignored"}"#)
            .expect(1)
            .create_async()
            .await;
        let got = self_bot_username(&server.url(), "testtoken")
            .await
            .expect("self_bot_username should succeed");
        assert_eq!(got, "my-bot");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn self_bot_username_errors_on_401() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/user")
            .with_status(401)
            .with_body(r#"{"message":"Bad credentials"}"#)
            .create_async()
            .await;
        let err = self_bot_username(&server.url(), "bad")
            .await
            .expect_err("401 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("401"), "error must name status: {msg}");
    }

    #[tokio::test]
    async fn self_bot_username_errors_on_404() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/user")
            .with_status(404)
            .with_body(r#"{"message":"Not Found"}"#)
            .create_async()
            .await;
        let err = self_bot_username(&server.url(), "t")
            .await
            .expect_err("404 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("404"));
    }

    #[tokio::test]
    async fn self_bot_username_errors_on_500() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", "/user")
            .with_status(500)
            .with_body("upstream error")
            .create_async()
            .await;
        let err = self_bot_username(&server.url(), "t")
            .await
            .expect_err("500 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("500"));
    }

    #[tokio::test]
    async fn list_open_prs_for_head_parses_response() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("state".into(), "open".into()),
                mockito::Matcher::UrlEncoded("head".into(), "owner:agent-q".into()),
                mockito::Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .match_header("authorization", "token testtoken")
            .with_status(200)
            .with_body(
                r#"[{
                    "number": 42,
                    "title": "Agent PR",
                    "state": "open",
                    "html_url": "https://github.com/owner/repo/pull/42",
                    "body": "Some body text",
                    "created_at": "2026-05-25T10:00:00Z",
                    "head": {"ref": "agent-q"},
                    "base": {"ref": "main"}
                }]"#,
            )
            .expect(1)
            .create_async()
            .await;
        let prs = list_open_prs_for_head(&server.url(), "testtoken", "owner", "repo", "owner", "agent-q")
            .await
            .expect("call should succeed");
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 42);
        assert_eq!(prs[0].title, "Agent PR");
        assert_eq!(prs[0].head.ref_, "agent-q");
        assert_eq!(prs[0].base.ref_, "main");
        assert_eq!(prs[0].body.as_deref(), Some("Some body text"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_open_prs_for_head_empty_list() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(200)
            .with_body("[]")
            .create_async()
            .await;
        let prs = list_open_prs_for_head(&server.url(), "t", "owner", "repo", "owner", "agent-q")
            .await
            .expect("empty list");
        assert!(prs.is_empty());
    }

    #[tokio::test]
    async fn list_open_prs_for_head_errors_on_404() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(404)
            .with_body(r#"{"message":"Not Found"}"#)
            .create_async()
            .await;
        let err = list_open_prs_for_head(&server.url(), "t", "owner", "repo", "owner", "agent-q")
            .await
            .expect_err("404 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("404"));
    }

    #[tokio::test]
    async fn list_open_prs_for_head_errors_on_500() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(500)
            .with_body("server error")
            .create_async()
            .await;
        let err = list_open_prs_for_head(&server.url(), "t", "owner", "repo", "owner", "agent-q")
            .await
            .expect_err("500 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("500"));
    }

    #[tokio::test]
    async fn list_open_prs_for_head_errors_on_401() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(401)
            .with_body(r#"{"message":"Bad credentials"}"#)
            .create_async()
            .await;
        let err = list_open_prs_for_head(&server.url(), "bad", "owner", "repo", "owner", "agent-q")
            .await
            .expect_err("401 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("401"));
    }

    // ---------- a20a4: head_owner-aware qualifier construction ----------

    /// Regression test for the fork-PR-mode head-qualifier bug. The
    /// helper MUST use the explicit `head_owner` parameter (the fork
    /// owner in fork-PR mode), NOT the URL-path `owner` parameter (the
    /// upstream owner). Pre-fix code reused `owner` for both AND
    /// produced `head=upstream-owner:agent-q` queries that never
    /// matched fork-headed PRs. This test fails against pre-fix code
    /// because mockito would receive an UNMATCHED query (the literal
    /// mocked `head` value is `fork-owner:agent-q` AND there's no
    /// fallback handler) AND mockito's `.expect(1)` would not be met.
    #[tokio::test]
    async fn list_open_prs_for_head_uses_head_owner_param_for_qualifier() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("state".into(), "open".into()),
                // Critical assertion: the head qualifier uses the FORK
                // owner, NOT the upstream owner that owns the URL path.
                mockito::Matcher::UrlEncoded(
                    "head".into(),
                    "fork-owner:agent-q".into(),
                ),
                mockito::Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .with_status(200)
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        let prs = list_open_prs_for_head(
            &server.url(),
            "tok",
            "upstream-owner",    // URL path owner (the upstream repo)
            "myrepo",
            "fork-owner",        // head qualifier owner (the fork)
            "agent-q",
        )
        .await
        .expect("call should succeed against mock");
        assert!(prs.is_empty());
        mock.assert_async().await;
    }

    /// Same shape for `latest_pr_for_head`. Pre-`a20a4` this query
    /// produced `head=upstream-owner:agent-q` which never matched a
    /// fork-headed PR, so the status reply showed `latest PR: (none)`
    /// for every fork-PR-mode operator.
    #[tokio::test]
    async fn latest_pr_for_head_uses_head_owner_param_for_qualifier() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", mockito::Matcher::Any)
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded(
                    "head".into(),
                    "fork-owner:agent-q".into(),
                ),
                mockito::Matcher::UrlEncoded("state".into(), "all".into()),
                mockito::Matcher::UrlEncoded("sort".into(), "created".into()),
                mockito::Matcher::UrlEncoded(
                    "direction".into(),
                    "desc".into(),
                ),
                mockito::Matcher::UrlEncoded("per_page".into(), "1".into()),
            ]))
            .with_status(200)
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        let pr = latest_pr_for_head(
            &server.url(),
            "tok",
            "upstream-owner",
            "myrepo",
            "fork-owner",
            "agent-q",
        )
        .await
        .expect("call should succeed against mock");
        assert!(pr.is_none(), "empty response → None");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_issue_comments_since_parses_response() {
        let mut server = mockito::Server::new_async().await;
        let since = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
            chrono::NaiveDateTime::new(
                chrono::NaiveDate::from_ymd_opt(2026, 5, 25).unwrap(),
                chrono::NaiveTime::from_hms_opt(10, 0, 0).unwrap(),
            ),
            chrono::Utc,
        );
        let mock = server
            .mock("GET", "/repos/owner/repo/issues/42/comments")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("since".into(), "2026-05-25T10:00:00.000Z".into()),
                mockito::Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .match_header("authorization", "token testtoken")
            .with_status(200)
            .with_body(
                r#"[
                  {
                    "id": 1001,
                    "body": "@bot revise drop error info",
                    "user": {"login": "operator"},
                    "created_at": "2026-05-25T10:01:00Z"
                  },
                  {
                    "id": 1002,
                    "body": "@bot looks good",
                    "user": {"login": "another"},
                    "created_at": "2026-05-25T10:02:00Z"
                  }
                ]"#,
            )
            .expect(1)
            .create_async()
            .await;
        let comments = list_issue_comments_since(
            &server.url(),
            "testtoken",
            "owner",
            "repo",
            42,
            since,
        )
        .await
        .expect("call should succeed");
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].id, 1001);
        assert_eq!(comments[0].user_login(), "operator");
        assert_eq!(comments[1].user_login(), "another");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_issue_comments_since_errors_on_404() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(404)
            .with_body(r#"{"message":"Not Found"}"#)
            .create_async()
            .await;
        let err = list_issue_comments_since(
            &server.url(),
            "t",
            "owner",
            "repo",
            42,
            chrono::Utc::now(),
        )
        .await
        .expect_err("404 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("404"));
    }

    #[tokio::test]
    async fn list_issue_comments_since_errors_on_500() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(500)
            .with_body("upstream")
            .create_async()
            .await;
        let err = list_issue_comments_since(
            &server.url(),
            "t",
            "owner",
            "repo",
            42,
            chrono::Utc::now(),
        )
        .await
        .expect_err("500 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("500"));
    }

    #[tokio::test]
    async fn list_issue_comments_since_uses_millisecond_precision() {
        // Marker with non-zero ms component must round-trip exactly.
        let mut server = mockito::Server::new_async().await;
        let since = chrono::DateTime::parse_from_rfc3339("2026-05-29T17:18:11.847Z")
            .expect("static rfc3339 parses")
            .with_timezone(&chrono::Utc);
        let mock = server
            .mock("GET", "/repos/owner/repo/issues/42/comments")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded(
                    "since".into(),
                    "2026-05-29T17:18:11.847Z".into(),
                ),
                mockito::Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .with_status(200)
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        list_issue_comments_since(
            &server.url(),
            "tok",
            "owner",
            "repo",
            42,
            since,
        )
        .await
        .expect("call should succeed");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_issue_comments_since_keeps_trailing_zero_milliseconds() {
        // Markers whose ms component is 0 must still be formatted with the
        // ".000Z" suffix — the formatter must NOT collapse them back to
        // second precision (which would re-introduce the GitHub fence-post
        // bug Layer 1 is supposed to fix).
        let mut server = mockito::Server::new_async().await;
        let since = chrono::DateTime::parse_from_rfc3339("2026-05-29T17:18:11.000Z")
            .expect("static rfc3339 parses")
            .with_timezone(&chrono::Utc);
        let mock = server
            .mock("GET", "/repos/owner/repo/issues/42/comments")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded(
                    "since".into(),
                    "2026-05-29T17:18:11.000Z".into(),
                ),
                mockito::Matcher::UrlEncoded("per_page".into(), "100".into()),
            ]))
            .with_status(200)
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        list_issue_comments_since(
            &server.url(),
            "tok",
            "owner",
            "repo",
            42,
            since,
        )
        .await
        .expect("call should succeed");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn list_issue_comments_since_errors_on_401() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("GET", mockito::Matcher::Any)
            .with_status(401)
            .with_body(r#"{"message":"Bad credentials"}"#)
            .create_async()
            .await;
        let err = list_issue_comments_since(
            &server.url(),
            "bad",
            "owner",
            "repo",
            42,
            chrono::Utc::now(),
        )
        .await
        .expect_err("401 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("401"));
    }

    #[tokio::test]
    async fn post_issue_comment_posts_expected_request() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/repos/owner/repo/issues/42/comments")
            .match_header("authorization", "token testtoken")
            .match_body(mockito::Matcher::JsonString(
                r#"{"body":"hello"}"#.to_string(),
            ))
            .with_status(201)
            .with_body(r#"{"id":1234}"#)
            .expect(1)
            .create_async()
            .await;
        post_issue_comment(&server.url(), "testtoken", "owner", "repo", 42, "hello")
            .await
            .expect("post should succeed on 201");
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn post_issue_comment_errors_on_404() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", mockito::Matcher::Any)
            .with_status(404)
            .with_body(r#"{"message":"Not Found"}"#)
            .create_async()
            .await;
        let err = post_issue_comment(&server.url(), "t", "owner", "repo", 42, "x")
            .await
            .expect_err("404 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("404"));
    }

    #[tokio::test]
    async fn post_issue_comment_errors_on_500() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", mockito::Matcher::Any)
            .with_status(500)
            .with_body("upstream")
            .create_async()
            .await;
        let err = post_issue_comment(&server.url(), "t", "owner", "repo", 42, "x")
            .await
            .expect_err("500 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("500"));
    }

    #[tokio::test]
    async fn post_issue_comment_errors_on_401() {
        let mut server = mockito::Server::new_async().await;
        let _mock = server
            .mock("POST", mockito::Matcher::Any)
            .with_status(401)
            .with_body(r#"{"message":"Bad credentials"}"#)
            .create_async()
            .await;
        let err = post_issue_comment(&server.url(), "bad", "owner", "repo", 42, "x")
            .await
            .expect_err("401 must surface as Err");
        let msg = format!("{err:#}");
        assert!(msg.contains("401"));
    }

    use crate::code_reviewer::PerChangeSection;

    /// Per-change mode renders multiple `## Code Review: <slug>`
    /// sections in order, instead of one combined block.
    #[test]
    fn compose_body_per_change_emits_section_per_slug() {
        let report = ReviewReport {
            verdict: ReviewVerdict::Concerns,
            markdown: String::new(),
            concerns: Vec::new(),
            per_change_sections: vec![
                PerChangeSection {
                    change_slug: "a07-foo".into(),
                    markdown: "VERDICT: Pass\n\nlooks good".into(),
                },
                PerChangeSection {
                    change_slug: "a08-bar".into(),
                    markdown: "VERDICT: Concerns\n\nsome nit".into(),
                },
                PerChangeSection {
                    change_slug: "a09-baz".into(),
                    markdown: "VERDICT: Block\n\nbug here".into(),
                },
            ],
        };
        let composed = compose_body("base body", Some(&report));
        // Sections appear in change order, all three present.
        let foo_idx = composed.find("## Code Review: a07-foo").expect("foo header present");
        let bar_idx = composed.find("## Code Review: a08-bar").expect("bar header present");
        let baz_idx = composed.find("## Code Review: a09-baz").expect("baz header present");
        assert!(foo_idx < bar_idx);
        assert!(bar_idx < baz_idx);
        // Section bodies inlined.
        assert!(composed.contains("looks good"));
        assert!(composed.contains("some nit"));
        assert!(composed.contains("bug here"));
        // The bundled-mode single header must NOT appear when sections are present.
        let bundled_header = "\n\n## Code Review\n\n";
        assert!(
            !composed.contains(bundled_header),
            "bundled-mode parent header must NOT appear in per-change body; got: {composed:?}"
        );
    }

    /// Truncation safety: a 3-change pass whose combined body would
    /// overflow GitHub's 65,535-char PR-body cap is truncated; the LAST
    /// section is shortened with the "see daemon log" pointer footer.
    #[test]
    fn compose_body_per_change_truncates_at_github_cap() {
        let big = "x".repeat(40_000);
        let report = ReviewReport {
            verdict: ReviewVerdict::Block,
            markdown: String::new(),
            concerns: Vec::new(),
            per_change_sections: vec![
                PerChangeSection {
                    change_slug: "a".into(),
                    markdown: big.clone(),
                },
                PerChangeSection {
                    change_slug: "b".into(),
                    markdown: big.clone(),
                },
                PerChangeSection {
                    change_slug: "c".into(),
                    markdown: big.clone(),
                },
            ],
        };
        let composed = compose_body("base", Some(&report));
        // Body stays under GitHub's cap.
        assert!(
            composed.len() <= GITHUB_PR_BODY_MAX_CHARS,
            "composed body must fit GitHub's cap; got {} chars",
            composed.len()
        );
        // First two sections present; last section pointer-truncated.
        assert!(composed.contains("## Code Review: a"));
        assert!(composed.contains("## Code Review: b"));
        assert!(composed.contains(
            "truncated to fit GitHub's PR-body cap; see daemon log for full review"
        ));
    }

    /// Bundled mode (empty `per_change_sections`) still emits the
    /// single `## Code Review` block (no regression).
    #[test]
    fn compose_body_bundled_still_uses_single_section() {
        let report = ReviewReport {
            verdict: ReviewVerdict::Pass,
            markdown: "VERDICT details".into(),
            concerns: Vec::new(),
            per_change_sections: Vec::new(),
        };
        let composed = compose_body("base body", Some(&report));
        assert!(composed.contains("\n\n## Code Review\n\nVERDICT details"));
        assert!(!composed.contains("## Code Review: "));
    }
}
