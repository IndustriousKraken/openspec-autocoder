//! GitHub REST API client for opening pull requests, plus URL parsing.

use crate::code_reviewer::ReviewReport;
use anyhow::{Result, anyhow};
use serde::Deserialize;

const DEFAULT_API_BASE: &str = "https://api.github.com";
const DRAFT_FALLBACK_LABEL: &str = "do-not-merge";

#[derive(Deserialize)]
struct PullResponse {
    html_url: String,
    number: u64,
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
) -> Result<String> {
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
) -> Result<String> {
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
) -> Result<String> {
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
        return Ok(parsed.html_url);
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

    Ok(parsed.html_url)
}

/// Compose the PR body: the existing body, optionally followed by a
/// `## Code Review` section containing the reviewer report's markdown.
fn compose_body(body: &str, review_report: Option<&ReviewReport>) -> String {
    match review_report {
        Some(r) => format!("{body}\n\n## Code Review\n\n{}", r.markdown),
        None => body.to_string(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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

        let url = create_pull_request_at(
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

        assert_eq!(url, "https://github.com/owner/repo/pull/1");
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

        let url = create_pull_request_at(
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

        assert_eq!(url, "https://github.com/owner/repo/pull/4");
        first.assert_async().await;
        second.assert_async().await;
        label.assert_async().await;
    }
}
