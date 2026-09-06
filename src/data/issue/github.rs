//! `GithubIssueSource` — GitHub implementation of `IssueSource`.

use std::path::Path;
use std::process::Command;

use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};

use super::{Issue, IssueSource, IssueSourceError};

/// Hint embedded into `IssueSourceError::Unauthorized` by GitHub auth/403
/// failures. Provider-specific text lives here, not in the trait-level enum.
const GH_AUTH_HINT: &str = "ensure gh is authenticated (`gh auth login`) or GITHUB_TOKEN is set";

pub struct GithubIssueSource;

impl IssueSource for GithubIssueSource {
    fn provider_name(&self) -> &str {
        "GitHub"
    }

    fn provider_prefix(&self) -> &str {
        "ghb"
    }

    fn issue_identifier(&self, issue: &Issue) -> String {
        issue
            .numeric_id()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "0".to_string())
    }

    fn can_handle(&self, input: &str) -> bool {
        let input = input.trim();
        if input.is_empty() {
            return false;
        }
        // Bare integer
        if input.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
        // GitHub URL
        if input.starts_with("https://github.com/") || input.starts_with("http://github.com/") {
            return true;
        }
        // owner/repo#N short form
        if input.contains('#') && !input.contains("://") {
            return true;
        }
        false
    }

    fn fetch_issue(&self, input: &str, git_root: &Path) -> Result<Issue, IssueSourceError> {
        let input = input.trim();
        let (owner, repo, number) = parse_input(input, git_root, self.provider_name())?;

        // Try gh CLI first
        if let Some(issue) = try_gh_cli(&owner, &repo, number, self.provider_name()) {
            return Ok(issue);
        }

        // Fall back to REST API
        fetch_rest_api(&owner, &repo, number, self.provider_name())
    }

    fn fetch_issue_with_progress(
        &self,
        input: &str,
        git_root: &Path,
        sink: &mut dyn UserMessageSink,
    ) -> Result<Issue, IssueSourceError> {
        fetch_issue_with_progress_using(
            "gh",
            "https://api.github.com",
            input,
            git_root,
            self.provider_name(),
            sink,
        )
    }
}

/// Inner implementation for `fetch_issue_with_progress`, parameterised so
/// tests can supply a deterministic CLI and REST endpoint without changing the
/// process-global `PATH` or reaching the network.
fn fetch_issue_with_progress_using(
    gh_command: &str,
    api_base_url: &str,
    input: &str,
    git_root: &Path,
    provider: &str,
    sink: &mut dyn UserMessageSink,
) -> Result<Issue, IssueSourceError> {
    let input = input.trim();
    let (owner, repo, number) = parse_input(input, git_root, provider)?;

    sink.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!(
            "running: gh issue view {number} --repo {owner}/{repo} --json number,title,body,url"
        ),
    });

    if let Some(issue) = try_gh_cli_with_cmd(gh_command, &owner, &repo, number, provider) {
        return Ok(issue);
    }

    let api_base_url = api_base_url.trim_end_matches('/');
    let api_url = format!("{api_base_url}/repos/{owner}/{repo}/issues/{number}");
    sink.write_message(UserMessage {
        level: MessageLevel::Info,
        text: format!("gh CLI unavailable, falling back to REST API: GET {api_url}"),
    });

    fetch_rest_api_with_base(api_base_url, &owner, &repo, number, provider)
}

/// Parse user input into (owner, repo, number).
fn parse_input(
    input: &str,
    git_root: &Path,
    provider: &str,
) -> Result<(String, String, u32), IssueSourceError> {
    // Bare integer — resolve from git remote
    if input.chars().all(|c| c.is_ascii_digit()) {
        let number = input
            .parse::<u32>()
            .map_err(|_| IssueSourceError::InvalidRef {
                provider: provider.to_string(),
                input: input.to_string(),
                hint: "could not parse as issue number".to_string(),
            })?;
        let (owner, repo) = detect_github_remote(git_root, provider)?;
        return Ok((owner, repo, number));
    }

    // Full URL: https://github.com/{owner}/{repo}/issues/{number}
    if input.starts_with("https://github.com/") || input.starts_with("http://github.com/") {
        return parse_github_url(input, provider);
    }

    // Non-GitHub URL
    if input.contains("://") {
        return Err(IssueSourceError::InvalidRef {
            provider: provider.to_string(),
            input: input.to_string(),
            hint: "URL does not point to github.com".to_string(),
        });
    }

    // owner/repo#N short form
    if let Some(hash_pos) = input.find('#') {
        let owner_repo = &input[..hash_pos];
        let number_str = &input[hash_pos + 1..];
        let number = number_str
            .parse::<u32>()
            .map_err(|_| IssueSourceError::InvalidRef {
                provider: provider.to_string(),
                input: input.to_string(),
                hint: format!("could not parse issue number from '{number_str}'"),
            })?;
        let parts: Vec<&str> = owner_repo.splitn(2, '/').collect();
        if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            return Err(IssueSourceError::InvalidRef {
                provider: provider.to_string(),
                input: input.to_string(),
                hint: "expected format: owner/repo#number".to_string(),
            });
        }
        return Ok((parts[0].to_string(), parts[1].to_string(), number));
    }

    Err(IssueSourceError::InvalidRef {
        provider: provider.to_string(),
        input: input.to_string(),
        hint: "expected a number, GitHub URL, or owner/repo#number".to_string(),
    })
}

fn parse_github_url(url: &str, provider: &str) -> Result<(String, String, u32), IssueSourceError> {
    // Strip scheme + host
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .unwrap_or("");
    let segments: Vec<&str> = path.split('/').collect();
    // Expected: {owner}/{repo}/issues/{number}
    if segments.len() >= 4 && segments[2] == "issues" {
        let number = segments[3]
            .parse::<u32>()
            .map_err(|_| IssueSourceError::InvalidRef {
                provider: provider.to_string(),
                input: url.to_string(),
                hint: format!(
                    "could not parse issue number from URL segment '{}'",
                    segments[3]
                ),
            })?;
        Ok((segments[0].to_string(), segments[1].to_string(), number))
    } else {
        Err(IssueSourceError::InvalidRef {
            provider: provider.to_string(),
            input: url.to_string(),
            hint: "expected URL format: https://github.com/owner/repo/issues/NUMBER".to_string(),
        })
    }
}

/// Detect the GitHub remote's owner/repo from the current git repo.
fn detect_github_remote(
    git_root: &Path,
    provider: &str,
) -> Result<(String, String), IssueSourceError> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(git_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => {
            return Err(IssueSourceError::NoRemoteDetected {
                provider: provider.to_string(),
            });
        }
    };

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_owner_repo_from_remote(&url).ok_or_else(|| IssueSourceError::NoRemoteDetected {
        provider: provider.to_string(),
    })
}

/// Extract owner/repo from various git remote URL formats.
fn parse_owner_repo_from_remote(remote_url: &str) -> Option<(String, String)> {
    let remote = remote_url.trim();

    // SSH: git@github.com:owner/repo.git
    if let Some(rest) = remote.strip_prefix("git@github.com:") {
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let parts: Vec<&str> = rest.splitn(2, '/').collect();
        if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some((parts[0].to_string(), parts[1].to_string()));
        }
    }

    // HTTPS: https://github.com/owner/repo.git
    let path = remote
        .strip_prefix("https://github.com/")
        .or_else(|| remote.strip_prefix("http://github.com/"))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    let parts: Vec<&str> = path.splitn(3, '/').collect();
    if parts.len() >= 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}

/// Try fetching via `gh issue view`.
fn try_gh_cli(owner: &str, repo: &str, number: u32, provider: &str) -> Option<Issue> {
    try_gh_cli_with_cmd("gh", owner, repo, number, provider)
}

/// Inner implementation for `try_gh_cli`, parameterised on the gh binary path.
/// Exposed as `pub(crate)` so tests can inject a fake `gh` script.
pub(crate) fn try_gh_cli_with_cmd(
    gh_cmd: &str,
    owner: &str,
    repo: &str,
    number: u32,
    provider: &str,
) -> Option<Issue> {
    let number = number.to_string();
    let owner_repo = format!("{owner}/{repo}");
    let mut retries = 0;
    let output = loop {
        let result = Command::new(gh_cmd)
            .args([
                "issue",
                "view",
                &number,
                "--repo",
                &owner_repo,
                "--json",
                "number,title,body,url",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();
        match result {
            Ok(output) => break output,
            // Parallel tests can fork while another test is still writing its
            // fake gh executable. The fork inherits that write descriptor,
            // creating a short ETXTBSY window even after the writer closes it.
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy && retries < 20 =>
            {
                retries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    };

    if !output.status.success() {
        return None;
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let title = json.get("title")?.as_str()?.to_string();
    let body = json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let source_id = json
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let source_id = if source_id.is_empty() {
        format!("https://github.com/{owner}/{repo}/issues/{number}")
    } else {
        source_id
    };

    Some(Issue {
        source_id,
        title,
        body,
        provider: provider.to_string(),
    })
}

/// Fetch via GitHub REST API.
fn fetch_rest_api(
    owner: &str,
    repo: &str,
    number: u32,
    provider: &str,
) -> Result<Issue, IssueSourceError> {
    fetch_rest_api_with_base("https://api.github.com", owner, repo, number, provider)
}

/// Inner implementation for `fetch_rest_api`, parameterised on the API base URL.
/// Exposed as `pub(crate)` so tests can inject a wiremock server URL.
pub(crate) fn fetch_rest_api_with_base(
    base_url: &str,
    owner: &str,
    repo: &str,
    number: u32,
    provider: &str,
) -> Result<Issue, IssueSourceError> {
    let url = format!("{base_url}/repos/{owner}/{repo}/issues/{number}");
    let client = reqwest::blocking::Client::builder()
        .user_agent("awman")
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| IssueSourceError::Network {
            provider: provider.to_string(),
            detail: e.to_string(),
        })?;

    let mut request = client.get(&url);

    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
    }

    let response = request.send().map_err(|e| IssueSourceError::Network {
        provider: provider.to_string(),
        detail: e.to_string(),
    })?;

    let status = response.status().as_u16();
    match status {
        200 => {}
        401 | 403 => {
            // 403 can be rate limiting or an auth issue; disambiguate by body.
            let body = response.text().unwrap_or_default();
            if body.contains("rate limit") {
                return Err(IssueSourceError::RateLimited {
                    provider: provider.to_string(),
                });
            }
            return Err(IssueSourceError::Unauthorized {
                provider: provider.to_string(),
                hint: GH_AUTH_HINT.to_string(),
            });
        }
        404 => {
            return Err(IssueSourceError::NotFound {
                provider: provider.to_string(),
                source_id: format!("https://github.com/{owner}/{repo}/issues/{number}"),
            });
        }
        429 => {
            return Err(IssueSourceError::RateLimited {
                provider: provider.to_string(),
            });
        }
        _ => {
            let body = response.text().unwrap_or_default();
            return Err(IssueSourceError::ProviderError {
                provider: provider.to_string(),
                detail: format!("HTTP {status}: {body}"),
            });
        }
    }

    let json: serde_json::Value = response
        .json()
        .map_err(|e| IssueSourceError::ProviderError {
            provider: provider.to_string(),
            detail: format!("failed to parse response: {e}"),
        })?;

    let title = json
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let body = json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok(Issue {
        source_id: format!("https://github.com/{owner}/{repo}/issues/{number}"),
        title,
        body,
        provider: provider.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_handle_bare_integer() {
        assert!(GithubIssueSource.can_handle("84"));
        assert!(GithubIssueSource.can_handle("1"));
    }

    #[test]
    fn can_handle_github_url() {
        assert!(GithubIssueSource.can_handle("https://github.com/owner/repo/issues/42"));
        assert!(GithubIssueSource.can_handle("http://github.com/owner/repo/issues/42"));
    }

    #[test]
    fn can_handle_short_form() {
        assert!(GithubIssueSource.can_handle("owner/repo#42"));
    }

    #[test]
    fn can_handle_rejects_non_github() {
        assert!(!GithubIssueSource.can_handle("https://gitlab.com/foo/bar/issues/1"));
        assert!(!GithubIssueSource.can_handle(""));
        assert!(!GithubIssueSource.can_handle("not-a-reference"));
    }

    #[test]
    fn parse_github_url_extracts_triple() {
        let (owner, repo, num) = parse_github_url(
            "https://github.com/prettysmartdev/awman/issues/84",
            "GitHub",
        )
        .unwrap();
        assert_eq!(owner, "prettysmartdev");
        assert_eq!(repo, "awman");
        assert_eq!(num, 84);
    }

    #[test]
    fn parse_github_url_invalid_returns_error() {
        assert!(parse_github_url("https://github.com/owner/repo", "GitHub").is_err());
    }

    #[test]
    fn parse_owner_repo_ssh() {
        let (o, r) = parse_owner_repo_from_remote("git@github.com:owner/repo.git").unwrap();
        assert_eq!(o, "owner");
        assert_eq!(r, "repo");
    }

    #[test]
    fn parse_owner_repo_https() {
        let (o, r) = parse_owner_repo_from_remote("https://github.com/owner/repo.git").unwrap();
        assert_eq!(o, "owner");
        assert_eq!(r, "repo");
    }

    #[test]
    fn parse_owner_repo_https_no_git_suffix() {
        let (o, r) = parse_owner_repo_from_remote("https://github.com/owner/repo").unwrap();
        assert_eq!(o, "owner");
        assert_eq!(r, "repo");
    }

    #[test]
    fn parse_owner_repo_non_github_returns_none() {
        assert!(parse_owner_repo_from_remote("https://gitlab.com/owner/repo").is_none());
    }

    #[test]
    fn provider_prefix_is_ghb() {
        assert_eq!(GithubIssueSource.provider_prefix(), "ghb");
    }

    #[test]
    fn issue_identifier_extracts_number() {
        let issue = Issue {
            source_id: "https://github.com/owner/repo/issues/84".into(),
            title: String::new(),
            body: String::new(),
            provider: "GitHub".into(),
        };
        assert_eq!(GithubIssueSource.issue_identifier(&issue), "84");
    }

    #[test]
    fn issue_identifier_non_numeric_falls_back() {
        let issue = Issue {
            source_id: "https://example.com/not-numeric".into(),
            title: String::new(),
            body: String::new(),
            provider: "GitHub".into(),
        };
        assert_eq!(GithubIssueSource.issue_identifier(&issue), "0");
    }

    #[test]
    fn title_slug_standard_case() {
        let issue = Issue {
            source_id: "https://github.com/prettysmartdev/awman/issues/84".into(),
            title: "GitHub Integration Part 1".into(),
            body: String::new(),
            provider: "GitHub".into(),
        };
        assert_eq!(
            GithubIssueSource.title_slug(&issue),
            "ghb84-github-integration-part-1"
        );
    }

    #[test]
    fn title_slug_empty_title() {
        let issue = Issue {
            source_id: "https://github.com/owner/repo/issues/42".into(),
            title: String::new(),
            body: String::new(),
            provider: "GitHub".into(),
        };
        assert_eq!(GithubIssueSource.title_slug(&issue), "ghb42");
    }

    #[test]
    fn title_slug_special_chars_in_title() {
        let issue = Issue {
            source_id: "https://github.com/owner/repo/issues/1".into(),
            title: "Fix: the bug!!!".into(),
            body: String::new(),
            provider: "GitHub".into(),
        };
        assert_eq!(GithubIssueSource.title_slug(&issue), "ghb1-fix-the-bug");
    }

    #[test]
    fn title_slug_non_ascii_title() {
        let issue = Issue {
            source_id: "https://github.com/owner/repo/issues/5".into(),
            title: "café résumé".into(),
            body: String::new(),
            provider: "GitHub".into(),
        };
        let slug = GithubIssueSource.title_slug(&issue);
        assert!(slug.starts_with("ghb5-"));
        assert!(slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn parse_input_short_form() {
        let tmp = tempfile::tempdir().unwrap();
        let (o, r, n) = parse_input("owner/repo#99", tmp.path(), "GitHub").unwrap();
        assert_eq!(o, "owner");
        assert_eq!(r, "repo");
        assert_eq!(n, 99);
    }

    #[test]
    fn parse_input_full_url() {
        let tmp = tempfile::tempdir().unwrap();
        let (o, r, n) = parse_input(
            "https://github.com/my-org/my-repo/issues/123",
            tmp.path(),
            "GitHub",
        )
        .unwrap();
        assert_eq!(o, "my-org");
        assert_eq!(r, "my-repo");
        assert_eq!(n, 123);
    }

    #[test]
    fn parse_input_non_github_url_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let result = parse_input("https://gitlab.com/o/r/issues/1", tmp.path(), "GitHub");
        assert!(result.is_err());
    }

    // ── Additional can_handle tests ───────────────────────────────────────────

    #[test]
    fn can_handle_empty_string_returns_false() {
        assert!(!GithubIssueSource.can_handle(""));
        assert!(!GithubIssueSource.can_handle("   "));
    }

    #[test]
    fn can_handle_bare_hash_matches_short_form_pattern() {
        // "#84" contains '#' and no URL scheme, so can_handle returns true
        // (it matches the owner/repo#N short-form pattern heuristic).
        // parse_input will later reject it with InvalidRef if owner/repo is missing.
        assert!(GithubIssueSource.can_handle("#84"));
    }

    #[test]
    fn can_handle_owner_repo_without_hash_returns_false() {
        // owner/repo without a "#" is not a GitHub short form.
        assert!(!GithubIssueSource.can_handle("owner/repo"));
    }

    #[test]
    fn can_handle_whitespace_only_returns_false() {
        assert!(!GithubIssueSource.can_handle("     "));
    }

    #[test]
    fn can_handle_http_github_url() {
        assert!(GithubIssueSource.can_handle("http://github.com/owner/repo/issues/7"));
    }

    // ── title_slug edge cases ─────────────────────────────────────────────────

    #[test]
    fn title_slug_very_long_title_truncated() {
        let long_title: String = "a".repeat(200);
        let issue = Issue {
            source_id: "https://github.com/o/r/issues/1".into(),
            title: long_title,
            body: String::new(),
            provider: "GitHub".into(),
        };
        let slug = GithubIssueSource.title_slug(&issue);
        assert!(
            slug.starts_with("ghb1-"),
            "slug must start with provider prefix and id"
        );
        assert!(slug.len() <= 100, "slug length {} exceeds 100", slug.len());
        assert!(!slug.ends_with('-'), "slug must not end with '-'");
    }

    #[test]
    fn title_slug_malformed_source_id_falls_back() {
        let issue = Issue {
            source_id: "https://example.com/not-github".into(),
            title: "Something".into(),
            body: String::new(),
            provider: "GitHub".into(),
        };
        let slug = GithubIssueSource.title_slug(&issue);
        assert!(
            slug.starts_with("ghb0"),
            "malformed source_id should fall back to 'ghb0', got: {slug}"
        );
    }

    // ── detect_github_remote tests ────────────────────────────────────────────

    #[test]
    fn detect_github_remote_fails_for_no_remote() {
        // A temp dir that is not a git repo — git remote get-url will fail.
        let tmp = tempfile::tempdir().unwrap();
        let result = detect_github_remote(tmp.path(), "GitHub");
        match result {
            Err(IssueSourceError::NoRemoteDetected { provider }) => {
                assert_eq!(provider, "GitHub");
            }
            other => panic!("expected NoRemoteDetected, got {other:?}"),
        }
    }

    // ── Fake gh CLI tests (try_gh_cli_with_cmd) ───────────────────────────────

    #[cfg(unix)]
    #[test]
    fn try_gh_cli_with_valid_json_returns_issue() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("gh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
echo '{"number":42,"title":"Test Issue","body":"Body text","url":"https://github.com/owner/repo/issues/42"}'
exit 0
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let result = try_gh_cli_with_cmd(script.to_str().unwrap(), "owner", "repo", 42, "GitHub");
        assert!(result.is_some(), "expected Some(Issue), got None");
        let issue = result.unwrap();
        assert_eq!(issue.title, "Test Issue");
        assert_eq!(issue.body, "Body text");
        assert_eq!(issue.provider, "GitHub");
    }

    #[cfg(unix)]
    #[test]
    fn try_gh_cli_with_failing_gh_returns_none() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("gh");
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let result = try_gh_cli_with_cmd(script.to_str().unwrap(), "owner", "repo", 1, "GitHub");
        assert!(result.is_none(), "expected None for failing gh");
    }

    #[cfg(unix)]
    #[test]
    fn try_gh_cli_uses_url_field_as_source_id() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("gh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
echo '{"number":10,"title":"My Issue","body":"","url":"https://github.com/org/project/issues/10"}'
exit 0
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let issue =
            try_gh_cli_with_cmd(script.to_str().unwrap(), "org", "project", 10, "GitHub").unwrap();
        assert_eq!(issue.source_id, "https://github.com/org/project/issues/10");
    }

    #[cfg(unix)]
    #[test]
    fn try_gh_cli_with_empty_url_field_constructs_canonical_url() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("gh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
echo '{"number":5,"title":"No URL Issue","body":"body","url":""}'
exit 0
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let issue =
            try_gh_cli_with_cmd(script.to_str().unwrap(), "owner", "repo", 5, "GitHub").unwrap();
        assert_eq!(
            issue.source_id, "https://github.com/owner/repo/issues/5",
            "empty url field must fall back to constructed canonical URL"
        );
    }

    // ── REST API mock tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_rest_api_200_returns_issue() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "title": "Hello Issue",
                "body": "Description here"
            })))
            .mount(&server)
            .await;

        let base_url = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            fetch_rest_api_with_base(&base_url, "owner", "repo", 42, "GitHub")
        })
        .await
        .unwrap();

        let issue = result.expect("expected Ok(Issue)");
        assert_eq!(issue.title, "Hello Issue");
        assert_eq!(issue.body, "Description here");
        assert_eq!(issue.provider, "GitHub");
    }

    #[tokio::test]
    async fn fetch_rest_api_404_returns_not_found() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/99"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let base_url = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            fetch_rest_api_with_base(&base_url, "owner", "repo", 99, "GitHub")
        })
        .await
        .unwrap();

        match result {
            Err(IssueSourceError::NotFound { provider, .. }) => {
                assert_eq!(provider, "GitHub");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_rest_api_401_returns_unauthorized() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/1"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;

        let base_url = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            fetch_rest_api_with_base(&base_url, "owner", "repo", 1, "GitHub")
        })
        .await
        .unwrap();

        match result {
            Err(IssueSourceError::Unauthorized { provider, hint }) => {
                assert_eq!(provider, "GitHub");
                assert!(
                    !hint.is_empty(),
                    "GitHub Unauthorized must carry a non-empty hint"
                );
            }
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_rest_api_403_non_rate_limit_returns_unauthorized() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/2"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let base_url = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            fetch_rest_api_with_base(&base_url, "owner", "repo", 2, "GitHub")
        })
        .await
        .unwrap();

        match result {
            Err(IssueSourceError::Unauthorized { provider, hint }) => {
                assert_eq!(provider, "GitHub");
                assert!(!hint.is_empty(), "403 Unauthorized must carry a hint");
            }
            other => panic!("expected Unauthorized, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_rest_api_429_returns_rate_limited() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/3"))
            .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
            .mount(&server)
            .await;

        let base_url = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            fetch_rest_api_with_base(&base_url, "owner", "repo", 3, "GitHub")
        })
        .await
        .unwrap();

        match result {
            Err(IssueSourceError::RateLimited { provider }) => {
                assert_eq!(provider, "GitHub");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_rest_api_source_id_is_canonical_github_url() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/myorg/myrepo/issues/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "title": "Some Issue",
                "body": null
            })))
            .mount(&server)
            .await;

        let base_url = server.uri();
        let result = tokio::task::spawn_blocking(move || {
            fetch_rest_api_with_base(&base_url, "myorg", "myrepo", 7, "GitHub")
        })
        .await
        .unwrap();

        let issue = result.expect("expected Ok");
        assert_eq!(issue.source_id, "https://github.com/myorg/myrepo/issues/7");
    }

    #[tokio::test]
    async fn fetch_rest_api_provider_field_matches_provider_name() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // 404 variant
        Mock::given(method("GET"))
            .and(path("/repos/a/b/issues/404"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        // 401 variant
        Mock::given(method("GET"))
            .and(path("/repos/a/b/issues/401"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        // 429 variant
        Mock::given(method("GET"))
            .and(path("/repos/a/b/issues/429"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let base_url = server.uri();
        let base_url2 = base_url.clone();
        let base_url3 = base_url.clone();

        let r404 = tokio::task::spawn_blocking(move || {
            fetch_rest_api_with_base(&base_url, "a", "b", 404, "GitHub")
        })
        .await
        .unwrap();
        match r404 {
            Err(IssueSourceError::NotFound { provider, .. }) => assert_eq!(provider, "GitHub"),
            other => panic!("404: expected NotFound, got {other:?}"),
        }

        let r401 = tokio::task::spawn_blocking(move || {
            fetch_rest_api_with_base(&base_url2, "a", "b", 401, "GitHub")
        })
        .await
        .unwrap();
        match r401 {
            Err(IssueSourceError::Unauthorized { provider, .. }) => {
                assert_eq!(provider, "GitHub")
            }
            other => panic!("401: expected Unauthorized, got {other:?}"),
        }

        let r429 = tokio::task::spawn_blocking(move || {
            fetch_rest_api_with_base(&base_url3, "a", "b", 429, "GitHub")
        })
        .await
        .unwrap();
        match r429 {
            Err(IssueSourceError::RateLimited { provider }) => assert_eq!(provider, "GitHub"),
            other => panic!("429: expected RateLimited, got {other:?}"),
        }
    }

    // ── fetch_issue_with_progress tests ──────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn fetch_with_progress_emits_gh_command_message_on_success() {
        use crate::data::message::RecordingMessageSink;
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        // Set up a fake git repo with a GitHub remote
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/myorg/myrepo.git",
            ])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let script = tmp.path().join("gh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
echo '{"number":42,"title":"Test","body":"body","url":"https://github.com/myorg/myrepo/issues/42"}'
exit 0
"#,
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let mut sink = RecordingMessageSink::new();
        let result = fetch_issue_with_progress_using(
            script.to_str().unwrap(),
            "invalid-api-base",
            "42",
            tmp.path(),
            "GitHub",
            &mut sink,
        );

        assert!(result.is_ok());
        let messages = sink.all();
        assert!(
            !messages.is_empty(),
            "expected at least one progress message"
        );
        assert!(
            messages[0]
                .text
                .contains("gh issue view 42 --repo myorg/myrepo"),
            "first message should describe the gh command, got: {}",
            messages[0].text
        );
    }

    #[test]
    fn fetch_with_progress_emits_fallback_message_when_gh_fails() {
        use crate::data::message::RecordingMessageSink;

        let tmp = tempfile::tempdir().unwrap();

        // Use a full URL input so parse_input doesn't need a git remote.
        // Both integrations are deliberately invalid so the test exercises
        // the fallback messages without consulting the host's gh CLI or the
        // network.
        let mut sink = RecordingMessageSink::new();
        let missing_gh = tmp.path().join("missing-gh");
        let _ = fetch_issue_with_progress_using(
            missing_gh.to_str().unwrap(),
            "invalid-api-base",
            "https://github.com/nonexistent-test-org-xyz/nonexistent-repo-xyz/issues/99999",
            tmp.path(),
            "GitHub",
            &mut sink,
        );

        let messages = sink.all();
        assert!(
            messages.len() >= 2,
            "expected at least 2 messages (gh attempt + fallback), got {}: {:?}",
            messages.len(),
            messages.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        assert!(
            messages[0].text.contains("gh issue view 99999"),
            "first message should describe the gh command, got: {}",
            messages[0].text
        );
        assert!(
            messages[1].text.contains("falling back to REST API"),
            "second message should describe the REST fallback, got: {}",
            messages[1].text
        );
    }
}
