use std::collections::HashMap;

use serde::Serialize;

use crate::error::{Result, TuicrError};
use crate::forge::azure_devops::az::AzCommandRunner;
use crate::forge::azure_devops::models::{AzureIterationChange, AzureThread};
use crate::forge::submit::{GhSide, InlineComment, SubmitEvent};
use crate::forge::traits::{
    CreateReviewRequest, GhCreateReviewResponse, PartialReviewSubmission, PostedReviewComment,
    PullRequestDetails, ReviewSubmissionFailure, ReviewSubmissionOutcome,
};

use super::{AzureDevOpsBackend, THREADS_ROUTE};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureThreadCreateRequest<'a> {
    comments: Vec<AzureCommentCreateRequest<'a>>,
    status: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_context: Option<AzureThreadCreateContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pull_request_thread_context: Option<AzurePullRequestThreadCreateContext>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureCommentCreateRequest<'a> {
    parent_comment_id: u64,
    content: &'a str,
    comment_type: u8,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureThreadCreateContext {
    file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    left_file_start: Option<AzureCommentPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    left_file_end: Option<AzureCommentPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    right_file_start: Option<AzureCommentPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    right_file_end: Option<AzureCommentPosition>,
}

#[derive(Debug, Clone, Serialize)]
struct AzureCommentPosition {
    line: u32,
    offset: u32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AzurePullRequestThreadCreateContext {
    change_tracking_id: u32,
    iteration_context: AzureIterationContext,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AzureIterationContext {
    first_comparing_iteration: u32,
    second_comparing_iteration: u32,
}

struct PreparedComment {
    local_id: String,
    body: Vec<u8>,
}

pub(super) fn create_review<R>(
    backend: &AzureDevOpsBackend<R>,
    pr: &PullRequestDetails,
    request: CreateReviewRequest<'_>,
) -> Result<GhCreateReviewResponse>
where
    R: AzCommandRunner,
{
    let event = request.event;
    let outcome = create_review_detailed(backend, pr, request)?;
    let ReviewSubmissionOutcome::NonAtomic(result) = outcome else {
        unreachable!("Azure DevOps submissions are non-atomic")
    };
    if !result.failures.is_empty() {
        let successful_ids = result
            .posted_comments
            .iter()
            .map(|comment| comment.local_comment_id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(TuicrError::Forge(format!(
            "Azure DevOps review was only partially submitted: {} comment(s) posted{}; {} operation(s) failed: {}",
            result.posted_comments.len(),
            if successful_ids.is_empty() {
                String::new()
            } else {
                format!(" (local IDs: {successful_ids})")
            },
            result.failures.len(),
            result
                .failures
                .iter()
                .map(|failure| format!("{}: {}", failure.operation, failure.message))
                .collect::<Vec<_>>()
                .join("; ")
        )));
    }

    let id = result
        .posted_comments
        .first()
        .and_then(|comment| comment.remote_thread_id.parse().ok())
        .unwrap_or(0);
    Ok(GhCreateReviewResponse {
        id,
        html_url: result.html_url,
        state: match event {
            SubmitEvent::Approve => "APPROVED",
            SubmitEvent::RequestChanges => "CHANGES_REQUESTED",
            SubmitEvent::Comment => "COMMENTED",
            SubmitEvent::Draft => unreachable!("draft rejected before submission"),
        }
        .to_string(),
    })
}

pub(super) fn create_review_detailed<R>(
    backend: &AzureDevOpsBackend<R>,
    pr: &PullRequestDetails,
    request: CreateReviewRequest<'_>,
) -> Result<ReviewSubmissionOutcome>
where
    R: AzCommandRunner,
{
    if request.event == SubmitEvent::Draft {
        return Err(TuicrError::UnsupportedOperation(
            "Azure DevOps does not provide a pending-review draft primitive; submit Comment, Approve, or Request changes instead"
                .to_string(),
        ));
    }

    let current = backend.refetch_pull_request(pr)?;
    if current.head_sha != pr.head_sha {
        return Err(TuicrError::Forge(format!(
            "Azure DevOps pull request {} advanced from `{}` to `{}`; reload before submitting",
            pr.number, pr.head_sha, current.head_sha
        )));
    }

    // Resolve and serialize every request before the first write. Azure PRs
    // that support iterations require changeTrackingId and iterationContext;
    // including them for all inline/file threads is also valid for first-
    // iteration PRs and avoids a race between capability lookup and posting.
    let prepared = prepare_comments(backend, pr, request.comments)?;
    let summary = (!request.body.trim().is_empty())
        .then(|| summary_request(request.body))
        .map(|body| serde_json::to_vec(&body))
        .transpose()?;

    let mut result = PartialReviewSubmission {
        posted_comments: Vec::new(),
        summary_posted: false,
        vote_applied: false,
        failures: Vec::new(),
        html_url: pr.url.clone(),
    };

    if let Some(summary) = summary {
        match post_thread(backend, pr, &summary) {
            Ok(_) => result.summary_posted = true,
            Err(error) => {
                result.failures.push(ReviewSubmissionFailure {
                    local_comment_id: None,
                    operation: "summary".to_string(),
                    message: error.to_string(),
                });
                return Ok(ReviewSubmissionOutcome::NonAtomic(result));
            }
        }
    }

    for comment in prepared {
        match post_thread(backend, pr, &comment.body) {
            Ok(thread) => {
                result.posted_comments.push(PostedReviewComment {
                    local_comment_id: comment.local_id,
                    remote_thread_id: thread.id.to_string(),
                    remote_comment_id: thread
                        .comments
                        .first()
                        .map(|comment| comment.id.to_string()),
                });
            }
            Err(error) => {
                result.failures.push(ReviewSubmissionFailure {
                    local_comment_id: Some(comment.local_id),
                    operation: "comment".to_string(),
                    message: error.to_string(),
                });
                // Independent calls have no transaction or idempotency key.
                // Stop so the returned successful local IDs form an exact,
                // unambiguous retry boundary.
                return Ok(ReviewSubmissionOutcome::NonAtomic(result));
            }
        }
    }

    if let Some(vote) = vote_for_event(request.event) {
        if let Err(error) = set_vote(backend, pr, vote) {
            result.failures.push(ReviewSubmissionFailure {
                local_comment_id: None,
                operation: "vote".to_string(),
                message: error.to_string(),
            });
        } else {
            result.vote_applied = true;
        }
    }

    Ok(ReviewSubmissionOutcome::NonAtomic(result))
}

fn prepare_comments<R>(
    backend: &AzureDevOpsBackend<R>,
    pr: &PullRequestDetails,
    comments: &[InlineComment],
) -> Result<Vec<PreparedComment>>
where
    R: AzCommandRunner,
{
    if comments.is_empty() {
        return Ok(Vec::new());
    }
    let iteration = backend.latest_iteration(pr)?;
    let changes = backend.list_iteration_changes(pr, iteration.id)?;
    let tracking_ids = change_tracking_ids(changes);
    comments
        .iter()
        .map(|comment| {
            let path = normalized_path(&comment.path.to_string_lossy());
            let old_path = comment
                .old_path
                .as_ref()
                .map(|path| normalized_path(&path.to_string_lossy()));
            let change_tracking_id = tracking_ids
                .get(&path)
                .or_else(|| old_path.as_ref().and_then(|path| tracking_ids.get(path)))
                .copied()
                .ok_or_else(|| {
                    TuicrError::Forge(format!(
                        "Azure DevOps latest iteration {} has no changeTrackingId for `{path}`",
                        iteration.id
                    ))
                })?;
            let body = inline_request(comment, iteration.id, change_tracking_id)?;
            Ok(PreparedComment {
                local_id: comment.comment_id.clone(),
                body: serde_json::to_vec(&body)?,
            })
        })
        .collect()
}

fn change_tracking_ids(changes: Vec<AzureIterationChange>) -> HashMap<String, u32> {
    changes
        .into_iter()
        .filter_map(|change| {
            let path = normalized_path(change.item?.path?.as_str());
            (!path.is_empty()).then_some((path, change.change_tracking_id))
        })
        .collect()
}

fn summary_request(body: &str) -> AzureThreadCreateRequest<'_> {
    AzureThreadCreateRequest {
        comments: vec![comment_request(body)],
        status: 1,
        thread_context: None,
        pull_request_thread_context: None,
    }
}

fn inline_request(
    comment: &InlineComment,
    iteration_id: u32,
    change_tracking_id: u32,
) -> Result<AzureThreadCreateRequest<'_>> {
    let selected_path = if comment.side == GhSide::Left {
        comment.old_path.as_ref().unwrap_or(&comment.path)
    } else {
        &comment.path
    };
    let path = format!("/{}", normalized_path(&selected_path.to_string_lossy()));
    let thread_context = if comment.file_level {
        // Azure's REST thread model supports filePath-only contexts, although
        // Microsoft does not publish an official create-thread request example
        // for a file-only comment. This is preferable to inventing a line.
        AzureThreadCreateContext {
            file_path: path,
            left_file_start: None,
            left_file_end: None,
            right_file_start: None,
            right_file_end: None,
        }
    } else {
        let start_side = comment.start_side.unwrap_or(comment.side);
        if start_side != comment.side {
            return Err(TuicrError::Forge(format!(
                "Azure DevOps cannot anchor mixed-side range comment `{}`",
                comment.comment_id
            )));
        }
        let start = AzureCommentPosition {
            line: comment.start_line.unwrap_or(comment.line),
            offset: 1,
        };
        let end = AzureCommentPosition {
            line: comment.line,
            offset: 1,
        };
        match comment.side {
            GhSide::Left => AzureThreadCreateContext {
                file_path: path,
                left_file_start: Some(start),
                left_file_end: Some(end),
                right_file_start: None,
                right_file_end: None,
            },
            GhSide::Right => AzureThreadCreateContext {
                file_path: path,
                left_file_start: None,
                left_file_end: None,
                right_file_start: Some(start),
                right_file_end: Some(end),
            },
        }
    };
    Ok(AzureThreadCreateRequest {
        comments: vec![comment_request(&comment.body)],
        status: 1,
        thread_context: Some(thread_context),
        pull_request_thread_context: Some(AzurePullRequestThreadCreateContext {
            change_tracking_id,
            iteration_context: AzureIterationContext {
                first_comparing_iteration: 1,
                second_comparing_iteration: iteration_id,
            },
        }),
    })
}

fn comment_request(body: &str) -> AzureCommentCreateRequest<'_> {
    AzureCommentCreateRequest {
        parent_comment_id: 0,
        content: body,
        comment_type: 1,
    }
}

/// Normalize a path for exact-match lookups against Azure's iteration changes.
///
/// Trims surrounding whitespace as well: both sides of the comparison must agree
/// exactly, and a stray delimiter tab from a diff header is invisible in any
/// error message it causes.
fn normalized_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim()
        .trim_start_matches('/')
        .to_string()
}

fn post_thread<R>(
    backend: &AzureDevOpsBackend<R>,
    pr: &PullRequestDetails,
    body: &[u8],
) -> Result<AzureThread>
where
    R: AzCommandRunner,
{
    let identity = AzureDevOpsBackend::<R>::identity(&pr.repository)?;
    let route = vec![
        ("project", identity.project.clone()),
        (
            "repositoryId",
            AzureDevOpsBackend::<R>::repository_id(&pr.repository)?.to_string(),
        ),
        ("pullRequestId", pr.number.to_string()),
    ];
    let output = backend.invoke(
        &pr.repository,
        THREADS_ROUTE,
        Some("POST"),
        &route,
        &[],
        Some(body),
    )?;
    let thread: AzureThread = serde_json::from_str(&output)?;
    if thread.id == 0 {
        return Err(TuicrError::Forge(
            "Azure DevOps create-thread response omitted the remote thread ID".to_string(),
        ));
    }
    Ok(thread)
}

fn vote_for_event(event: SubmitEvent) -> Option<&'static str> {
    match event {
        SubmitEvent::Comment => None,
        SubmitEvent::Approve => Some("approve"),
        SubmitEvent::RequestChanges => Some("wait-for-author"),
        SubmitEvent::Draft => None,
    }
}

fn set_vote<R>(backend: &AzureDevOpsBackend<R>, pr: &PullRequestDetails, vote: &str) -> Result<()>
where
    R: AzCommandRunner,
{
    let identity = AzureDevOpsBackend::<R>::identity(&pr.repository)?;
    backend.run_az(vec![
        "repos".to_string(),
        "pr".to_string(),
        "set-vote".to_string(),
        "--id".to_string(),
        pr.number.to_string(),
        "--vote".to_string(),
        vote.to_string(),
        "--organization".to_string(),
        identity.organization_url.clone(),
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::forge::azure_devops::az::{AzCommandError, AzCommandResult};
    use crate::forge::traits::{ForgeBackend, ForgeRepository};

    type RecordedCall = (Vec<String>, Option<Vec<u8>>);

    struct FakeRunner {
        replies: Mutex<VecDeque<AzCommandResult<String>>>,
        calls: Mutex<Vec<RecordedCall>>,
    }

    impl FakeRunner {
        fn new(replies: Vec<AzCommandResult<String>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl AzCommandRunner for FakeRunner {
        fn run(&self, args: &[String]) -> AzCommandResult<String> {
            self.calls.lock().unwrap().push((args.to_vec(), None));
            self.replies.lock().unwrap().pop_front().unwrap()
        }

        fn run_with_input_file(&self, args: &[String], input: &[u8]) -> AzCommandResult<String> {
            self.calls
                .lock()
                .unwrap()
                .push((args.to_vec(), Some(input.to_vec())));
            self.replies.lock().unwrap().pop_front().unwrap()
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

    fn pr() -> PullRequestDetails {
        PullRequestDetails {
            repository: repository(),
            number: 11,
            title: "Review".to_string(),
            url:
                "https://dev.azure.com/neutral-org/neutral-project/_git/neutral-repo/pullrequest/11"
                    .to_string(),
            state: "OPEN".to_string(),
            is_draft: false,
            author: None,
            head_ref_name: "topic".to_string(),
            base_ref_name: "main".to_string(),
            head_sha: "head-sha".to_string(),
            base_sha: "base-sha".to_string(),
            body: String::new(),
            updated_at: None,
            closed: false,
            merged_at: None,
            diff_start_sha: None,
        }
    }

    fn show(head: &str) -> String {
        format!(
            r#"{{"pullRequestId":11,"title":"Review","status":"active","sourceRefName":"refs/heads/topic","targetRefName":"refs/heads/main","lastMergeSourceCommit":{{"commitId":"{head}"}},"lastMergeTargetCommit":{{"commitId":"base-sha"}},"repository":{{"id":"repo-id"}}}}"#
        )
    }

    fn discovery(route: &str, resource: &str) -> String {
        format!(r#"[{{"area":"git","resourceName":"{resource}","routeTemplate":"{route}"}}]"#)
    }

    fn comment(id: &str) -> InlineComment {
        InlineComment {
            path: PathBuf::from("src/lib.rs"),
            line: 9,
            side: GhSide::Right,
            counterpart_line: None,
            start_line: Some(7),
            start_side: Some(GhSide::Right),
            old_path: None,
            file_level: false,
            body: "comment".to_string(),
            comment_id: id.to_string(),
        }
    }

    fn ok(value: String) -> AzCommandResult<String> {
        Ok(value)
    }

    #[test]
    fn serializes_official_thread_constants_and_right_range() {
        let comment = comment("local-1");
        let request = inline_request(&comment, 3, 42).unwrap();
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["comments"][0]["commentType"], 1);
        assert_eq!(value["status"], 1);
        assert_eq!(value["threadContext"]["filePath"], "/src/lib.rs");
        assert_eq!(value["threadContext"]["rightFileStart"]["line"], 7);
        assert_eq!(value["threadContext"]["rightFileEnd"]["line"], 9);
        assert!(value["threadContext"].get("leftFileStart").is_none());
        assert_eq!(value["pullRequestThreadContext"]["changeTrackingId"], 42);
        assert_eq!(
            value["pullRequestThreadContext"]["iterationContext"]["secondComparingIteration"],
            3
        );
    }

    #[test]
    fn serializes_file_marker_as_file_path_only() {
        let mut file = comment("file");
        file.body = "[NOTE] File-level: consider this file".to_string();
        file.file_level = true;
        file.start_line = None;
        let value = serde_json::to_value(inline_request(&file, 1, 2).unwrap()).unwrap();
        let context = &value["threadContext"];
        assert_eq!(context["filePath"], "/src/lib.rs");
        assert!(context.get("rightFileStart").is_none());
        assert!(context.get("leftFileStart").is_none());
    }

    #[test]
    fn rejects_draft_and_stale_head_before_writes() {
        let draft_runner = FakeRunner::new(vec![]);
        let draft_backend = AzureDevOpsBackend::with_runner(None, draft_runner);
        let error = draft_backend
            .create_review_detailed(
                &pr(),
                CreateReviewRequest {
                    event: SubmitEvent::Draft,
                    commit_id: "head-sha",
                    body: "",
                    comments: &[],
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("pending-review"));
        assert!(draft_backend.runner.calls.lock().unwrap().is_empty());

        let stale_runner = FakeRunner::new(vec![ok(show("new-head"))]);
        let stale_backend = AzureDevOpsBackend::with_runner(None, stale_runner);
        let error = stale_backend
            .create_review_detailed(
                &pr(),
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: "head-sha",
                    body: "summary",
                    comments: &[],
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("advanced"));
        assert_eq!(stale_backend.runner.calls.lock().unwrap().len(), 1);
    }

    /// Regression: a filename containing a space made Git append a delimiter tab
    /// in the diff header, which used to survive into the comment path and miss
    /// the `changeTrackingId` map, failing the whole submit. Both the parser fix
    /// and `normalized_path` trimming must keep this working.
    #[test]
    fn resolves_change_tracking_id_for_paths_containing_spaces() {
        let path = "packages/fortress/deep-research-report (28).md";
        let runner = FakeRunner::new(vec![
            ok(show("head-sha")),
            ok(discovery(super::super::ITERATIONS_ROUTE, "iterations")),
            ok(r#"[{"id":2}]"#.to_string()),
            ok(discovery(super::super::ITERATION_CHANGES_ROUTE, "changes")),
            ok(format!(
                r#"{{"changeEntries":[{{"changeTrackingId":4,"item":{{"path":"/{path}"}}}}]}}"#
            )),
            ok(discovery(super::super::THREADS_ROUTE, "threads")),
            ok(r#"{"id":70,"comments":[{"id":700}]}"#.to_string()),
        ]);
        let backend = AzureDevOpsBackend::with_runner(None, runner);
        let mut inline = comment("local-1");
        inline.path = PathBuf::from(path);
        let comments = [inline];

        let outcome = backend
            .create_review_detailed(
                &pr(),
                CreateReviewRequest {
                    event: SubmitEvent::Comment,
                    commit_id: "head-sha",
                    body: "",
                    comments: &comments,
                },
            )
            .expect("submit must not fail on a path containing a space");

        let ReviewSubmissionOutcome::NonAtomic(result) = outcome else {
            panic!("non-atomic result")
        };
        assert!(
            result.failures.is_empty(),
            "failures: {:?}",
            result.failures
        );
        assert_eq!(result.posted_comments.len(), 1);

        // The posted thread must carry the resolved tracking id and the exact path.
        let calls = backend.runner.calls.lock().unwrap();
        let body = calls
            .iter()
            .filter_map(|call| call.1.as_ref())
            .next_back()
            .expect("a thread payload");
        let value: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(value["pullRequestThreadContext"]["changeTrackingId"], 4);
        assert_eq!(value["threadContext"]["filePath"], format!("/{path}"));
    }

    /// A tab that somehow reaches the submit layer must not defeat the lookup.
    #[test]
    fn normalized_path_trims_delimiter_whitespace() {
        assert_eq!(
            normalized_path("/packages/fortress/deep-research-report (28).md\t"),
            "packages/fortress/deep-research-report (28).md"
        );
        assert_eq!(normalized_path("\\a\\b.txt"), "a/b.txt");
        // Interior spaces are part of the name and must survive.
        assert_eq!(normalized_path("/my file.txt"), "my file.txt");
    }

    #[test]
    fn summary_failure_stops_before_comments_or_vote() {
        let runner = FakeRunner::new(vec![
            ok(show("head-sha")),
            ok(discovery(super::super::ITERATIONS_ROUTE, "iterations")),
            ok(r#"[{"id":2}]"#.to_string()),
            ok(discovery(super::super::ITERATION_CHANGES_ROUTE, "changes")),
            ok(
                r#"{"changeEntries":[{"changeTrackingId":4,"item":{"path":"/src/lib.rs"}}]}"#
                    .to_string(),
            ),
            ok(discovery(super::super::THREADS_ROUTE, "threads")),
            Err(AzCommandError::Failed {
                status: Some(1),
                output: "summary failed".to_string(),
            }),
        ]);
        let backend = AzureDevOpsBackend::with_runner(None, runner);
        let comments = [comment("local-1")];
        let outcome = backend
            .create_review_detailed(
                &pr(),
                CreateReviewRequest {
                    event: SubmitEvent::Approve,
                    commit_id: "head-sha",
                    body: "summary",
                    comments: &comments,
                },
            )
            .unwrap();
        let ReviewSubmissionOutcome::NonAtomic(result) = outcome else {
            panic!("non-atomic result")
        };
        assert!(!result.summary_posted);
        assert!(result.posted_comments.is_empty());
        assert_eq!(result.failures[0].operation, "summary");
        assert_eq!(backend.runner.calls.lock().unwrap().len(), 7);
    }

    #[test]
    fn mid_comment_failure_preserves_successful_local_ids_and_stops() {
        let runner = FakeRunner::new(vec![
            ok(show("head-sha")),
            ok(discovery(super::super::ITERATIONS_ROUTE, "iterations")),
            ok(r#"[{"id":2}]"#.to_string()),
            ok(discovery(super::super::ITERATION_CHANGES_ROUTE, "changes")),
            ok(
                r#"{"changeEntries":[{"changeTrackingId":4,"item":{"path":"/src/lib.rs"}}]}"#
                    .to_string(),
            ),
            ok(discovery(super::super::THREADS_ROUTE, "threads")),
            ok(r#"{"id":70,"comments":[{"id":700}]}"#.to_string()),
            ok(discovery(super::super::THREADS_ROUTE, "threads")),
            Err(AzCommandError::Failed {
                status: Some(1),
                output: "second failed".to_string(),
            }),
        ]);
        let backend = AzureDevOpsBackend::with_runner(None, runner);
        let comments = [comment("local-1"), comment("local-2"), comment("local-3")];
        let outcome = backend
            .create_review_detailed(
                &pr(),
                CreateReviewRequest {
                    event: SubmitEvent::Approve,
                    commit_id: "head-sha",
                    body: "",
                    comments: &comments,
                },
            )
            .unwrap();
        let ReviewSubmissionOutcome::NonAtomic(result) = outcome else {
            panic!("non-atomic result")
        };
        assert_eq!(result.posted_comments.len(), 1);
        assert_eq!(result.posted_comments[0].local_comment_id, "local-1");
        assert_eq!(result.posted_comments[0].remote_thread_id, "70");
        assert_eq!(
            result.posted_comments[0].remote_comment_id.as_deref(),
            Some("700")
        );
        assert_eq!(
            result.failures[0].local_comment_id.as_deref(),
            Some("local-2")
        );
        assert!(!result.vote_applied);
        let calls = backend.runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 9);
        assert_eq!(calls.iter().filter(|call| call.1.is_some()).count(), 2);
        assert!(calls.iter().filter_map(|call| call.1.as_ref()).all(|body| {
            let value: serde_json::Value = serde_json::from_slice(body).unwrap();
            value["comments"][0]["commentType"] == 1 && value["status"] == 1
        }));
    }

    #[test]
    fn vote_is_applied_last_and_vote_failure_is_partial() {
        let runner = FakeRunner::new(vec![
            ok(show("head-sha")),
            ok(discovery(super::super::ITERATIONS_ROUTE, "iterations")),
            ok(r#"[{"id":2}]"#.to_string()),
            ok(discovery(super::super::ITERATION_CHANGES_ROUTE, "changes")),
            ok(
                r#"{"changeEntries":[{"changeTrackingId":4,"item":{"path":"/src/lib.rs"}}]}"#
                    .to_string(),
            ),
            ok(discovery(super::super::THREADS_ROUTE, "threads")),
            ok(r#"{"id":70,"comments":[{"id":700}]}"#.to_string()),
            Err(AzCommandError::Failed {
                status: Some(1),
                output: "vote failed".to_string(),
            }),
        ]);
        let backend = AzureDevOpsBackend::with_runner(None, runner);
        let comments = [comment("local-1")];
        let outcome = backend
            .create_review_detailed(
                &pr(),
                CreateReviewRequest {
                    event: SubmitEvent::RequestChanges,
                    commit_id: "head-sha",
                    body: "",
                    comments: &comments,
                },
            )
            .unwrap();
        let ReviewSubmissionOutcome::NonAtomic(result) = outcome else {
            panic!("non-atomic result")
        };
        assert_eq!(result.posted_comments[0].local_comment_id, "local-1");
        assert_eq!(result.failures[0].operation, "vote");
        let calls = backend.runner.calls.lock().unwrap();
        assert_eq!(
            calls.last().unwrap().0,
            [
                "repos",
                "pr",
                "set-vote",
                "--id",
                "11",
                "--vote",
                "wait-for-author",
                "--organization",
                "https://dev.azure.com/neutral-org"
            ]
        );
    }
}
