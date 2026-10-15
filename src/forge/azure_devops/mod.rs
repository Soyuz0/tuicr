pub mod az;
mod discovery;
pub mod models;
pub mod remote;
mod submit;

use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use serde::Deserialize;

use crate::error::{Result, TuicrError};
use crate::forge::remote_comments::RemoteReviewThread;
use crate::forge::traits::{
    CreateReviewRequest, ForgeBackend, ForgeFileLinesRequest, ForgeRepository,
    GhCreateReviewResponse, PagedPullRequests, PullRequestCommit, PullRequestDetails,
    PullRequestListQuery, PullRequestListScope, PullRequestReviewMetadata, PullRequestReviewRecord,
    PullRequestTarget, ReviewSubmissionOutcome,
};
use crate::model::{DiffLine, FilePatch};
use crate::process::{run_command_output, run_command_output_bytes_with_stdin};
use crate::vcs::slice_context_lines;

use self::az::{AzCommandRunner, SystemAzRunner, json_payload, map_az_error};
use self::models::{
    AzureApiResourceLocation, AzureGitItem, AzureIteration, AzureIterationChange, AzurePullRequest,
    parse_azure_commits, parse_azure_iteration_changes, parse_azure_iterations,
    parse_azure_pull_requests, parse_azure_threads,
};

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

#[derive(Debug, Default)]
pub struct AzureDevOpsBackend<R = SystemAzRunner> {
    default_repository: Option<ForgeRepository>,
    runner: R,
    local_checkout: Option<PathBuf>,
    /// Memoizes "does `local_checkout` point at this repository?" per repository.
    /// Resolving it opens the repo with libgit2 and walks its remotes, which is
    /// far too expensive to repeat per file.
    checkout_matches: Mutex<HashMap<String, bool>>,
}

impl AzureDevOpsBackend<SystemAzRunner> {
    pub fn new(default_repository: Option<ForgeRepository>) -> Self {
        Self {
            default_repository,
            runner: SystemAzRunner,
            local_checkout: None,
            checkout_matches: Mutex::new(HashMap::new()),
        }
    }
}

/// Tolerates poisoning: this is a pure memo, so a panic elsewhere must not turn
/// every later lookup into a failure.
fn lock_checkout_cache(
    cache: &Mutex<HashMap<String, bool>>,
) -> MutexGuard<'_, HashMap<String, bool>> {
    cache.lock().unwrap_or_else(|error| error.into_inner())
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
            checkout_matches: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_local_checkout(mut self, checkout: Option<PathBuf>) -> Self {
        self.local_checkout = checkout;
        self
    }

    fn run_az(&self, args: Vec<String>) -> Result<String> {
        self.runner
            .run(&args)
            .map_err(map_az_error)
            .map(|output| json_payload(&output).to_string())
    }

    pub(crate) fn run_az_with_input(&self, args: Vec<String>, input: &[u8]) -> Result<String> {
        self.runner
            .run_with_input_file(&args, input)
            .map_err(map_az_error)
            .map(|output| json_payload(&output).to_string())
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

    /// Run the untargeted `az devops invoke` that enumerates the organization's
    /// API surface. This is the expensive call (16-22s against a live org), so
    /// every caller goes through [`discovery::resource_table`].
    fn fetch_resource_table(&self, organization_url: &str) -> Result<String> {
        self.run_az(vec![
            "devops".to_string(),
            "invoke".to_string(),
            "--organization".to_string(),
            organization_url.to_string(),
            "--query".to_string(),
            "[?area=='git']".to_string(),
        ])
    }

    fn discover_resource(
        &self,
        repository: &ForgeRepository,
        expected_route: &str,
    ) -> Result<AzureApiResourceLocation> {
        let identity = Self::identity(repository)?;
        let organization_url = identity.organization_url.as_str();
        let expected = normalize_route(expected_route);

        let find = |table: &[AzureApiResourceLocation]| {
            table
                .iter()
                .find(|location| {
                    location.area.eq_ignore_ascii_case("git")
                        && route_matches(&resolve_route_placeholders(location), &expected)
                        && !location.resource_name.trim().is_empty()
                })
                .cloned()
        };

        let cached = discovery::resource_table(organization_url, false, || {
            self.fetch_resource_table(organization_url)
        })?;
        if let Some(location) = find(&cached.table) {
            return Ok(location);
        }

        // A *cached* table that lacks the route is treated as stale rather than
        // as a hard failure, so a route added after the entry was written still
        // resolves. A freshly fetched table is authoritative, so it is not
        // refetched -- that would double the `az` calls on a real miss.
        if cached.from_cache {
            let fresh = discovery::resource_table(organization_url, true, || {
                self.fetch_resource_table(organization_url)
            })?;
            if let Some(location) = find(&fresh.table) {
                return Ok(location);
            }
        }

        Err(TuicrError::Forge(format!(
            "Azure DevOps Git API discovery returned no exact route for `{expected_route}`"
        )))
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
        if !route_matches(
            &resolve_route_placeholders(&resource),
            &normalize_route(expected_route),
        ) {
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

    /// Resolve (and remember) whether the configured checkout really points at
    /// `repository`.
    ///
    /// The underlying check opens the repository with libgit2 and walks its
    /// remotes. It used to run once per file during a PR open, which is pure
    /// repeated work: both the checkout path and the repository are fixed for
    /// the lifetime of this backend.
    fn matching_checkout(&self, repository: &ForgeRepository) -> Option<&Path> {
        let root = self.local_checkout.as_deref()?;
        let key = repository.display_name();
        if let Some(&matched) = lock_checkout_cache(&self.checkout_matches).get(&key) {
            return matched.then_some(root);
        }
        let matched = crate::forge::local_checkout_for_repo(root, repository).is_some();
        lock_checkout_cache(&self.checkout_matches).insert(key, matched);
        matched.then_some(root)
    }

    fn local_content(&self, request: &ForgeFileLinesRequest) -> Option<String> {
        let root = self.matching_checkout(&request.repository)?;
        let spec = format!("{}:{}", request.sha(), slash_path(request.path.as_path()));
        run_git(root, &["cat-file", "blob", &spec]).ok()
    }

    /// Resolve line counts for as many requests as the local checkout can serve,
    /// using a single `git cat-file --batch` process.
    ///
    /// Returns the indices it could **not** answer, so the caller can fall back
    /// to the per-file (and possibly remote) path for those. One process for the
    /// whole PR replaces one spawn per changed file, which was measured at ~1s of
    /// blocked UI for a 167-file PR versus ~50ms batched.
    fn batch_local_line_counts(
        &self,
        requests: &[ForgeFileLinesRequest],
        counts: &mut [Option<u32>],
    ) -> Vec<usize> {
        let all = || (0..requests.len()).collect::<Vec<_>>();
        let Some(repository) = requests.first().map(|request| &request.repository) else {
            return Vec::new();
        };
        // Every request in a batch belongs to one PR, so a single checkout check
        // covers them all; anything else falls back per file.
        if requests
            .iter()
            .any(|request| !request.repository.same_repository(repository))
        {
            return all();
        }
        let Some(root) = self.matching_checkout(repository) else {
            return all();
        };

        // `git cat-file --batch` takes one newline-delimited spec per line, so a
        // path containing a newline would split into several requests and shift
        // every later record. Such paths are excluded from the batch and answered
        // per file instead; misattributing a line count is worse than a subprocess.
        let mut unresolved = Vec::new();
        let mut batched = Vec::with_capacity(requests.len());
        let mut stdin = Vec::with_capacity(requests.len() * 64);
        for (index, request) in requests.iter().enumerate() {
            let path = slash_path(request.path.as_path());
            if path.contains('\n') || path.contains('\r') {
                unresolved.push(index);
                continue;
            }
            batched.push(index);
            stdin.extend_from_slice(request.sha().as_bytes());
            stdin.push(b':');
            stdin.extend_from_slice(path.as_bytes());
            stdin.push(b'\n');
        }
        if batched.is_empty() {
            return all();
        }

        let Ok(stdout) = run_command_output_bytes_with_stdin(
            &git_executable(),
            Some(root),
            ["cat-file", "--batch"].iter().map(OsStr::new),
            &stdin,
        ) else {
            return all();
        };

        let mut offset = 0;
        let mut batched = batched.into_iter();
        while let Some(index) = batched.next() {
            match parse_batch_record(&stdout, offset) {
                Some((count, next)) => {
                    counts[index] = Some(count);
                    offset = next;
                }
                None => {
                    // Either the object is missing or the stream desynchronized.
                    // Advance past this record when possible; otherwise abandon the
                    // remainder so nothing is attributed to the wrong file.
                    match skip_batch_record(&stdout, offset) {
                        Some(next) => {
                            unresolved.push(index);
                            offset = next;
                        }
                        None => {
                            unresolved.push(index);
                            unresolved.extend(batched);
                            break;
                        }
                    }
                }
            }
        }
        unresolved.sort_unstable();
        unresolved
    }

    fn immutable_diff(
        &self,
        pr: &PullRequestDetails,
        start_sha: &str,
        end_sha: &str,
    ) -> Result<Vec<FilePatch>> {
        let root = self
            .local_checkout
            .as_deref()
            .ok_or_else(rest_diff_limitation)?;
        let remote = crate::forge::local_fetch_remote_for_repo(root, &pr.repository)
            .ok_or_else(rest_diff_limitation)?;
        for sha in [start_sha, end_sha] {
            let commit = format!("{sha}^{{commit}}");
            if run_git(root, &["cat-file", "-e", &commit]).is_err() {
                fetch_commit(root, &remote, sha)?;
                run_git(root, &["cat-file", "-e", &commit]).map_err(|error| {
                    TuicrError::Forge(format!(
                        "Azure DevOps fetch completed but commit `{sha}` is still unavailable: {error}"
                    ))
                })?;
            }
        }
        // Three-dot: diff from the merge base of the two commits, not between
        // their tips. Azure's UI diffs a PR against the iteration's
        // `commonRefCommit` (the merge base), so a two-dot diff against the
        // target branch tip also reported every commit merged into the target
        // since the branch diverged -- as reversed changes. On a real PR that
        // turned 40 changed files into 167.
        //
        // For a commit subrange `start_sha` is already an ancestor of
        // `end_sha`, so the merge base is `start_sha` and this is equivalent to
        // the two-dot form.
        let range = format!("{start_sha}...{end_sha}");
        let output = run_command_output_bytes_with_stdin(
            &git_executable(),
            Some(root),
            [
                "diff",
                "--no-ext-diff",
                "--raw",
                "-z",
                "--patch",
                "--binary",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                &range,
            ],
            &[],
        )
        .map_err(|error| {
            TuicrError::Forge(format!(
                "Azure DevOps local Git diff failed: {}",
                error.stderr
            ))
        })?;
        crate::vcs::git::raw::parse_raw_patch_output(&output)
    }

    /// Resolve the merge base that the diff's `a/` side actually refers to.
    ///
    /// Azure reports `lastMergeTargetCommit` (the target branch tip) as the
    /// base, but the diff is taken from the merge base. Leaving `base_sha` at the
    /// tip made base-side context expansion read the wrong revision for any file
    /// that also changed on the target branch. Best-effort: on failure the
    /// caller keeps Azure's value, and the diff stays correct regardless because
    /// [`Self::immutable_diff`] recomputes the merge base itself.
    fn resolve_merge_base(&self, pr: &PullRequestDetails) -> Option<String> {
        let root = self.matching_checkout(&pr.repository)?;
        let merge_base = run_git(root, &["merge-base", &pr.base_sha, &pr.head_sha]).ok()?;
        let merge_base = merge_base.trim();
        (!merge_base.is_empty()).then(|| merge_base.to_string())
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

    fn get_pull_request_diff(&self, pr: &PullRequestDetails) -> Result<Vec<FilePatch>> {
        self.immutable_diff(pr, &pr.base_sha, &pr.head_sha)
    }

    fn resolve_diff_base_sha(&self, pr: &PullRequestDetails) -> Option<String> {
        self.resolve_merge_base(pr)
    }

    fn fetch_file_lines(&self, request: ForgeFileLinesRequest) -> Result<Vec<DiffLine>> {
        if request.start_line == 0 || request.start_line > request.end_line {
            return Ok(Vec::new());
        }
        let content = self.fetch_file_content(request.clone())?;
        Ok(slice_context_lines(
            &content,
            request.start_line,
            request.end_line,
        ))
    }

    fn fetch_file_content(&self, request: ForgeFileLinesRequest) -> Result<String> {
        self.local_content(&request)
            .map(Ok)
            .unwrap_or_else(|| self.fetch_item(&request))
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

    fn file_line_counts(&self, requests: Vec<ForgeFileLinesRequest>) -> Vec<Option<u32>> {
        let mut counts = vec![None; requests.len()];
        for index in self.batch_local_line_counts(&requests, &mut counts) {
            counts[index] = self.file_line_count(requests[index].clone()).ok();
        }
        counts
    }

    fn list_review_threads(&self, pr: &PullRequestDetails) -> Result<Vec<RemoteReviewThread>> {
        let route = Self::pull_request_route_parameters(&pr.repository, pr.number)?;
        let output = self.invoke(&pr.repository, THREADS_ROUTE, None, &route, &[], None)?;
        Ok(parse_azure_threads(&output)?
            .into_iter()
            .filter_map(|thread| thread.into_review_thread())
            .collect())
    }

    fn set_review_thread_resolved(
        &self,
        pr: &PullRequestDetails,
        thread_id: &str,
        resolved: bool,
    ) -> Result<()> {
        // `RemoteReviewThread::id` is Azure's numeric thread id (see
        // `AzureThread::into_review_thread`), which is exactly the `threadId`
        // route parameter. Reject anything else rather than build a bad route.
        if thread_id.is_empty() || !thread_id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(TuicrError::Forge(format!(
                "Azure DevOps review thread id `{thread_id}` is not numeric"
            )));
        }
        let mut route = Self::pull_request_route_parameters(&pr.repository, pr.number)?;
        route.push(("threadId", thread_id.to_string()));

        // `fixed` is what the Azure UI's "Resolved" sets; `active` is its
        // "Reactivate". Both round-trip through `is_resolved_status`.
        let status = if resolved { "fixed" } else { "active" };
        let body = serde_json::to_vec(&serde_json::json!({ "status": status }))?;
        self.invoke(
            &pr.repository,
            THREADS_ROUTE,
            Some("PATCH"),
            &route,
            &[],
            Some(&body),
        )?;
        Ok(())
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
    ) -> Result<Vec<FilePatch>> {
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

/// Substitute the placeholders that a resource location resolves by definition.
///
/// Azure returns parameterized templates such as
/// `{project}/_apis/{area}/repositories/{repositoryId}/{resource}/{*path}`,
/// where `{area}` and `{resource}` are filled in by the entry's own `area` and
/// `resourceName`. The expected routes in this module are written with those
/// already resolved (literal `git`, literal `items`), so comparing raw templates
/// never matched a single real route and discovery always failed.
fn resolve_route_placeholders(location: &AzureApiResourceLocation) -> String {
    location
        .route_template
        .replace("{area}", &location.area)
        .replace("{resource}", &location.resource_name)
}

fn route_matches(discovered_route: &str, normalized_expected: &str) -> bool {
    let discovered = normalize_route(discovered_route);
    if discovered == normalized_expected {
        return true;
    }
    // Collection routes carry an optional trailing item segment
    // (`.../threads/{threadId}`, `.../items/{*path}`).
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

fn fetch_commit(root: &Path, remote: &str, sha: &str) -> Result<()> {
    let output = Command::new(git_executable())
        .current_dir(root)
        .args([
            "-c",
            "credential.interactive=false",
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            remote,
            sha,
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            TuicrError::Forge(format!(
                "Could not run Git to fetch Azure PR commit `{sha}`: {error}"
            ))
        })?;
    if !output.status.success() {
        return Err(TuicrError::Forge(format!(
            "Azure DevOps immutable fetch of `{sha}` using remote `{remote}` failed: {}\nRun `git fetch {remote}` in this checkout to verify its Git credentials, then retry. Azure CLI login and Git authentication are separate.",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn git_executable() -> String {
    env::var("TUICR_GIT_BIN")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "git".to_string())
}

/// Split one `git cat-file --batch` record header at `offset`.
///
/// A successful record is `<oid> SP <type> SP <size> LF`, followed by exactly
/// `size` content bytes and a trailing `LF`. Missing objects instead emit
/// `<request> SP missing LF` with no content, and newer Git can report
/// `ambiguous`. Anything that is not a well-formed three-field header with a
/// numeric size is treated as "no content".
fn parse_batch_header(stdout: &[u8], offset: usize) -> Option<(usize, usize)> {
    let rest = stdout.get(offset..)?;
    let newline = rest.iter().position(|byte| *byte == b'\n')?;
    let header = std::str::from_utf8(&rest[..newline]).ok()?;
    let size = header
        .rsplit(' ')
        .next()
        .filter(|_| header.split(' ').count() >= 3)
        .and_then(|size| size.parse::<usize>().ok())?;
    Some((size, offset + newline + 1))
}

/// Line count for the record at `offset`, plus the offset of the next record.
///
/// Matches `str::lines().count()`: a trailing newline does not add a line.
fn parse_batch_record(stdout: &[u8], offset: usize) -> Option<(u32, usize)> {
    let (size, content_start) = parse_batch_header(stdout, offset)?;
    // A malformed header can advertise an enormous size, so this must not be
    // allowed to overflow into a valid-looking (wrong) range.
    let content_end = content_start.checked_add(size)?;
    let content = stdout.get(content_start..content_end)?;
    // The record is only complete once its trailing separator is present.
    if stdout.get(content_end) != Some(&b'\n') {
        return None;
    }
    let newlines = content.iter().filter(|byte| **byte == b'\n').count();
    let lines = newlines + usize::from(!content.is_empty() && content.last() != Some(&b'\n'));
    Some((u32::try_from(lines).ok()?, content_end + 1))
}

/// Offset just past the record at `offset`, for records with no usable content
/// (`missing`/`ambiguous`), which occupy only their header line.
///
/// Returns `None` when the stream cannot be resynchronized, so the caller stops
/// rather than resuming mid-record and shifting every later file's count.
fn skip_batch_record(stdout: &[u8], offset: usize) -> Option<usize> {
    if let Some((size, content_start)) = parse_batch_header(stdout, offset) {
        // A well-formed numeric header means a content record. Only skip it when
        // it is actually complete; a truncated one leaves the stream unusable.
        let content_end = content_start.checked_add(size)?;
        return (stdout.get(content_end) == Some(&b'\n')).then_some(content_end + 1);
    }
    // Otherwise it is a header-only record such as `<request> missing`.
    let rest = stdout.get(offset..)?;
    let newline = rest.iter().position(|byte| *byte == b'\n')?;
    Some(offset + newline + 1)
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

    fn details(number: u64) -> PullRequestDetails {
        PullRequestDetails {
            repository: repository(),
            number,
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
        }
    }

    /// Discovery is the dominant cost of an Azure PR open (16-22s live), and
    /// `invoke` needs it for every REST call. One PR open issues at least three
    /// `invoke`s, so they have to share a single discovery.
    #[test]
    fn repeated_invokes_share_one_cached_discovery() {
        let cache = discovery::TestCache::enable();
        let discovered = format!(
            r#"[{{"area":"git","resourceName":"server-name","routeTemplate":"{COMMITS_ROUTE}"}},
                {{"area":"git","resourceName":"threads-name","routeTemplate":"{THREADS_ROUTE}"}}]"#
        );
        let runner = FakeRunner::with_replies([
            Box::leak(discovered.into_boxed_str()),
            "[]",
            r#"{"value":[]}"#,
        ]);
        let backend = AzureDevOpsBackend::with_runner(None, runner);
        let pr = details(7);

        backend.list_pull_request_commits(&pr).unwrap();
        backend.list_review_threads(&pr).unwrap();

        let discovery_calls = backend
            .runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.0.iter().any(|argument| argument == "[?area=='git']"))
            .count();
        assert_eq!(
            discovery_calls, 1,
            "two invokes must not each pay for discovery"
        );

        // A fresh backend on another thread (one per PR-open phase) must reuse
        // the process-wide memo rather than rediscovering.
        let second = AzureDevOpsBackend::with_runner(None, FakeRunner::with_replies(["[]"]));
        second.list_pull_request_commits(&pr).unwrap();
        assert!(
            second.runner.calls.lock().unwrap()[0]
                .0
                .iter()
                .all(|argument| argument != "[?area=='git']"),
            "a new backend must reuse the cached table"
        );

        // Dropping the memo still avoids the network: the on-disk entry serves it.
        cache.clear_memo();
        let third = AzureDevOpsBackend::with_runner(None, FakeRunner::with_replies(["[]"]));
        third.list_pull_request_commits(&pr).unwrap();
        assert!(
            third.runner.calls.lock().unwrap()[0]
                .0
                .iter()
                .all(|argument| argument != "[?area=='git']"),
            "the persisted cache must survive losing the memo"
        );
    }

    /// A cached table predates any route Azure adds later, so a miss against a
    /// cached table must retry once with a live fetch instead of failing.
    #[test]
    fn cached_table_missing_a_route_refetches_once() {
        let _cache = discovery::TestCache::enable();
        let stale = r#"[{"area":"git","resourceName":"threads-name","routeTemplate":"{project}/_apis/git/repositories/{repositoryId}/pullRequests/{pullRequestId}/threads"}]"#;
        let fresh = format!(
            r#"[{{"area":"git","resourceName":"server-name","routeTemplate":"{COMMITS_ROUTE}"}}]"#
        );
        // Reply order: seed discovery, threads payload, forced discovery, commits payload.
        let runner = FakeRunner::with_replies([
            stale,
            r#"{"value":[]}"#,
            Box::leak(fresh.into_boxed_str()),
            "[]",
        ]);
        let backend = AzureDevOpsBackend::with_runner(None, runner);
        let pr = details(7);

        // Seeds the cache with a table that lacks COMMITS_ROUTE.
        backend.list_review_threads(&pr).unwrap();
        backend.list_pull_request_commits(&pr).unwrap();

        let calls = backend.runner.calls.lock().unwrap();
        let discovery_calls = calls
            .iter()
            .filter(|call| call.0.iter().any(|argument| argument == "[?area=='git']"))
            .count();
        assert_eq!(discovery_calls, 2, "exactly one forced refetch");
        assert!(
            calls
                .last()
                .unwrap()
                .0
                .windows(2)
                .any(|pair| pair == ["--resource", "server-name"])
        );
    }

    /// `az devops invoke` writes `Please wait a couple of seconds...` to stdout
    /// (not stderr) and `--only-show-errors` does not suppress it. Feeding that
    /// to serde failed at column 1, which silently disabled every discovery-backed
    /// Azure feature. Live output also returns `minVersion`/`maxVersion` as JSON
    /// numbers, which a `String` field rejected.
    #[test]
    fn tolerates_stdout_preamble_and_numeric_api_versions() {
        let discovered = format!(
            "Please wait a couple of seconds while we fetch all required information.\n\
             [{{\"area\":\"git\",\"resourceName\":\"server-name\",\"minVersion\":2.1,\
             \"maxVersion\":7.2,\"releasedVersion\":\"7.1\",\"resourceVersion\":1,\
             \"routeTemplate\":\"{COMMITS_ROUTE}\"}}]"
        );
        let runner = FakeRunner::with_replies([Box::leak(discovered.into_boxed_str()), "[]"]);
        let backend = AzureDevOpsBackend::with_runner(None, runner);

        backend
            .list_pull_request_commits(&details(7))
            .expect("live-shaped discovery output must parse");
        assert!(
            backend.runner.calls.lock().unwrap()[1]
                .0
                .windows(2)
                .any(|pair| pair == ["--resource", "server-name"])
        );
    }

    /// Byte-for-byte output of a real `git cat-file --batch` for, in order:
    /// a file ending in a newline, one that does not, an empty file, a binary
    /// file, and a missing object.
    const REAL_BATCH_STREAM: &[u8] = b"de980441c3ab03a8c07dda1ad27b8a11f39deb1e blob 6\na\nb\nc\n\n1c943a98887754f364fafaa1da3ac56e0e0875a9 blob 5\na\nb\nc\ne69de29bb2d1d6434b8b29ae775ad8c2e48c5391 blob 0\n\nb538f94a18841c76cfe8257f992b84fb562ac9ef blob 6\nx\x00\xff\ny\n\nHEAD:nope.txt missing\n";

    /// Line counts must match `str::lines().count()`, and the length-prefixed
    /// walk must stay byte-exact: the binary record contains `0xFF`, which lossy
    /// UTF-8 conversion would widen and desynchronize every later record.
    #[test]
    fn parses_real_cat_file_batch_records_including_binary_and_missing() {
        let mut offset = 0;
        let mut counts = Vec::new();
        for _ in 0..4 {
            let (count, next) =
                parse_batch_record(REAL_BATCH_STREAM, offset).expect("well-formed record");
            counts.push(count);
            offset = next;
        }
        assert_eq!(counts, vec![3, 3, 0, 2]);

        // The trailing record is `missing`: no content, header line only.
        assert!(parse_batch_record(REAL_BATCH_STREAM, offset).is_none());
        let after_missing = skip_batch_record(REAL_BATCH_STREAM, offset).expect("skip missing");
        assert_eq!(after_missing, REAL_BATCH_STREAM.len());
    }

    /// A truncated record must not be reported as a count, or the remaining
    /// files would silently inherit another file's length.
    #[test]
    fn refuses_truncated_or_malformed_batch_records() {
        let truncated = &REAL_BATCH_STREAM[..REAL_BATCH_STREAM.len() - 30];
        let mut offset = 0;
        while let Some((_, next)) = parse_batch_record(truncated, offset) {
            offset = next;
        }
        assert!(offset < truncated.len(), "expected an unparsed tail");

        assert!(parse_batch_record(b"no-newline-anywhere", 0).is_none());
        assert!(parse_batch_record(b"oid blob notanumber\ncontent\n", 0).is_none());
        assert!(parse_batch_record(b"", 0).is_none());
        assert!(skip_batch_record(b"", 0).is_none());
    }

    /// A header advertising an absurd size must not overflow into a wrong range.
    #[test]
    fn oversized_batch_header_neither_panics_nor_resynchronizes() {
        let hostile = format!("oid blob {}\ncontent\n", usize::MAX).into_bytes();
        assert!(parse_batch_record(&hostile, 0).is_none());
        assert!(skip_batch_record(&hostile, 0).is_none());

        // A complete header whose content is truncated must also refuse to skip,
        // so the caller stops instead of resuming mid-record.
        assert!(parse_batch_record(b"oid blob 100\nshort\n", 0).is_none());
        assert!(skip_batch_record(b"oid blob 100\nshort\n", 0).is_none());
    }

    /// A PR diff must show only what the PR changes, even when the target branch
    /// has moved on. Azure's UI diffs against the iteration's `commonRefCommit`
    /// (the merge base); diffing against the target tip instead also reports
    /// every commit merged into the target since the branch diverged, as
    /// reversed changes. On a real 40-file PR that produced 167 files.
    #[cfg(unix)]
    #[test]
    fn pr_diff_preserves_exact_paths_and_excludes_target_branch_commits() {
        let _lock = crate::process::TEST_ENV_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        let git_repository = git2::Repository::init(&checkout).unwrap();
        git_repository
            .remote(
                "origin",
                "https://dev.azure.com/neutral-org/neutral-project/_git/neutral-repo",
            )
            .unwrap();
        let signature = git2::Signature::now("t", "t@example.invalid").unwrap();

        let commit = |parent: Option<git2::Oid>, files: &[(&str, &str)]| -> git2::Oid {
            let mut builder = git_repository.treebuilder(None).unwrap();
            for (path, contents) in files {
                let blob = git_repository.blob(contents.as_bytes()).unwrap();
                builder.insert(path, blob, 0o100_644).unwrap();
            }
            let tree = git_repository.find_tree(builder.write().unwrap()).unwrap();
            let parents: Vec<git2::Commit> = parent
                .map(|oid| git_repository.find_commit(oid).unwrap())
                .into_iter()
                .collect();
            let borrowed: Vec<&git2::Commit> = parents.iter().collect();
            git_repository
                .commit(None, &signature, &signature, "c", &tree, &borrowed)
                .unwrap()
        };

        // Shared history, then the branch and the target diverge.
        let root = commit(None, &[("shared.txt", "shared\n")]);
        // Git appends a delimiter tab to the space-containing name's display
        // header, and quotes the literal tab. Neither may change file identity.
        let head = commit(
            Some(root),
            &[
                ("shared.txt", "shared\n"),
                ("deep-research-report (28).md", "feature\n"),
                ("literal\tname.txt", "tabbed name\n"),
            ],
        );
        // Meanwhile the target branch gains an unrelated file and edits another.
        let target = commit(
            Some(root),
            &[
                ("shared.txt", "changed on target\n"),
                ("other.txt", "other\n"),
            ],
        );

        let backend = AzureDevOpsBackend::with_runner(None, FakeRunner::default())
            .with_local_checkout(Some(checkout));
        let mut pr = details(7);
        pr.base_sha = target.to_string();
        pr.head_sha = head.to_string();

        let diff = backend.get_pull_request_diff(&pr).unwrap();
        let changed: Vec<&str> = diff
            .iter()
            .map(|file| file.display_path().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            changed,
            ["deep-research-report (28).md", "literal\tname.txt"],
            "only the PR's own change may appear; got diff:\n{diff:?}"
        );
        assert!(
            diff.iter()
                .all(|file| !file.patch.contains("changed on target")),
            "target-branch commits must not leak into the PR diff:\n{diff:?}"
        );

        // `base_sha` must name the merge base, so base-side context expansion
        // reads the revision the diff's `a/` side actually refers to.
        assert_eq!(
            backend.resolve_merge_base(&pr).as_deref(),
            Some(root.to_string().as_str())
        );

        let files = crate::vcs::diff_parser::parse_file_patches(
            diff,
            &crate::syntax::SyntaxHighlighter::default(),
        )
        .unwrap();
        assert_eq!(
            files[0].display_path(),
            Path::new("deep-research-report (28).md")
        );
        assert_eq!(files[1].display_path(), Path::new("literal\tname.txt"));
        assert_eq!(files[0].hunks[0].lines[0].content, "feature");
    }

    /// End-to-end check of the batched path against real `git cat-file --batch`
    /// output, covering the cases that break naive parsing: a file without a
    /// trailing newline, an empty file, binary content containing `0xFF` (which
    /// lossy UTF-8 conversion would widen and desynchronize), a path with a
    /// newline in it, and an object that is simply absent.
    #[cfg(unix)]
    #[test]
    fn batched_line_counts_match_real_git_for_awkward_files() {
        let directory = tempfile::tempdir().unwrap();
        let checkout = directory.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        let git_repository = git2::Repository::init(&checkout).unwrap();
        git_repository
            .remote(
                "origin",
                "https://dev.azure.com/neutral-org/neutral-project/_git/neutral-repo",
            )
            .unwrap();

        // (path, contents, expected line count)
        let files: [(&str, &[u8], u32); 5] = [
            ("trailing.txt", b"a\nb\nc\n", 3),
            ("no_trailing.txt", b"a\nb\nc", 3),
            ("empty.txt", b"", 0),
            ("binary.dat", b"x\x00\xffy\nz\n", 2),
            ("news\nline.txt", b"a\nb\n", 2),
        ];

        let commit_id = {
            let mut builder = git_repository.treebuilder(None).unwrap();
            for (path, contents, _) in files {
                let blob = git_repository.blob(contents).unwrap();
                builder.insert(path, blob, 0o100_644).unwrap();
            }
            let tree = git_repository.find_tree(builder.write().unwrap()).unwrap();
            let signature = git2::Signature::now("t", "t@example.invalid").unwrap();
            git_repository
                .commit(Some("HEAD"), &signature, &signature, "x", &tree, &[])
                .unwrap()
                .to_string()
        };

        let backend = AzureDevOpsBackend::with_runner(None, FakeRunner::default())
            .with_local_checkout(Some(checkout));

        let mut requests: Vec<ForgeFileLinesRequest> = files
            .iter()
            .map(|(path, _, _)| line_request(path))
            .collect();
        // An object that is not in the tree: `git cat-file` reports it missing.
        requests.push(line_request("absent.txt"));
        for request in &mut requests {
            request.head_sha = commit_id.clone();
        }

        let counts = backend.file_line_counts(requests);
        let expected: Vec<Option<u32>> = files
            .iter()
            .map(|(_, _, lines)| Some(*lines))
            // The absent object cannot be resolved locally, and the fake `az`
            // runner cannot serve it remotely either.
            .chain(std::iter::once(None))
            .collect();
        assert_eq!(counts, expected);
    }

    /// `git cat-file --batch` is newline-delimited, so a path containing a newline
    /// would split into extra requests and shift every later record. Those paths
    /// must be excluded from the batch rather than risk a wrong line count.
    #[test]
    fn paths_containing_newlines_are_excluded_from_the_batch() {
        let backend = AzureDevOpsBackend::with_runner(None, FakeRunner::default())
            .with_local_checkout(Some(PathBuf::from("/nonexistent/checkout")));
        let requests = vec![
            line_request("src/ok.rs"),
            line_request("src/we\nird.rs"),
            line_request("src/also\rbad.rs"),
        ];
        let mut counts = vec![None; requests.len()];
        // The checkout does not match, so everything falls back; the point is that
        // the unresolved list is complete, ordered, and free of duplicates.
        let unresolved = backend.batch_local_line_counts(&requests, &mut counts);
        assert_eq!(unresolved, vec![0, 1, 2]);
        assert!(unresolved.windows(2).all(|pair| pair[0] < pair[1]));
    }

    /// The batch must not run against a checkout that belongs to a different
    /// repository, and every request must still get an aligned slot.
    #[test]
    fn batch_line_counts_skip_a_non_matching_checkout() {
        let backend = AzureDevOpsBackend::with_runner(None, FakeRunner::default())
            .with_local_checkout(Some(PathBuf::from("/nonexistent/checkout")));
        let requests = vec![line_request("src/a.rs"), line_request("src/b.rs")];
        let mut counts = vec![None; requests.len()];
        let unresolved = backend.batch_local_line_counts(&requests, &mut counts);
        assert_eq!(unresolved, vec![0, 1], "both must fall back");
        assert_eq!(counts, vec![None, None]);
    }

    fn line_request(path: &str) -> ForgeFileLinesRequest {
        ForgeFileLinesRequest {
            repository: repository(),
            base_sha: "base".to_string(),
            head_sha: "head".to_string(),
            path: PathBuf::from(path),
            status: crate::model::FileStatus::Modified,
            side: crate::forge::traits::ForgeFileSide::Head,
            start_line: 0,
            end_line: 0,
        }
    }

    /// Resolution is a thread property. Azure's UI "Resolved" is `fixed` and its
    /// "Reactivate" is `active`, and both must round-trip through
    /// `is_resolved_status` so a resolve does not read back as still open.
    #[test]
    fn resolving_a_thread_patches_status_on_the_thread_route() {
        for (resolved, expected_status) in [(true, "fixed"), (false, "active")] {
            let discovered = format!(
                r#"[{{"area":"git","resourceName":"threads-name","routeTemplate":"{THREADS_ROUTE}/{{threadId}}"}}]"#
            );
            let runner =
                FakeRunner::with_replies([Box::leak(discovered.into_boxed_str()), "{\"id\":42}"]);
            let backend = AzureDevOpsBackend::with_runner(None, runner);

            backend
                .set_review_thread_resolved(&details(7), "42", resolved)
                .expect("PATCH must succeed");

            let calls = backend.runner.calls.lock().unwrap();
            let (args, body) = calls.last().expect("an invoke call");
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--http-method", "PATCH"]),
                "must PATCH, not POST: {args:?}"
            );
            assert!(
                args.contains(&"threadId=42".to_string()),
                "thread id must go in the route: {args:?}"
            );
            assert!(args.contains(&"pullRequestId=7".to_string()));

            let body = body.as_ref().expect("status payload sent via --in-file");
            let payload: serde_json::Value = serde_json::from_slice(body).unwrap();
            assert_eq!(payload["status"], expected_status);
            // Nothing else may be sent: a full-thread PUT would clobber comments.
            assert_eq!(
                payload.as_object().map(serde_json::Map::len),
                Some(1),
                "payload must carry only `status`: {payload}"
            );

            // The status we send must map back to the resolved flag we intended.
            assert_eq!(
                crate::forge::azure_devops::models::is_resolved_status_for_test(expected_status),
                resolved
            );
        }
    }

    /// The thread id goes straight into a route, so a non-numeric one is rejected
    /// rather than used to build a bogus request.
    #[test]
    fn non_numeric_thread_ids_are_rejected_before_any_call() {
        for bad in ["", "abc", "42x", "PRRT_kwDO", "1 2", "-1"] {
            let backend = AzureDevOpsBackend::with_runner(None, FakeRunner::default());
            let error = backend
                .set_review_thread_resolved(&details(7), bad, true)
                .expect_err("must reject");
            assert!(error.to_string().contains("not numeric"), "for {bad:?}");
            assert!(
                backend.runner.calls.lock().unwrap().is_empty(),
                "must not call az for {bad:?}"
            );
        }
    }

    /// Backends without a resolve concept must say so, not silently no-op.
    #[test]
    fn other_backends_report_resolve_as_unsupported() {
        use crate::forge::github::gh::GitHubGhBackend;
        let error = GitHubGhBackend::new(None)
            .set_review_thread_resolved(&details(7), "PRRT_abc", true)
            .expect_err("default must be unsupported");
        assert!(matches!(error, TuicrError::UnsupportedOperation(_)));
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

    /// Verbatim `routeTemplate` values from a live organization's discovery
    /// response. Azure parameterizes the area and resource segments, so raw
    /// template comparison matched none of these and discovery always failed --
    /// which `unwrap_or_default` then hid as "no commits / no threads".
    #[test]
    fn resolves_every_live_route_template_to_its_resource() {
        let live = [
            (
                COMMITS_ROUTE,
                "pullRequestCommits",
                "{project}/_apis/{area}/repositories/{repositoryId}/pullRequests/{pullRequestId}/commits",
            ),
            (
                ITERATIONS_ROUTE,
                "pullRequestIterations",
                "{project}/_apis/{area}/repositories/{repositoryId}/pullRequests/{pullRequestId}/iterations/{iterationId}",
            ),
            (
                ITERATION_CHANGES_ROUTE,
                "pullRequestIterationChanges",
                "{project}/_apis/{area}/repositories/{repositoryId}/pullRequests/{pullRequestId}/iterations/{iterationId}/changes",
            ),
            (
                THREADS_ROUTE,
                "pullRequestThreads",
                "{project}/_apis/{area}/repositories/{repositoryId}/pullRequests/{pullRequestId}/threads/{threadId}",
            ),
            (
                REVIEWERS_ROUTE,
                "pullRequestReviewers",
                "{project}/_apis/{area}/repositories/{repositoryId}/pullRequests/{pullRequestId}/reviewers/{reviewerId}",
            ),
            (
                // Uses both `{area}` and `{resource}`, plus a wildcard tail.
                ITEMS_ROUTE,
                "items",
                "{project}/_apis/{area}/repositories/{repositoryId}/{resource}/{*path}",
            ),
        ];

        for (expected_route, resource_name, template) in live {
            let location = AzureApiResourceLocation {
                id: String::new(),
                area: "git".to_string(),
                resource_name: resource_name.to_string(),
                route_template: template.to_string(),
            };
            assert!(
                route_matches(
                    &resolve_route_placeholders(&location),
                    &normalize_route(expected_route)
                ),
                "live template for `{resource_name}` must resolve `{expected_route}`"
            );
        }
    }

    /// Substitution must not let a different git resource satisfy a route.
    #[test]
    fn placeholder_substitution_does_not_create_false_matches() {
        for (resource_name, template) in [
            (
                "itemsBatch",
                "{project}/_apis/{area}/repositories/{repositoryId}/{resource}",
            ),
            (
                "hfsItems",
                "{project}/_apis/{area}/repositories/{repositoryId}/{resource}/{*path}",
            ),
            (
                "pullRequestWorkItems",
                "{project}/_apis/{area}/repositories/{repositoryId}/pullRequests/{pullRequestId}/workitems",
            ),
        ] {
            let location = AzureApiResourceLocation {
                id: String::new(),
                area: "git".to_string(),
                resource_name: resource_name.to_string(),
                route_template: template.to_string(),
            };
            let resolved = resolve_route_placeholders(&location);
            assert!(
                !route_matches(&resolved, &normalize_route(ITEMS_ROUTE)),
                "`{resource_name}` must not satisfy the items route"
            );
            assert!(
                !route_matches(&resolved, &normalize_route(COMMITS_ROUTE)),
                "`{resource_name}` must not satisfy the commits route"
            );
        }
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
                "git@ssh.dev.azure.com:v3/neutral-org/neutral-project/neutral-repo",
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
if [ "$1" = "-c" ] && [ "$2" = "credential.interactive=false" ]; then
  shift 2
fi
if [ "$1" = "cat-file" ] && [ "$3" = "missing^{commit}" ] && [ ! -f "$TUICR_GIT_STATE" ]; then
  exit 1
fi
if [ "$1" = "fetch" ]; then
  [ "$GIT_TERMINAL_PROMPT" = "0" ] || exit 99
  touch "$TUICR_GIT_STATE"
fi
if [ "$1" = "diff" ]; then
  printf ':100644 100644 aaaaaaa bbbbbbb M\000file.txt\000\000'
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

        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].display_path(), Some(Path::new("file.txt")));
        assert!(diff[0].patch.contains("diff --git a/file.txt b/file.txt"));
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
                "-c",
                "credential.interactive=false",
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "origin",
                "missing"
            ]
        );
        assert_eq!(calls[3], ["cat-file", "-e", "missing^{commit}"]);
        // Three-dot: the diff must start at the merge base, not the target tip,
        // or every commit merged into the target since the branch diverged shows
        // up as a reversed change.
        assert_eq!(
            calls[4],
            [
                "diff",
                "--no-ext-diff",
                "--raw",
                "-z",
                "--patch",
                "--binary",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "base...missing"
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
