pub mod az;
pub mod models;
pub mod remote;
mod submit;

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Result, TuicrError};
use crate::forge::remote_comments::RemoteReviewThread;
use crate::forge::traits::{
    CreateReviewRequest, ForgeBackend, ForgeFileLinesRequest, ForgeRepository,
    GhCreateReviewResponse, PagedPullRequests, PullRequestCommit, PullRequestDetails,
    PullRequestListQuery, PullRequestListScope, PullRequestReviewMetadata, PullRequestReviewRecord,
    PullRequestTarget, ReviewSubmissionOutcome,
};
use crate::model::DiffLine;
use crate::process::run_command_output;
use crate::vcs::slice_context_lines;

use self::az::{AzCommandRunner, SystemAzRunner, map_az_error};
use self::models::{
    AzureApiResourceLocation, AzureGitItem, AzureIteration, AzureIterationChange, AzurePullRequest,
    parse_azure_api_resources, parse_azure_commits, parse_azure_iteration_changes,
    parse_azure_iterations, parse_azure_pull_requests, parse_azure_threads,
};
use self::remote::percent_encode_path_component;

const API_VERSION: &str = "7.1";
const COMMITS_ROUTE: &str =
    "{project}/_apis/git/repositories/{repositoryId}/pullRequests/{pullRequestId}/commits";
const ITERATIONS_ROUTE: &str =
    "{project}/_apis/git/repositories/{repositoryId}/pullRequests/{pullRequestId}/iterations";
const ITERATION_CHANGES_ROUTE: &str = "{project}/_apis/git/repositories/{repositoryId}/pullRequests/{pullRequestId}/iterations/{iterationId}/changes";
const THREADS_ROUTE: &str =
    "{project}/_apis/git/repositories/{repositoryId}/pullRequests/{pullRequestId}/threads";
const REVIEWERS_ROUTE: &str =
    "{project}/_apis/git/repositories/{repositoryId}/pullRequests/{pullRequestId}/reviewers";
const ITEMS_ROUTE: &str = "{project}/_apis/git/repositories/{repositoryId}/items";

#[derive(Debug, Clone)]
pub struct AzureDevOpsBackend<R = SystemAzRunner> {
    default_repository: Option<ForgeRepository>,
    runner: R,
    local_checkout: Option<PathBuf>,
}

impl AzureDevOpsBackend<SystemAzRunner> {
    pub fn new(default_repository: Option<ForgeRepository>) -> Self {
        Self {
            default_repository,
            runner: SystemAzRunner,
            local_checkout: None,
        }
    }
}

impl<R> AzureDevOpsBackend<R>
where
    R: AzCommandRunner,
{
    pub fn with_runner(default_repository: Option<ForgeRepository>, runner: R) -> Self {
        Self {
            default_repository,
            runner,
            local_checkout: None,
        }
    }

    pub fn with_local_checkout(mut self, checkout: Option<PathBuf>) -> Self {
        self.local_checkout = checkout;
        self
    }

    fn run_az(&self, args: Vec<String>) -> Result<String> {
        self.runner.run(&args).map_err(map_az_error)
    }

    pub(crate) fn run_az_with_input(&self, args: Vec<String>, input: &[u8]) -> Result<String> {
        self.runner
            .run_with_input_file(&args, input)
            .map_err(map_az_error)
    }

    fn resolve_repository(&self, target: &PullRequestTarget) -> Result<ForgeRepository> {
        target
            .repository
            .clone()
            .or_else(|| self.default_repository.clone())
            .ok_or_else(|| {
                TuicrError::Forge(format!(
                    "Azure DevOps pull request target `{}` does not include a repository",
                    target.original
                ))
            })
    }

    fn identity(
        repository: &ForgeRepository,
    ) -> Result<&crate::forge::traits::AzureDevOpsRepositoryIdentity> {
        repository.azure_devops_identity().ok_or_else(|| {
            TuicrError::Forge(format!(
                "repository `{}` is not an Azure DevOps repository",
                repository.display_name()
            ))
        })
    }

    fn repository_id(repository: &ForgeRepository) -> Result<&str> {
        Self::identity(repository)?
            .repository_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                TuicrError::Forge(format!(
                    "Azure DevOps repository ID is unresolved for `{}`; reload the pull request",
                    repository.display_name()
                ))
            })
    }

    fn current_identity(&self) -> Result<String> {
        let output = self
            .run_az(vec!["account".to_string(), "show".to_string()])
            .map_err(|error| {
                TuicrError::Forge(format!(
                    "Azure DevOps review-request filtering could not resolve the current identity with `az account show`: {error}\nRun `az login`, or use the all-open filter. PAT users may continue using `az devops login` for normal Azure DevOps operations."
                ))
            })?;
        let value: serde_json::Value = serde_json::from_str(&output)?;
        value
            .pointer("/user/name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                TuicrError::Forge(
                    "Azure DevOps review-request filtering needs the current identity from `az account show`. Run `az login`, or use the all-open filter. PAT users may continue using `az devops login` for normal Azure DevOps operations."
                        .to_string(),
                )
            })
    }

    fn discover_resource(
        &self,
        repository: &ForgeRepository,
        expected_route: &str,
    ) -> Result<AzureApiResourceLocation> {
        let identity = Self::identity(repository)?;
        let output = self.run_az(vec![
            "devops".to_string(),
            "invoke".to_string(),
            "--organization".to_string(),
            identity.organization_url.clone(),
            "--query".to_string(),
            "[?area=='git']".to_string(),
        ])?;
        let expected = normalize_route(expected_route);
        parse_azure_api_resources(&output)?
            .into_iter()
            .find(|location| {
                location.area.eq_ignore_ascii_case("git")
                    && route_matches(&location.route_template, &expected)
                    && !location.resource_name.trim().is_empty()
            })
            .ok_or_else(|| {
                TuicrError::Forge(format!(
                    "Azure DevOps Git API discovery returned no exact route for `{expected_route}`"
                ))
            })
    }

    pub(crate) fn invoke(
        &self,
        repository: &ForgeRepository,
        expected_route: &str,
        method: Option<&str>,
        route_parameters: &[(&str, String)],
        query_parameters: &[(&str, String)],
        input: Option<&[u8]>,
    ) -> Result<String> {
        let identity = Self::identity(repository)?;
        let resource = self.discover_resource(repository, expected_route)?;
        if !route_matches(&resource.route_template, &normalize_route(expected_route)) {
            return Err(TuicrError::Forge(format!(
                "Azure DevOps discovered resource `{}` with an unexpected route `{}`",
                resource.resource_name, resource.route_template
            )));
        }

        let mut args = vec![
            "devops".to_string(),
            "invoke".to_string(),
            "--organization".to_string(),
            identity.organization_url.clone(),
            "--area".to_string(),
            "git".to_string(),
            "--resource".to_string(),
            resource.resource_name,
            "--api-version".to_string(),
            API_VERSION.to_string(),
        ];
        if let Some(method) = method {
            args.extend(["--http-method".to_string(), method.to_string()]);
        }
        if !route_parameters.is_empty() {
            args.push("--route-parameters".to_string());
            args.extend(
                route_parameters
                    .iter()
                    .map(|(key, value)| format!("{key}={value}")),
            );
        }
        if !query_parameters.is_empty() {
            args.push("--query-parameters".to_string());
            args.extend(
                query_parameters
                    .iter()
                    .map(|(key, value)| format!("{key}={value}")),
            );
        }
        match input {
            Some(input) => self.run_az_with_input(args, input),
            None => self.run_az(args),
        }
    }

    fn pull_request_route_parameters(
        repository: &ForgeRepository,
        pull_request_id: u64,
    ) -> Result<Vec<(&'static str, String)>> {
        let identity = Self::identity(repository)?;
        Ok(vec![
            ("project", identity.project.clone()),
            ("repositoryId", Self::repository_id(repository)?.to_string()),
            ("pullRequestId", pull_request_id.to_string()),
        ])
    }

    pub(crate) fn list_iterations(&self, pr: &PullRequestDetails) -> Result<Vec<AzureIteration>> {
        let route = Self::pull_request_route_parameters(&pr.repository, pr.number)?;
        let output = self.invoke(&pr.repository, ITERATIONS_ROUTE, None, &route, &[], None)?;
        parse_azure_iterations(&output)
    }

    pub(crate) fn latest_iteration(&self, pr: &PullRequestDetails) -> Result<AzureIteration> {
        self.list_iterations(pr)?
            .into_iter()
            .max_by_key(|iteration| iteration.id)
            .ok_or_else(|| {
                TuicrError::Forge(format!(
                    "Azure DevOps pull request {} has no iterations",
                    pr.number
                ))
            })
    }

    pub(crate) fn list_iteration_changes(
        &self,
        pr: &PullRequestDetails,
        iteration_id: u32,
    ) -> Result<Vec<AzureIterationChange>> {
        let mut route = Self::pull_request_route_parameters(&pr.repository, pr.number)?;
        route.push(("iterationId", iteration_id.to_string()));
        let output = self.invoke(
            &pr.repository,
            ITERATION_CHANGES_ROUTE,
            None,
            &route,
            &[
                ("$compareTo", "0".to_string()),
                ("$top", "2000".to_string()),
            ],
            None,
        )?;
        let changes = parse_azure_iteration_changes(&output)?;
        if changes.len() == 2000 {
            return Err(TuicrError::Forge(
                "Azure DevOps returned the iteration-change safety limit (2000); refusing to submit with a potentially incomplete changeTrackingId map"
                    .to_string(),
            ));
        }
        Ok(changes)
    }

    fn fetch_item(&self, request: &ForgeFileLinesRequest) -> Result<String> {
        let identity = Self::identity(&request.repository)?;
        let route = vec![
            ("project", identity.project.clone()),
            (
                "repositoryId",
                Self::repository_id(&request.repository)?.to_string(),
            ),
        ];
        let path = slash_path(request.path.as_path());
        let output = self.invoke(
            &request.repository,
            ITEMS_ROUTE,
            None,
            &route,
            &[
                ("path", format!("/{path}")),
                ("versionDescriptor.version", request.sha().to_string()),
                ("versionDescriptor.versionType", "commit".to_string()),
                ("includeContent", "true".to_string()),
            ],
            None,
        )?;
        let item: AzureGitItem = serde_json::from_str(&output)?;
        item.content.ok_or_else(|| {
            TuicrError::Forge(format!(
                "Azure DevOps Items response omitted content for `{path}` at `{}` (binary and oversized files cannot be expanded)",
                request.sha()
            ))
        })
    }

    fn local_content(&self, request: &ForgeFileLinesRequest) -> Option<String> {
        let root = self.local_checkout.as_deref()?;
        crate::forge::local_checkout_for_repo(root, &request.repository)?;
        let spec = format!("{}:{}", request.sha(), slash_path(request.path.as_path()));
        run_git(root, &["cat-file", "blob", &spec]).ok()
    }

    fn immutable_diff(
        &self,
        pr: &PullRequestDetails,
        start_sha: &str,
        end_sha: &str,
    ) -> Result<String> {
        let root = self
            .local_checkout
            .as_deref()
            .ok_or_else(rest_diff_limitation)?;
        if crate::forge::local_checkout_for_repo(root, &pr.repository).is_none() {
            return Err(rest_diff_limitation());
        }
        let remote_url = azure_remote_url(&pr.repository)?;
        for sha in [start_sha, end_sha] {
            let commit = format!("{sha}^{{commit}}");
            if run_git(root, &["cat-file", "-e", &commit]).is_err() {
                run_git(
                    root,
                    &[
                        "fetch",
                        "--no-tags",
                        "--no-write-fetch-head",
                        &remote_url,
                        sha,
                    ],
                )
                .map_err(|error| {
                    TuicrError::Forge(format!(
                        "Azure DevOps immutable fetch of `{sha}` failed: {error}. {}",
                        rest_diff_limitation()
                    ))
                })?;
                run_git(root, &["cat-file", "-e", &commit]).map_err(|error| {
                    TuicrError::Forge(format!(
                        "Azure DevOps fetch completed but commit `{sha}` is still unavailable: {error}"
                    ))
                })?;
            }
        }
        run_git(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--binary",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                start_sha,
                end_sha,
            ],
        )
        .map_err(|error| TuicrError::Forge(format!("Azure DevOps local Git diff failed: {error}")))
    }

    pub(crate) fn refetch_pull_request(
        &self,
        pr: &PullRequestDetails,
    ) -> Result<PullRequestDetails> {
        self.get_pull_request(PullRequestTarget::with_repository(
            pr.repository.clone(),
            pr.number,
            pr.url.clone(),
        ))
    }
}

impl<R> ForgeBackend for AzureDevOpsBackend<R>
where
    R: AzCommandRunner,
{
    fn list_pull_requests(&self, query: PullRequestListQuery) -> Result<PagedPullRequests> {
        let identity = Self::identity(&query.repository)?;
        let page_size = query.page_size.max(1);
        let repository = identity
            .repository_id
            .as_deref()
            .unwrap_or(&query.repository.name);
        let mut args = vec![
            "repos".to_string(),
            "pr".to_string(),
            "list".to_string(),
            "--organization".to_string(),
            identity.organization_url.clone(),
            "--project".to_string(),
            identity.project.clone(),
            "--repository".to_string(),
            repository.to_string(),
            "--status".to_string(),
            "active".to_string(),
            "--skip".to_string(),
            query.already_loaded.to_string(),
            "--top".to_string(),
            (page_size + 1).to_string(),
        ];
        if query.scope == PullRequestListScope::ReviewRequested {
            args.extend(["--reviewer".to_string(), self.current_identity()?]);
        }
        let rows = parse_azure_pull_requests(&self.run_az(args)?)?;
        let has_more = rows.len() > page_size;
        let pull_requests = rows
            .into_iter()
            .take(page_size)
            .map(|row| row.into_summary(&query.repository))
            .collect::<Vec<_>>();
        Ok(PagedPullRequests {
            total_loaded: query.already_loaded + pull_requests.len(),
            pull_requests,
            has_more,
        })
    }

    fn get_pull_request(&self, target: PullRequestTarget) -> Result<PullRequestDetails> {
        let repository = self.resolve_repository(&target)?;
        let identity = Self::identity(&repository)?;
        let output = self.run_az(vec![
            "repos".to_string(),
            "pr".to_string(),
            "show".to_string(),
            "--id".to_string(),
            target.number.to_string(),
            "--organization".to_string(),
            identity.organization_url.clone(),
        ])?;
        let pull_request: AzurePullRequest = serde_json::from_str(&output)?;
        if pull_request.pull_request_id != 0 && pull_request.pull_request_id != target.number {
            return Err(TuicrError::Forge(format!(
                "Azure DevOps returned pull request {} while {} was requested",
                pull_request.pull_request_id, target.number
            )));
        }
        if let Some(returned_repository) = pull_request.repository.as_ref() {
            if let Some(name) = returned_repository.name.as_deref()
                && !name.eq_ignore_ascii_case(&repository.name)
            {
                return Err(TuicrError::Forge(format!(
                    "Azure DevOps pull request {} belongs to repository `{name}`, not `{}`",
                    target.number, repository.name
                )));
            }
            if let Some(project) = returned_repository
                .project
                .as_ref()
                .and_then(|project| project.name.as_deref())
                && !project.eq_ignore_ascii_case(&identity.project)
            {
                return Err(TuicrError::Forge(format!(
                    "Azure DevOps pull request {} belongs to project `{project}`, not `{}`",
                    target.number, identity.project
                )));
            }
        }
        pull_request.into_details(&repository)
    }

    fn get_pull_request_diff(&self, pr: &PullRequestDetails) -> Result<String> {
        self.immutable_diff(pr, &pr.base_sha, &pr.head_sha)
    }

    fn fetch_file_lines(&self, request: ForgeFileLinesRequest) -> Result<Vec<DiffLine>> {
        if request.start_line == 0 || request.start_line > request.end_line {
            return Ok(Vec::new());
        }
        let content = self
            .local_content(&request)
            .map(Ok)
            .unwrap_or_else(|| self.fetch_item(&request))?;
        Ok(slice_context_lines(
            &content,
            request.start_line,
            request.end_line,
        ))
    }

    fn file_line_count(&self, request: ForgeFileLinesRequest) -> Result<u32> {
        let content = self
            .local_content(&request)
            .map(Ok)
            .unwrap_or_else(|| self.fetch_item(&request))?;
        u32::try_from(content.lines().count()).map_err(|_| {
            TuicrError::Forge("Azure DevOps file has more than u32::MAX lines".to_string())
        })
    }

    fn list_review_threads(&self, pr: &PullRequestDetails) -> Result<Vec<RemoteReviewThread>> {
        let route = Self::pull_request_route_parameters(&pr.repository, pr.number)?;
        let output = self.invoke(&pr.repository, THREADS_ROUTE, None, &route, &[], None)?;
        Ok(parse_azure_threads(&output)?
            .into_iter()
            .filter_map(|thread| thread.into_review_thread())
            .collect())
    }

    fn list_pull_request_commits(&self, pr: &PullRequestDetails) -> Result<Vec<PullRequestCommit>> {
        let route = Self::pull_request_route_parameters(&pr.repository, pr.number)?;
        let output = self.invoke(
            &pr.repository,
            COMMITS_ROUTE,
            None,
            &route,
            &[("$top", "1000".to_string())],
            None,
        )?;
        let rows = parse_azure_commits(&output)?;
        if rows.len() == 1000 {
            return Err(TuicrError::Forge(
                "Azure DevOps returned the PR-commit safety limit (1000); `az devops invoke` does not expose the continuation header, so refusing to return a truncated commit list"
                    .to_string(),
            ));
        }
        let mut commits = rows
            .into_iter()
            .map(|commit| commit.into_pull_request_commit())
            .collect::<Vec<_>>();
        if commits.iter().all(|commit| commit.timestamp.is_some()) {
            commits.sort_by_key(|commit| commit.timestamp);
        }
        Ok(commits)
    }

    fn list_pull_request_review_metadata(
        &self,
        pr: &PullRequestDetails,
    ) -> Result<PullRequestReviewMetadata> {
        let viewer_login = self.current_identity().ok();
        let reviews = (|| -> Result<Vec<PullRequestReviewRecord>> {
            let route = Self::pull_request_route_parameters(&pr.repository, pr.number)?;
            let output = self.invoke(&pr.repository, REVIEWERS_ROUTE, None, &route, &[], None)?;
            let reviewers: Vec<AzureReviewer> = parse_value_array(&output, "reviewers")?;
            Ok(reviewers
                .into_iter()
                .filter(|reviewer| reviewer.vote != 0)
                .map(|reviewer| PullRequestReviewRecord {
                    author: reviewer.display_name.or(reviewer.unique_name),
                    // Azure reviewer votes do not include a submission time or
                    // the commit voted on. Leave both absent rather than infer
                    // values that could incorrectly scope the commit selector.
                    submitted_at: None,
                    commit_oid: None,
                })
                .collect())
        })()
        .unwrap_or_default();
        Ok(PullRequestReviewMetadata {
            viewer_login,
            reviews,
        })
    }

    fn get_pull_request_commit_range_diff(
        &self,
        pr: &PullRequestDetails,
        start_sha: &str,
        end_sha: &str,
    ) -> Result<String> {
        self.immutable_diff(pr, start_sha, end_sha)
    }

    fn local_checkout_path(&self) -> Option<PathBuf> {
        self.local_checkout.clone()
    }

    fn create_review(
        &self,
        pr: &PullRequestDetails,
        request: CreateReviewRequest<'_>,
    ) -> Result<GhCreateReviewResponse> {
        submit::create_review(self, pr, request)
    }

    fn create_review_detailed(
        &self,
        pr: &PullRequestDetails,
        request: CreateReviewRequest<'_>,
    ) -> Result<ReviewSubmissionOutcome> {
        submit::create_review_detailed(self, pr, request)
    }

    fn supports_submit_event(&self, event: crate::forge::submit::SubmitEvent) -> bool {
        event != crate::forge::submit::SubmitEvent::Draft
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzureReviewer {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    unique_name: Option<String>,
    #[serde(default)]
    vote: i32,
}

fn normalize_route(route: &str) -> String {
    route
        .trim_matches('/')
        .split('/')
        .map(|segment| {
            let segment = segment.trim();
            if segment.starts_with('{') && segment.ends_with('}') {
                format!(
                    "{{{}}}",
                    segment[1..segment.len() - 1]
                        .trim_start_matches(['*', '?'])
                        .to_ascii_lowercase()
                )
            } else {
                segment.to_ascii_lowercase()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn route_matches(discovered_route: &str, normalized_expected: &str) -> bool {
    let discovered = normalize_route(discovered_route);
    if discovered == normalized_expected {
        return true;
    }
    discovered
        .rsplit_once('/')
        .is_some_and(|(base, last)| base == normalized_expected && last.starts_with('{'))
}

fn parse_value_array<T>(json: &str, label: &str) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let value: serde_json::Value = serde_json::from_str(json)?;
    let values = if value.is_array() {
        value
    } else {
        value.get("value").cloned().ok_or_else(|| {
            TuicrError::Forge(format!(
                "Azure DevOps {label} response has no `value` array"
            ))
        })?
    };
    Ok(serde_json::from_value(values)?)
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn azure_remote_url(repository: &ForgeRepository) -> Result<String> {
    let identity = AzureDevOpsBackend::<SystemAzRunner>::identity(repository)?;
    Ok(format!(
        "{}/{}/_git/{}",
        identity.organization_url.trim_end_matches('/'),
        percent_encode_path_component(&identity.project),
        percent_encode_path_component(&repository.name)
    ))
}

fn git_executable() -> String {
    env::var("TUICR_GIT_BIN")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "git".to_string())
}

fn run_git(root: &Path, args: &[&str]) -> std::result::Result<String, String> {
    run_command_output(
        &git_executable(),
        Some(root),
        args.iter().map(|argument| OsStr::new(*argument)),
    )
    .map_err(|error| {
        if error.stderr.trim().is_empty() {
            error.status.map_or_else(
                || "git command failed".to_string(),
                |code| format!("git exited with status {code}"),
            )
        } else {
            error.stderr
        }
    })
}

fn rest_diff_limitation() -> TuicrError {
    TuicrError::UnsupportedOperation(
        "Azure DevOps cumulative REST diff fallback is unavailable: the documented latest-iteration Changes response cannot always reconstruct copies, renames, binary modes, and truncated content without risking a silently incorrect patch. Use a matching local checkout so tuicr can run immutable Git object operations."
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::forge::azure_devops::az::{AzCommandError, AzCommandResult};

    type RecordedCall = (Vec<String>, Option<Vec<u8>>);

    #[derive(Default)]
    struct FakeRunner {
        replies: Mutex<VecDeque<AzCommandResult<String>>>,
        calls: Mutex<Vec<RecordedCall>>,
    }

    impl FakeRunner {
        fn with_replies(replies: impl IntoIterator<Item = &'static str>) -> Self {
            Self {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|reply| Ok(reply.to_string()))
                        .collect(),
                ),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl AzCommandRunner for FakeRunner {
        fn run(&self, args: &[String]) -> AzCommandResult<String> {
            self.calls.lock().unwrap().push((args.to_vec(), None));
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok("{}".to_string()))
        }

        fn run_with_input_file(&self, args: &[String], input: &[u8]) -> AzCommandResult<String> {
            self.calls
                .lock()
                .unwrap()
                .push((args.to_vec(), Some(input.to_vec())));
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok("{}".to_string()))
        }
    }

    fn repository() -> ForgeRepository {
        ForgeRepository::azure_devops(
            "dev.azure.com",
            "neutral-org",
            "https://dev.azure.com/neutral-org",
            "neutral-project",
            "neutral-repo",
        )
        .with_repository_id("repo-id")
    }

    #[test]
    fn list_uses_specialized_command_and_documented_reviewer_flag() {
        let runner =
            FakeRunner::with_replies([r#"{"user":{"name":"reviewer@example.invalid"}}"#, "[]"]);
        let backend = AzureDevOpsBackend::with_runner(None, runner);
        backend
            .list_pull_requests(PullRequestListQuery {
                repository: repository(),
                already_loaded: 4,
                page_size: 2,
                scope: PullRequestListScope::ReviewRequested,
            })
            .unwrap();
        let calls = backend.runner.calls.lock().unwrap();
        assert_eq!(calls[0].0, ["account", "show"]);
        assert_eq!(
            calls[1].0,
            [
                "repos",
                "pr",
                "list",
                "--organization",
                "https://dev.azure.com/neutral-org",
                "--project",
                "neutral-project",
                "--repository",
                "repo-id",
                "--status",
                "active",
                "--skip",
                "4",
                "--top",
                "3",
                "--reviewer",
                "reviewer@example.invalid",
            ]
        );
    }

    #[test]
    fn show_uses_only_the_documented_specialized_selector() {
        let runner = FakeRunner::with_replies([r#"{
            "pullRequestId":9,"title":"x","status":"active",
            "sourceRefName":"refs/heads/topic","targetRefName":"refs/heads/main",
            "lastMergeSourceCommit":{"commitId":"head"},
            "lastMergeTargetCommit":{"commitId":"base"},
            "repository":{"id":"resolved-id"}
        }"#]);
        let backend = AzureDevOpsBackend::with_runner(Some(repository()), runner);
        backend
            .get_pull_request(PullRequestTarget::number(9, "9"))
            .unwrap();
        assert_eq!(
            backend.runner.calls.lock().unwrap()[0].0,
            [
                "repos",
                "pr",
                "show",
                "--id",
                "9",
                "--organization",
                "https://dev.azure.com/neutral-org"
            ]
        );
    }

    #[test]
    fn invoke_uses_discovered_resource_and_exact_route_parameters() {
        let discovery = format!(
            r#"[{{"area":"git","resourceName":"server-name","routeTemplate":"{COMMITS_ROUTE}"}}]"#
        );
        let runner = FakeRunner::with_replies([Box::leak(discovery.into_boxed_str()), "[]"]);
        let backend = AzureDevOpsBackend::with_runner(None, runner);
        let pr = PullRequestDetails {
            repository: repository(),
            number: 7,
            title: String::new(),
            url: String::new(),
            state: "OPEN".to_string(),
            is_draft: false,
            author: None,
            head_ref_name: String::new(),
            base_ref_name: String::new(),
            head_sha: "head".to_string(),
            base_sha: "base".to_string(),
            body: String::new(),
            updated_at: None,
            closed: false,
            merged_at: None,
            diff_start_sha: None,
        };
        backend.list_pull_request_commits(&pr).unwrap();
        let calls = backend.runner.calls.lock().unwrap();
        assert_eq!(
            calls[0].0,
            [
                "devops",
                "invoke",
                "--organization",
                "https://dev.azure.com/neutral-org",
                "--query",
                "[?area=='git']"
            ]
        );
        assert!(
            calls[1]
                .0
                .windows(2)
                .any(|pair| pair == ["--resource", "server-name"])
        );
        assert!(calls[1].0.contains(&"repositoryId=repo-id".to_string()));
        assert!(calls[1].0.contains(&"$top=1000".to_string()));
        assert!(
            !calls[1]
                .0
                .iter()
                .any(|argument| argument.contains("resource-guid"))
        );
    }

    #[test]
    fn missing_exact_discovery_route_is_explicit() {
        let runner = FakeRunner::with_replies([
            r#"[{"area":"git","resourceName":"wrong","routeTemplate":"{project}/_apis/git/repositories/{repositoryId}/commits"}]"#,
        ]);
        let backend = AzureDevOpsBackend::with_runner(None, runner);
        let error = backend
            .invoke(&repository(), COMMITS_ROUTE, None, &[], &[], None)
            .unwrap_err();
        assert!(error.to_string().contains("no exact route"));
    }

    #[test]
    fn discovery_accepts_documented_collection_route_with_optional_item_id() {
        let discovered = format!("{THREADS_ROUTE}/{{threadId}}");
        assert!(route_matches(&discovered, &normalize_route(THREADS_ROUTE)));
    }

    #[test]
    fn runner_errors_are_routed_through_azure_error_mapping() {
        let runner = FakeRunner {
            replies: Mutex::new(VecDeque::from([Err(AzCommandError::Failed {
                status: Some(1),
                output: "401 Unauthorized".to_string(),
            })])),
            calls: Mutex::new(Vec::new()),
        };
        let backend = AzureDevOpsBackend::with_runner(Some(repository()), runner);
        let error = backend
            .get_pull_request(PullRequestTarget::number(1, "1"))
            .unwrap_err();
        assert!(error.to_string().contains("az login"));
    }

    #[cfg(unix)]
    #[test]
    fn immutable_diff_uses_exact_object_only_git_arguments() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        let _lock = crate::process::TEST_ENV_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("checkout");
        fs::create_dir(&checkout).unwrap();
        let git_repository = git2::Repository::init(&checkout).unwrap();
        git_repository
            .remote(
                "origin",
                "https://dev.azure.com/neutral-org/neutral-project/_git/neutral-repo",
            )
            .unwrap();

        let executable = directory.path().join("fake-git");
        let log = directory.path().join("git.log");
        let state = directory.path().join("fetched");
        fs::write(
            &executable,
            r#"#!/bin/sh
for argument in "$@"; do printf '%s\037' "$argument" >> "$TUICR_GIT_LOG"; done
printf '\n' >> "$TUICR_GIT_LOG"
if [ "$1" = "cat-file" ] && [ "$3" = "missing^{commit}" ] && [ ! -f "$TUICR_GIT_STATE" ]; then
  exit 1
fi
if [ "$1" = "fetch" ]; then
  touch "$TUICR_GIT_STATE"
fi
if [ "$1" = "diff" ]; then
  printf 'diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n'
fi
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).unwrap();

        let previous_bin = env::var_os("TUICR_GIT_BIN");
        let previous_log = env::var_os("TUICR_GIT_LOG");
        let previous_state = env::var_os("TUICR_GIT_STATE");
        // SAFETY: this test serializes and restores these process-wide values.
        unsafe {
            env::set_var("TUICR_GIT_BIN", &executable);
            env::set_var("TUICR_GIT_LOG", &log);
            env::set_var("TUICR_GIT_STATE", &state);
        }

        let backend = AzureDevOpsBackend::with_runner(None, FakeRunner::default())
            .with_local_checkout(Some(checkout));
        let pr = PullRequestDetails {
            repository: repository(),
            number: 7,
            title: String::new(),
            url: String::new(),
            state: "OPEN".to_string(),
            is_draft: false,
            author: None,
            head_ref_name: String::new(),
            base_ref_name: String::new(),
            head_sha: "missing".to_string(),
            base_sha: "base".to_string(),
            body: String::new(),
            updated_at: None,
            closed: false,
            merged_at: None,
            diff_start_sha: None,
        };
        let diff = backend
            .get_pull_request_commit_range_diff(&pr, "base", "missing")
            .unwrap();

        // SAFETY: restore values before releasing ENV_LOCK.
        unsafe {
            restore_env("TUICR_GIT_BIN", previous_bin);
            restore_env("TUICR_GIT_LOG", previous_log);
            restore_env("TUICR_GIT_STATE", previous_state);
        }

        assert!(diff.contains("diff --git a/file.txt b/file.txt"));
        let log = fs::read_to_string(log).unwrap();
        let calls = log
            .lines()
            .map(|line| {
                line.trim_end_matches('\u{1f}')
                    .split('\u{1f}')
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(calls[0], ["cat-file", "-e", "base^{commit}"]);
        assert_eq!(calls[1], ["cat-file", "-e", "missing^{commit}"]);
        assert_eq!(
            calls[2],
            [
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "https://dev.azure.com/neutral-org/neutral-project/_git/neutral-repo",
                "missing"
            ]
        );
        assert_eq!(calls[3], ["cat-file", "-e", "missing^{commit}"]);
        assert_eq!(
            calls[4],
            [
                "diff",
                "--no-ext-diff",
                "--binary",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "base",
                "missing"
            ]
        );
        assert!(!calls.iter().flatten().any(|argument| matches!(
            *argument,
            "checkout" | "switch" | "reset" | "clean" | "stash"
        )));
    }

    #[cfg(unix)]
    unsafe fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => unsafe { env::set_var(key, value) },
            None => unsafe { env::remove_var(key) },
        }
    }
}
