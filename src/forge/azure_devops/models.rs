use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};

use crate::error::{Result, TuicrError};
use crate::forge::remote_comments::{RemoteCommentSide, RemoteReviewComment, RemoteReviewThread};
use crate::forge::traits::{
    ForgeRepository, PullRequestCommit, PullRequestDetails, PullRequestSummary,
};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureIdentityRef {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub unique_name: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
}

impl AzureIdentityRef {
    fn display_identity(self) -> Option<String> {
        self.display_name
            .filter(|value| !value.is_empty())
            .or_else(|| self.unique_name.filter(|value| !value.is_empty()))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureProject {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub visibility: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureRepository {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub remote_url: Option<String>,
    #[serde(default)]
    pub ssh_url: Option<String>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub project: Option<AzureProject>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureCommitRef {
    #[serde(default)]
    pub commit_id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureCompletionOptions {
    #[serde(default)]
    pub delete_source_branch: Option<bool>,
    #[serde(default)]
    pub merge_commit_message: Option<String>,
    #[serde(default)]
    pub squash_merge: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AzureLink {
    #[serde(default)]
    pub href: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AzurePullRequestLinks {
    #[serde(default)]
    pub web: Option<AzureLink>,
}

/// Shared Azure CLI/REST pull-request shape. Nullable fields are represented
/// explicitly because Azure returns null for identities, descriptions, and
/// completion options in otherwise valid responses.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzurePullRequest {
    #[serde(default)]
    pub pull_request_id: u64,
    #[serde(default)]
    pub code_review_id: Option<u64>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "deserialize_enum_string")]
    pub status: String,
    #[serde(default)]
    pub is_draft: Option<bool>,
    #[serde(default)]
    pub created_by: Option<AzureIdentityRef>,
    #[serde(default)]
    pub creation_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub closed_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub source_ref_name: String,
    #[serde(default)]
    pub target_ref_name: String,
    #[serde(default)]
    pub last_merge_source_commit: Option<AzureCommitRef>,
    #[serde(default)]
    pub last_merge_target_commit: Option<AzureCommitRef>,
    #[serde(default)]
    pub last_merge_commit: Option<AzureCommitRef>,
    #[serde(default)]
    pub repository: Option<AzureRepository>,
    #[serde(default)]
    pub completion_options: Option<AzureCompletionOptions>,
    #[serde(default)]
    pub supports_iterations: Option<bool>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, rename = "_links")]
    pub links: Option<AzurePullRequestLinks>,
}

pub type AzurePullRequestSummary = AzurePullRequest;
pub type AzurePullRequestDetails = AzurePullRequest;

impl AzurePullRequest {
    pub fn into_summary(self, repository: &ForgeRepository) -> PullRequestSummary {
        let resolved_repository = self.resolved_repository(repository);
        let url = self.web_url();
        PullRequestSummary {
            repository: resolved_repository,
            number: self.pull_request_id,
            title: self.title,
            author: self.created_by.and_then(AzureIdentityRef::display_identity),
            head_ref_name: normalize_branch(&self.source_ref_name),
            base_ref_name: normalize_branch(&self.target_ref_name),
            updated_at: self.closed_date.or(self.creation_date),
            url,
            state: normalize_state(&self.status),
            is_draft: self.is_draft.unwrap_or(false),
        }
    }

    pub fn into_details(self, repository: &ForgeRepository) -> Result<PullRequestDetails> {
        let head_sha = required_commit(
            self.last_merge_source_commit.as_ref(),
            "lastMergeSourceCommit.commitId",
        )?;
        let base_sha = required_commit(
            self.last_merge_target_commit.as_ref(),
            "lastMergeTargetCommit.commitId",
        )?;
        let normalized_state = normalize_state(&self.status);
        let completed = self.status.eq_ignore_ascii_case("completed");
        let closed = completed || self.status.eq_ignore_ascii_case("abandoned");
        let resolved_repository = self.resolved_repository(repository);
        let url = self.web_url();
        let merged_at = if completed { self.closed_date } else { None };

        Ok(PullRequestDetails {
            repository: resolved_repository,
            number: self.pull_request_id,
            title: self.title,
            url,
            state: normalized_state,
            is_draft: self.is_draft.unwrap_or(false),
            author: self.created_by.and_then(AzureIdentityRef::display_identity),
            head_ref_name: normalize_branch(&self.source_ref_name),
            base_ref_name: normalize_branch(&self.target_ref_name),
            head_sha,
            base_sha,
            body: self.description.unwrap_or_default(),
            updated_at: self.closed_date.or(self.creation_date),
            closed,
            merged_at,
            diff_start_sha: None,
        })
    }

    pub fn resolved_repository(&self, repository: &ForgeRepository) -> ForgeRepository {
        self.repository
            .as_ref()
            .and_then(|value| value.id.as_deref())
            .filter(|id| !id.is_empty())
            .map_or_else(
                || repository.clone(),
                |id| repository.clone().with_repository_id(id),
            )
    }

    fn web_url(&self) -> String {
        if let Some(url) = self
            .links
            .as_ref()
            .and_then(|links| links.web.as_ref())
            .and_then(|link| link.href.as_ref())
            .filter(|url| !url.is_empty())
        {
            return url.clone();
        }
        if let Some(repository_url) = self.repository.as_ref().and_then(|repository| {
            repository
                .web_url
                .as_ref()
                .or(repository.remote_url.as_ref())
        }) {
            return format!(
                "{}/pullrequest/{}",
                repository_url.trim_end_matches('/'),
                self.pull_request_id
            );
        }
        self.url.clone().unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureGitCommitRef {
    #[serde(default)]
    pub commit_id: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub author: Option<AzureGitUserDate>,
    #[serde(default)]
    pub committer: Option<AzureGitUserDate>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub remote_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureGitUserDate {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub date: Option<DateTime<Utc>>,
}

impl AzureGitCommitRef {
    pub fn into_pull_request_commit(self) -> PullRequestCommit {
        let author = self.author.or(self.committer).unwrap_or_default();
        let author_name = author
            .name
            .filter(|name| !name.is_empty())
            .or(author.email)
            .unwrap_or_else(|| "unknown".to_string());
        PullRequestCommit {
            short_oid: self.commit_id.chars().take(7).collect(),
            oid: self.commit_id,
            summary: self.comment.lines().next().unwrap_or("").to_string(),
            author: author_name,
            timestamp: author.date,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureIteration {
    #[serde(default)]
    pub id: u32,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<AzureIdentityRef>,
    #[serde(default)]
    pub created_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub source_ref_commit: Option<AzureCommitRef>,
    #[serde(default)]
    pub target_ref_commit: Option<AzureCommitRef>,
    #[serde(default)]
    pub common_ref_commit: Option<AzureCommitRef>,
    #[serde(default)]
    pub has_more_commits: Option<bool>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureIterationChange {
    #[serde(default)]
    pub change_tracking_id: u32,
    #[serde(default)]
    pub change_id: Option<u32>,
    #[serde(default)]
    pub change_type: Option<String>,
    #[serde(default)]
    pub item: Option<AzureGitItem>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureIterationChanges {
    #[serde(default)]
    pub change_entries: Vec<AzureIterationChange>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureGitItem {
    #[serde(default)]
    pub object_id: Option<String>,
    #[serde(default)]
    pub original_object_id: Option<String>,
    #[serde(default)]
    pub git_object_type: Option<String>,
    #[serde(default)]
    pub commit_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub is_folder: Option<bool>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub content_metadata: Option<AzureGitItemContentMetadata>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureGitItemContentMetadata {
    #[serde(default)]
    pub encoding: Option<u32>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub extension: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureThread {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub published_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_updated_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub comments: Vec<AzureComment>,
    #[serde(default, deserialize_with = "deserialize_enum_string")]
    pub status: String,
    #[serde(default)]
    pub thread_context: Option<AzureThreadContext>,
    #[serde(default)]
    pub pull_request_thread_context: Option<AzurePullRequestThreadContext>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub properties: BTreeMap<String, AzurePropertyValue>,
}

impl AzureThread {
    pub fn into_review_thread(self) -> Option<RemoteReviewThread> {
        let context_is_outdated = property_is_true(&self.properties, "isoutdated");
        let expected_inline_context = self.pull_request_thread_context.is_some();
        let (path, start_line, line, side, invalid_position) = match self.thread_context.as_ref() {
            None => (
                String::new(),
                None,
                None,
                RemoteCommentSide::Right,
                expected_inline_context,
            ),
            Some(context) => match context.anchor() {
                Some((path, start, end, side)) => (path, start, Some(end), side, false),
                None => {
                    let path = context.normalized_path().unwrap_or_default();
                    let invalid = path.is_empty() || context.has_any_position();
                    (path, None, None, RemoteCommentSide::Right, invalid)
                }
            },
        };
        let comments = self
            .comments
            .into_iter()
            .filter(|comment| !comment.comment_type.eq_ignore_ascii_case("system"))
            .map(AzureComment::into_remote_comment)
            .collect::<Vec<_>>();
        if comments.is_empty() {
            return None;
        }

        Some(RemoteReviewThread {
            id: self.id.to_string(),
            path,
            line,
            start_line,
            side,
            is_resolved: is_resolved_status(&self.status),
            is_outdated: context_is_outdated || invalid_position,
            comments,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureComment {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub parent_comment_id: u64,
    #[serde(default)]
    pub author: Option<AzureIdentityRef>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub published_date: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_updated_date: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "deserialize_enum_string")]
    pub comment_type: String,
    #[serde(default)]
    pub is_deleted: bool,
    #[serde(default)]
    pub url: Option<String>,
}

impl AzureComment {
    fn into_remote_comment(self) -> RemoteReviewComment {
        RemoteReviewComment {
            id: self.id.to_string(),
            author: self.author.and_then(AzureIdentityRef::display_identity),
            body: if self.is_deleted {
                "[deleted Azure DevOps comment]".to_string()
            } else {
                self.content.unwrap_or_default()
            },
            created_at: self.published_date,
            in_reply_to: (self.parent_comment_id > 0).then(|| self.parent_comment_id.to_string()),
            url: self.url.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureThreadContext {
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default)]
    pub right_file_start: Option<AzureCommentPosition>,
    #[serde(default)]
    pub right_file_end: Option<AzureCommentPosition>,
    #[serde(default)]
    pub left_file_start: Option<AzureCommentPosition>,
    #[serde(default)]
    pub left_file_end: Option<AzureCommentPosition>,
}

impl AzureThreadContext {
    fn has_any_position(&self) -> bool {
        self.right_file_start.is_some()
            || self.right_file_end.is_some()
            || self.left_file_start.is_some()
            || self.left_file_end.is_some()
    }
    fn normalized_path(&self) -> Option<String> {
        self.file_path
            .as_deref()
            .map(|path| path.trim_start_matches('/'))
            .filter(|path| !path.is_empty())
            .map(str::to_string)
    }

    fn anchor(&self) -> Option<(String, Option<u32>, u32, RemoteCommentSide)> {
        let path = self.normalized_path()?;
        let right = side_span(self.right_file_start.as_ref(), self.right_file_end.as_ref());
        let left = side_span(self.left_file_start.as_ref(), self.left_file_end.as_ref());
        let (start, line, side) = match (right, left) {
            (
                Some((right_score, right_start, right_line)),
                Some((left_score, left_start, left_line)),
            ) => {
                if left_score > right_score {
                    (left_start, left_line, RemoteCommentSide::Left)
                } else {
                    (right_start, right_line, RemoteCommentSide::Right)
                }
            }
            (Some((_, start, line)), None) => (start, line, RemoteCommentSide::Right),
            (None, Some((_, start, line))) => (start, line, RemoteCommentSide::Left),
            (None, None) => return None,
        };
        Some((path, (start != line).then_some(start), line, side))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AzureCommentPosition {
    #[serde(default)]
    pub line: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzurePullRequestThreadContext {
    #[serde(default)]
    pub change_tracking_id: Option<u32>,
    #[serde(default)]
    pub iteration_context: Option<AzureCommentIterationContext>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureCommentIterationContext {
    #[serde(default)]
    pub first_comparing_iteration: Option<u32>,
    #[serde(default)]
    pub second_comparing_iteration: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AzurePropertyValue {
    #[serde(default, rename = "$type")]
    pub value_type: Option<String>,
    #[serde(default, rename = "$value")]
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureResourceArea {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub location_url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureApiResourceLocation {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub area: String,
    #[serde(default)]
    pub resource_name: String,
    #[serde(default)]
    pub route_template: String,
    #[serde(default)]
    pub min_version: Option<String>,
    #[serde(default)]
    pub max_version: Option<String>,
    #[serde(default)]
    pub released_version: Option<String>,
    #[serde(default)]
    pub resource_version: Option<u32>,
}

pub fn parse_azure_pull_requests(json: &str) -> Result<Vec<AzurePullRequest>> {
    parse_array_or_envelope(json, "pull requests")
}

pub fn parse_azure_commits(json: &str) -> Result<Vec<AzureGitCommitRef>> {
    parse_array_or_envelope(json, "commits")
}

pub fn parse_azure_iterations(json: &str) -> Result<Vec<AzureIteration>> {
    parse_array_or_envelope(json, "iterations")
}

pub fn parse_azure_threads(json: &str) -> Result<Vec<AzureThread>> {
    parse_array_or_envelope(json, "review threads")
}

pub fn parse_azure_resource_areas(json: &str) -> Result<Vec<AzureResourceArea>> {
    parse_array_or_envelope(json, "resource areas")
}

pub fn parse_azure_api_resources(json: &str) -> Result<Vec<AzureApiResourceLocation>> {
    parse_array_or_envelope(json, "API resources")
}

pub fn parse_azure_iteration_changes(json: &str) -> Result<Vec<AzureIterationChange>> {
    let value: serde_json::Value = serde_json::from_str(json)?;
    if let Some(entries) = value.get("changeEntries") {
        return Ok(serde_json::from_value(entries.clone())?);
    }
    parse_value_as_array_or_envelope(value, "iteration changes")
}

fn parse_array_or_envelope<T>(json: &str, resource: &str) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    let value: serde_json::Value = serde_json::from_str(json)?;
    parse_value_as_array_or_envelope(value, resource)
}

fn parse_value_as_array_or_envelope<T>(value: serde_json::Value, resource: &str) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    let entries = if value.is_array() {
        value
    } else {
        value.get("value").cloned().ok_or_else(|| {
            TuicrError::Forge(format!(
                "Azure DevOps {resource} response was neither an array nor a `{{count,value}}` envelope"
            ))
        })?
    };
    if !entries.is_array() {
        return Err(TuicrError::Forge(format!(
            "Azure DevOps {resource} response field `value` was not an array"
        )));
    }
    Ok(serde_json::from_value(entries)?)
}

fn required_commit(commit: Option<&AzureCommitRef>, field: &str) -> Result<String> {
    commit
        .and_then(|value| value.commit_id.as_ref())
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| {
            TuicrError::Forge(format!(
                "Azure DevOps pull request response is missing required `{field}`; reload the PR and verify it has a source and target commit"
            ))
        })
}

fn normalize_branch(branch: &str) -> String {
    branch
        .strip_prefix("refs/heads/")
        .unwrap_or(branch)
        .to_string()
}

fn normalize_state(state: &str) -> String {
    match state.to_ascii_lowercase().as_str() {
        "active" => "OPEN".to_string(),
        "completed" => "MERGED".to_string(),
        "abandoned" => "CLOSED".to_string(),
        other => other.to_ascii_uppercase(),
    }
}

fn side_span(
    start: Option<&AzureCommentPosition>,
    end: Option<&AzureCommentPosition>,
) -> Option<(u8, u32, u32)> {
    let start_line = start.and_then(valid_line);
    let end_line = end.and_then(valid_line);
    match (start_line, end_line) {
        (Some(start), Some(end)) => Some((2, start.min(end), start.max(end))),
        (Some(line), None) | (None, Some(line)) => Some((1, line, line)),
        (None, None) => None,
    }
}

fn valid_line(position: &AzureCommentPosition) -> Option<u32> {
    u32::try_from(position.line?).ok().filter(|line| *line > 0)
}

fn is_resolved_status(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "fixed" | "wontfix" | "closed" | "bydesign"
    )
}

fn property_is_true(properties: &BTreeMap<String, AzurePropertyValue>, suffix: &str) -> bool {
    properties.iter().any(|(key, property)| {
        key.to_ascii_lowercase().ends_with(suffix)
            && match &property.value {
                serde_json::Value::Bool(value) => *value,
                serde_json::Value::Number(value) => value.as_u64().is_some_and(|value| value != 0),
                serde_json::Value::String(value) => {
                    value == "1" || value.eq_ignore_ascii_case("true")
                }
                _ => false,
            }
    })
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

fn deserialize_enum_string<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?.unwrap_or_default();
    Ok(match value {
        serde_json::Value::String(value) => value,
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> ForgeRepository {
        ForgeRepository::azure_devops(
            "dev.azure.com",
            "acme",
            "https://dev.azure.com/acme",
            "widgets",
            "service",
        )
    }

    #[test]
    fn parses_cli_arrays_and_rest_envelopes() {
        let item = r#"{"pullRequestId":7,"title":"Change"}"#;
        let raw = parse_azure_pull_requests(&format!("[{item}]")).unwrap();
        let envelope =
            parse_azure_pull_requests(&format!(r#"{{"count":1,"value":[{item}]}}"#)).unwrap();
        assert_eq!(raw[0].pull_request_id, 7);
        assert_eq!(envelope[0].title, "Change");
    }

    #[test]
    fn parses_checked_in_pr_commit_and_thread_fixtures() {
        let details: AzurePullRequest = serde_json::from_str(include_str!(
            "../../../tests/fixtures/azure_devops/pr_details.json"
        ))
        .unwrap();
        assert_eq!(details.pull_request_id, 125);
        assert_eq!(
            parse_azure_pull_requests(include_str!(
                "../../../tests/fixtures/azure_devops/pr_list.json"
            ))
            .unwrap()
            .len(),
            1
        );
        assert_eq!(
            parse_azure_commits(include_str!(
                "../../../tests/fixtures/azure_devops/commits.json"
            ))
            .unwrap()
            .len(),
            1
        );
        let threads = parse_azure_threads(include_str!(
            "../../../tests/fixtures/azure_devops/threads.json"
        ))
        .unwrap()
        .into_iter()
        .filter_map(AzureThread::into_review_thread)
        .collect::<Vec<_>>();
        assert_eq!(threads.len(), 4);
        assert_eq!(threads[0].start_line, Some(10));
        assert_eq!(threads[0].line, Some(12));
        assert!(threads[1].is_resolved);
        assert_eq!(threads[2].line, None);
        assert_eq!(
            threads[3].comments[0].body,
            "[deleted Azure DevOps comment]"
        );
        assert_eq!(
            parse_azure_iteration_changes(include_str!(
                "../../../tests/fixtures/azure_devops/iteration_changes.json"
            ))
            .unwrap()[0]
                .change_tracking_id,
            7
        );
        let created: AzureThread = serde_json::from_str(include_str!(
            "../../../tests/fixtures/azure_devops/thread_created.json"
        ))
        .unwrap();
        assert_eq!(created.id, 70);
    }

    #[test]
    fn converts_nullable_pull_request_and_resolves_repository_id() {
        let json = r#"{
            "pullRequestId": 12,
            "title": "Safe nulls",
            "description": null,
            "status": "completed",
            "isDraft": null,
            "createdBy": null,
            "closedDate": "2026-07-20T10:00:00Z",
            "sourceRefName": "refs/heads/feature",
            "targetRefName": "refs/heads/main",
            "lastMergeSourceCommit": {"commitId":"head123"},
            "lastMergeTargetCommit": {"commitId":"base123"},
            "repository": {
                "id": "repo-guid",
                "webUrl": "https://dev.azure.com/acme/widgets/_git/service"
            },
            "completionOptions": {"deleteSourceBranch": null},
            "unknownFutureField": true
        }"#;
        let pull_request: AzurePullRequest = serde_json::from_str(json).unwrap();
        assert_eq!(
            pull_request
                .completion_options
                .as_ref()
                .unwrap()
                .delete_source_branch,
            None
        );
        let details = pull_request.into_details(&repository()).unwrap();
        assert_eq!(details.head_ref_name, "feature");
        assert_eq!(details.base_ref_name, "main");
        assert_eq!(details.head_sha, "head123");
        assert_eq!(details.base_sha, "base123");
        assert_eq!(details.body, "");
        assert_eq!(details.state, "MERGED");
        assert!(details.closed);
        assert!(details.merged_at.is_some());
        assert_eq!(
            details
                .repository
                .azure_devops_identity()
                .unwrap()
                .repository_id
                .as_deref(),
            Some("repo-guid")
        );
        assert_eq!(
            details.url,
            "https://dev.azure.com/acme/widgets/_git/service/pullrequest/12"
        );
    }

    #[test]
    fn keeps_active_draft_pull_requests_reviewable() {
        let json = r#"{
            "pullRequestId": 13,
            "title": "Work in progress",
            "status": "active",
            "isDraft": true,
            "sourceRefName": "refs/heads/feature",
            "targetRefName": "refs/heads/main",
            "lastMergeSourceCommit": {"commitId":"head123"},
            "lastMergeTargetCommit": {"commitId":"base123"}
        }"#;
        let pull_request: AzurePullRequest = serde_json::from_str(json).unwrap();

        let summary = pull_request.clone().into_summary(&repository());
        let details = pull_request.into_details(&repository()).unwrap();

        assert!(summary.is_draft);
        assert!(details.is_draft);
        assert!(!details.is_read_only());
    }

    #[test]
    fn reports_missing_merge_shas_with_field_names() {
        let pull_request = AzurePullRequest {
            pull_request_id: 1,
            last_merge_target_commit: Some(AzureCommitRef {
                commit_id: Some("base".to_string()),
                ..AzureCommitRef::default()
            }),
            ..AzurePullRequest::default()
        };
        let error = pull_request.into_details(&repository()).unwrap_err();
        assert!(error.to_string().contains("lastMergeSourceCommit.commitId"));
    }

    #[test]
    fn converts_commit_and_iteration_tracking_models() {
        let commits = parse_azure_commits(
            r#"{"count":1,"value":[{"commitId":"123456789","comment":"Subject\nBody","author":{"name":"Alice","date":"2026-01-01T00:00:00Z"}}]}"#,
        )
        .unwrap();
        let commit = commits
            .into_iter()
            .next()
            .unwrap()
            .into_pull_request_commit();
        assert_eq!(commit.short_oid, "1234567");
        assert_eq!(commit.summary, "Subject");
        assert_eq!(commit.author, "Alice");

        let changes = parse_azure_iteration_changes(
            r#"{"changeEntries":[{"changeTrackingId":23,"changeType":"edit","item":{"path":"/src/lib.rs"}}]}"#,
        )
        .unwrap();
        assert_eq!(changes[0].change_tracking_id, 23);
        assert_eq!(
            changes[0].item.as_ref().unwrap().path.as_deref(),
            Some("/src/lib.rs")
        );
    }

    #[test]
    fn converts_right_and_left_range_anchors_using_end_line() {
        let right: AzureThread = serde_json::from_str(
            r#"{"id":1,"status":"active","comments":[{"id":10,"content":"right","commentType":"text"}],"threadContext":{"filePath":"/src/lib.rs","rightFileStart":{"line":4},"rightFileEnd":{"line":8}}}"#,
        )
        .unwrap();
        let right = right.into_review_thread().unwrap();
        assert_eq!(right.path, "src/lib.rs");
        assert_eq!(right.start_line, Some(4));
        assert_eq!(right.line, Some(8));
        assert_eq!(right.side, RemoteCommentSide::Right);

        let left: AzureThread = serde_json::from_str(
            r#"{"id":2,"status":"active","comments":[{"id":11,"content":"left","commentType":"text"}],"threadContext":{"filePath":"/src/lib.rs","leftFileStart":{"line":7},"leftFileEnd":{"line":9}}}"#,
        )
        .unwrap();
        let left = left.into_review_thread().unwrap();
        assert_eq!(left.start_line, Some(7));
        assert_eq!(left.line, Some(9));
        assert_eq!(left.side, RemoteCommentSide::Left);
    }

    #[test]
    fn chooses_the_side_with_a_real_span_in_mixed_contexts() {
        let thread: AzureThread = serde_json::from_str(
            r#"{"id":3,"comments":[{"id":12,"content":"mixed","commentType":"text"}],"threadContext":{"filePath":"/old.rs","rightFileStart":{"line":0},"rightFileEnd":{"line":0},"leftFileStart":{"line":20},"leftFileEnd":{"line":22}}}"#,
        )
        .unwrap();
        let thread = thread.into_review_thread().unwrap();
        assert_eq!(thread.line, Some(22));
        assert_eq!(thread.side, RemoteCommentSide::Left);
    }

    #[test]
    fn preserves_pr_level_deleted_comments_and_reply_parents() {
        let thread: AzureThread = serde_json::from_str(
            r#"{
                "id":4,
                "status":"byDesign",
                "comments":[
                    {"id":1,"content":"automation","commentType":"system"},
                    {"id":2,"content":null,"commentType":"text","isDeleted":true},
                    {"id":3,"parentCommentId":2,"content":"reply","commentType":"codeChange"}
                ]
            }"#,
        )
        .unwrap();
        let thread = thread.into_review_thread().unwrap();
        assert_eq!(thread.path, "");
        assert_eq!(thread.line, None);
        assert!(!thread.is_outdated);
        assert!(thread.is_resolved);
        assert_eq!(thread.comments.len(), 2);
        assert_eq!(thread.comments[0].body, "[deleted Azure DevOps comment]");
        assert_eq!(thread.comments[1].in_reply_to.as_deref(), Some("2"));
    }

    #[test]
    fn marks_malformed_inline_positions_outdated() {
        let thread: AzureThread = serde_json::from_str(
            r#"{"id":5,"comments":[{"id":1,"content":"lost","commentType":"text"}],"threadContext":{"filePath":"/src/lib.rs","rightFileEnd":{"line":0}}}"#,
        )
        .unwrap();
        let thread = thread.into_review_thread().unwrap();
        assert_eq!(thread.path, "src/lib.rs");
        assert_eq!(thread.line, None);
        assert!(thread.is_outdated);
    }

    #[test]
    fn preserves_file_level_context_without_marking_it_outdated() {
        let thread: AzureThread = serde_json::from_str(
            r#"{"id":6,"comments":[{"id":1,"content":"file","commentType":"text"}],"threadContext":{"filePath":"/src/lib.rs"}}"#,
        )
        .unwrap();
        let thread = thread.into_review_thread().unwrap();
        assert_eq!(thread.path, "src/lib.rs");
        assert_eq!(thread.line, None);
        assert!(!thread.is_outdated);
    }

    #[test]
    fn parses_resource_discovery_envelopes() {
        let areas = parse_azure_resource_areas(
            r#"{"count":1,"value":[{"id":"git-area","name":"git","locationUrl":"https://dev.azure.com/acme/"}]}"#,
        )
        .unwrap();
        assert_eq!(areas[0].id, "git-area");
        let resources = parse_azure_api_resources(
            r#"[{"id":"pulls","area":"git","resourceName":"pullRequests","routeTemplate":"{project}/_apis/git/repositories/{repositoryId}/pullRequests/{pullRequestId}","resourceVersion":1}]"#,
        )
        .unwrap();
        assert_eq!(resources[0].resource_name, "pullRequests");
    }
}
