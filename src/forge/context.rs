//! Context provider abstraction for gap expansion.
//!
//! Gap expansion needs file content at an exact revision: working tree for
//! local review, base/head SHAs for PR review. `ContextProvider` factors
//! that lookup so the App can route expansion through the right source
//! without knowing whether the diff is local or remote.
//!
//! Implementations:
//! - `VcsContextProvider`: delegates to `VcsBackend::fetch_context_lines`.
//! - `ForgeContextProvider`: builds a `ForgeFileLinesRequest` for the given
//!   file/status and delegates to `ForgeBackend::fetch_file_lines`.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::forge::traits::{ForgeBackend, ForgeFileLinesRequest, ForgeRepository, PrSessionKey};
use crate::model::{DiffLine, FileStatus};
use crate::vcs::VcsBackend;

/// Source of context lines for gap expansion. Implementations either read
/// from a local VCS working tree / blob, or fetch the exact base/head SHA
/// from a remote forge.
pub trait ContextProvider {
    /// Fetch context lines for a file in `[start_line, end_line]` inclusive.
    /// `old_path` and `new_path` come straight from the parsed diff and are
    /// used to map rename/copy sides correctly.
    fn fetch_context_lines(
        &self,
        old_path: Option<&PathBuf>,
        new_path: Option<&PathBuf>,
        file_status: FileStatus,
        start_line: u32,
        end_line: u32,
    ) -> Result<Vec<DiffLine>>;

    /// Return the total number of lines in the file at the revision used by
    /// this provider. Returns `Ok(0)` when the file cannot be found.
    fn file_line_count(
        &self,
        old_path: Option<&PathBuf>,
        new_path: Option<&PathBuf>,
        file_status: FileStatus,
    ) -> Result<u32>;

    /// Batched [`Self::file_line_count`], one output per input in order.
    ///
    /// A PR open needs a count for every changed file before the first render,
    /// so providers backed by subprocesses can collapse that into a single call.
    /// The default keeps the per-file behaviour.
    fn file_line_counts(&self, files: &[FileRevision]) -> Vec<Option<u32>> {
        files
            .iter()
            .map(|file| {
                self.file_line_count(file.old_path.as_ref(), file.new_path.as_ref(), file.status)
                    .ok()
            })
            .collect()
    }
}

/// One file's identity for a batched line-count lookup, mirroring the arguments
/// of [`ContextProvider::file_line_count`].
#[derive(Debug, Clone)]
pub struct FileRevision {
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub status: FileStatus,
}

/// Adapter over a `VcsBackend`. Picks the appropriate display path (new on
/// the head side, old on the base side) and forwards to the backend.
pub struct VcsContextProvider<'a> {
    pub vcs: &'a dyn VcsBackend,
    pub ref_commit: Option<String>,
}

impl ContextProvider for VcsContextProvider<'_> {
    fn fetch_context_lines(
        &self,
        old_path: Option<&PathBuf>,
        new_path: Option<&PathBuf>,
        file_status: FileStatus,
        start_line: u32,
        end_line: u32,
    ) -> Result<Vec<DiffLine>> {
        // Mirror the existing display_path rule: prefer new_path, fall back
        // to old_path. Local VCS backends pick the side internally based on
        // file status.
        let path: &Path = match new_path.or(old_path) {
            Some(p) => p.as_path(),
            None => return Ok(Vec::new()),
        };
        self.vcs.fetch_context_lines(
            path,
            file_status,
            self.ref_commit.as_deref(),
            start_line,
            end_line,
        )
    }

    fn file_line_count(
        &self,
        old_path: Option<&PathBuf>,
        new_path: Option<&PathBuf>,
        file_status: FileStatus,
    ) -> Result<u32> {
        let path: &Path = match new_path.or(old_path) {
            Some(p) => p.as_path(),
            None => return Ok(0),
        };
        self.vcs
            .file_line_count(path, file_status, self.ref_commit.as_deref())
    }
}

/// Adapter over a `ForgeBackend` for PR review mode.
///
/// Built from the PR's `PrSessionKey` (carries head SHA and repository) and
/// the captured `base_sha`. Each `fetch_context_lines` call constructs the
/// appropriate `ForgeFileLinesRequest` and delegates to the backend.
pub struct ForgeContextProvider<'a> {
    pub forge: &'a dyn ForgeBackend,
    pub repository: ForgeRepository,
    pub base_sha: String,
    pub head_sha: String,
}

impl<'a> ForgeContextProvider<'a> {
    pub fn for_pr(
        forge: &'a dyn ForgeBackend,
        key: &PrSessionKey,
        base_sha: impl Into<String>,
    ) -> Self {
        Self {
            forge,
            repository: key.repository.clone(),
            base_sha: base_sha.into(),
            head_sha: key.head_sha.clone(),
        }
    }
}

impl ContextProvider for ForgeContextProvider<'_> {
    fn fetch_context_lines(
        &self,
        old_path: Option<&PathBuf>,
        new_path: Option<&PathBuf>,
        file_status: FileStatus,
        start_line: u32,
        end_line: u32,
    ) -> Result<Vec<DiffLine>> {
        let side = ForgeFileLinesRequest::side_for_status(file_status);
        let Some(path) = ForgeFileLinesRequest::path_for_side(side, old_path, new_path) else {
            return Ok(Vec::new());
        };
        let request = ForgeFileLinesRequest {
            repository: self.repository.clone(),
            base_sha: self.base_sha.clone(),
            head_sha: self.head_sha.clone(),
            path,
            status: file_status,
            side,
            start_line,
            end_line,
        };
        self.forge.fetch_file_lines(request)
    }

    fn file_line_count(
        &self,
        old_path: Option<&PathBuf>,
        new_path: Option<&PathBuf>,
        file_status: FileStatus,
    ) -> Result<u32> {
        let side = ForgeFileLinesRequest::side_for_status(file_status);
        let Some(path) = ForgeFileLinesRequest::path_for_side(side, old_path, new_path) else {
            return Ok(0);
        };
        let request = ForgeFileLinesRequest {
            repository: self.repository.clone(),
            base_sha: self.base_sha.clone(),
            head_sha: self.head_sha.clone(),
            path,
            status: file_status,
            side,
            start_line: 0,
            end_line: 0,
        };
        self.forge.file_line_count(request)
    }

    fn file_line_counts(&self, files: &[FileRevision]) -> Vec<Option<u32>> {
        // Files with no usable path on the relevant side get `Some(0)` here so
        // the result stays index-aligned with `files`, matching what
        // `file_line_count` returns for them.
        let mut requests = Vec::with_capacity(files.len());
        let mut slots = Vec::with_capacity(files.len());
        for file in files {
            let side = ForgeFileLinesRequest::side_for_status(file.status);
            match ForgeFileLinesRequest::path_for_side(
                side,
                file.old_path.as_ref(),
                file.new_path.as_ref(),
            ) {
                Some(path) => {
                    slots.push(Some(requests.len()));
                    requests.push(ForgeFileLinesRequest {
                        repository: self.repository.clone(),
                        base_sha: self.base_sha.clone(),
                        head_sha: self.head_sha.clone(),
                        path,
                        status: file.status,
                        side,
                        start_line: 0,
                        end_line: 0,
                    });
                }
                None => slots.push(None),
            }
        }

        let counts = self.forge.file_line_counts(requests);
        slots
            .into_iter()
            .map(|slot| match slot {
                Some(index) => counts.get(index).copied().flatten(),
                None => Some(0),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::traits::{
        ForgeFileSide, PagedPullRequests, PullRequestDetails, PullRequestListQuery,
        PullRequestTarget,
    };
    use crate::model::{FilePatch, LineOrigin};
    use std::cell::RefCell;

    struct CapturingForge {
        seen: RefCell<Vec<ForgeFileLinesRequest>>,
        response: Vec<DiffLine>,
    }

    impl ForgeBackend for CapturingForge {
        fn list_pull_requests(&self, _query: PullRequestListQuery) -> Result<PagedPullRequests> {
            unimplemented!()
        }
        fn get_pull_request(&self, _target: PullRequestTarget) -> Result<PullRequestDetails> {
            unimplemented!()
        }
        fn get_pull_request_diff(
            &self,
            _pr: &PullRequestDetails,
        ) -> Result<Vec<crate::model::FilePatch>> {
            unimplemented!()
        }
        fn fetch_file_lines(&self, request: ForgeFileLinesRequest) -> Result<Vec<DiffLine>> {
            self.seen.borrow_mut().push(request);
            Ok(self.response.clone())
        }
        fn list_review_threads(
            &self,
            _pr: &PullRequestDetails,
        ) -> Result<Vec<crate::forge::remote_comments::RemoteReviewThread>> {
            Ok(Vec::new())
        }
        fn list_pull_request_commits(
            &self,
            _pr: &PullRequestDetails,
        ) -> Result<Vec<crate::forge::traits::PullRequestCommit>> {
            Ok(Vec::new())
        }
        fn get_pull_request_commit_range_diff(
            &self,
            _pr: &PullRequestDetails,
            _start_sha: &str,
            _end_sha: &str,
        ) -> Result<Vec<crate::model::FilePatch>> {
            unimplemented!()
        }
        fn create_review(
            &self,
            _pr: &PullRequestDetails,
            _request: crate::forge::traits::CreateReviewRequest<'_>,
        ) -> Result<crate::forge::traits::GhCreateReviewResponse> {
            unimplemented!()
        }
    }

    /// Records the batch it receives and answers with each request's index, so
    /// misalignment between inputs and outputs is detectable.
    struct BatchingForge {
        batch_sizes: RefCell<Vec<usize>>,
    }

    impl ForgeBackend for BatchingForge {
        fn list_pull_requests(&self, _query: PullRequestListQuery) -> Result<PagedPullRequests> {
            unimplemented!()
        }
        fn get_pull_request(&self, _target: PullRequestTarget) -> Result<PullRequestDetails> {
            unimplemented!()
        }
        fn get_pull_request_diff(&self, _pr: &PullRequestDetails) -> Result<Vec<FilePatch>> {
            unimplemented!()
        }
        fn fetch_file_lines(&self, _request: ForgeFileLinesRequest) -> Result<Vec<DiffLine>> {
            unimplemented!()
        }
        fn file_line_counts(&self, requests: Vec<ForgeFileLinesRequest>) -> Vec<Option<u32>> {
            self.batch_sizes.borrow_mut().push(requests.len());
            (0..requests.len())
                .map(|index| u32::try_from(index + 100).ok())
                .collect()
        }
        fn list_review_threads(
            &self,
            _pr: &PullRequestDetails,
        ) -> Result<Vec<crate::forge::remote_comments::RemoteReviewThread>> {
            Ok(Vec::new())
        }
        fn list_pull_request_commits(
            &self,
            _pr: &PullRequestDetails,
        ) -> Result<Vec<crate::forge::traits::PullRequestCommit>> {
            Ok(Vec::new())
        }
        fn get_pull_request_commit_range_diff(
            &self,
            _pr: &PullRequestDetails,
            _start_sha: &str,
            _end_sha: &str,
        ) -> Result<Vec<FilePatch>> {
            unimplemented!()
        }
        fn create_review(
            &self,
            _pr: &PullRequestDetails,
            _request: crate::forge::traits::CreateReviewRequest<'_>,
        ) -> Result<crate::forge::traits::GhCreateReviewResponse> {
            unimplemented!()
        }
    }

    /// Files with no path on the relevant side are not sent to the backend, but
    /// must still occupy their slot, or every later file gets another file's count.
    #[test]
    fn batched_counts_stay_aligned_when_some_files_have_no_path() {
        let forge = BatchingForge {
            batch_sizes: RefCell::new(Vec::new()),
        };
        let provider = ForgeContextProvider::for_pr(&forge, &key(), "basesha");
        let files = vec![
            FileRevision {
                old_path: None,
                new_path: Some(PathBuf::from("a.rs")),
                status: FileStatus::Modified,
            },
            FileRevision {
                old_path: None,
                new_path: None,
                status: FileStatus::Modified,
            },
            FileRevision {
                old_path: None,
                new_path: Some(PathBuf::from("b.rs")),
                status: FileStatus::Modified,
            },
        ];

        let counts = provider.file_line_counts(&files);

        assert_eq!(
            *forge.batch_sizes.borrow(),
            vec![2],
            "one batched call, skipping the pathless file"
        );
        assert_eq!(counts, vec![Some(100), Some(0), Some(101)]);
    }

    /// The default trait implementation must keep the documented per-file
    /// behaviour for backends that do not batch.
    #[test]
    fn default_batch_falls_back_to_per_file_counts() {
        let forge = CapturingForge {
            seen: RefCell::new(Vec::new()),
            response: Vec::new(),
        };
        let provider = ForgeContextProvider::for_pr(&forge, &key(), "basesha");
        let counts = provider.file_line_counts(&[FileRevision {
            old_path: None,
            new_path: Some(PathBuf::from("a.rs")),
            status: FileStatus::Modified,
        }]);
        // `CapturingForge` does not override `file_line_count`, whose default is 0.
        assert_eq!(counts, vec![Some(0)]);
    }

    fn repo() -> ForgeRepository {
        ForgeRepository::github("github.com", "agavra", "tuicr")
    }

    fn key() -> PrSessionKey {
        PrSessionKey::new(repo(), 125, "headsha".to_string())
    }

    fn make_line(text: &str) -> DiffLine {
        DiffLine {
            origin: LineOrigin::Context,
            content: text.to_string(),
            old_lineno: Some(1),
            new_lineno: Some(1),
            highlighted_spans: None,
        }
    }

    #[test]
    fn should_use_head_side_for_modified_file() {
        // given
        let forge = CapturingForge {
            seen: RefCell::new(Vec::new()),
            response: vec![make_line("a")],
        };
        let provider = ForgeContextProvider::for_pr(&forge, &key(), "basesha");
        let new_path = PathBuf::from("src/lib.rs");
        // when
        let _ = provider
            .fetch_context_lines(None, Some(&new_path), FileStatus::Modified, 1, 1)
            .unwrap();
        // then
        let seen = forge.seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].side, ForgeFileSide::Head);
        assert_eq!(seen[0].sha(), "headsha");
        assert_eq!(seen[0].path, PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn should_use_base_side_for_deleted_file() {
        // given
        let forge = CapturingForge {
            seen: RefCell::new(Vec::new()),
            response: vec![],
        };
        let provider = ForgeContextProvider::for_pr(&forge, &key(), "basesha");
        let old_path = PathBuf::from("src/gone.rs");
        // when
        let _ = provider
            .fetch_context_lines(Some(&old_path), None, FileStatus::Deleted, 1, 1)
            .unwrap();
        // then
        let seen = forge.seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].side, ForgeFileSide::Base);
        assert_eq!(seen[0].sha(), "basesha");
        assert_eq!(seen[0].path, PathBuf::from("src/gone.rs"));
    }

    #[test]
    fn should_pick_old_path_for_rename_base_side() {
        // when called for a renamed file's base side (we'd never call that in
        // expansion today, but the helper must do it), the old path wins.
        let old = PathBuf::from("old.rs");
        let new = PathBuf::from("new.rs");
        assert_eq!(
            ForgeFileLinesRequest::path_for_side(ForgeFileSide::Base, Some(&old), Some(&new),),
            Some(old.clone()),
        );
        assert_eq!(
            ForgeFileLinesRequest::path_for_side(ForgeFileSide::Head, Some(&old), Some(&new),),
            Some(new),
        );
    }

    #[test]
    fn should_short_circuit_when_path_missing() {
        // given a file with no paths at all (degenerate case)
        let forge = CapturingForge {
            seen: RefCell::new(Vec::new()),
            response: vec![make_line("x")],
        };
        let provider = ForgeContextProvider::for_pr(&forge, &key(), "basesha");
        // when
        let result = provider
            .fetch_context_lines(None, None, FileStatus::Modified, 1, 5)
            .unwrap();
        // then — empty result, no backend call
        assert!(result.is_empty());
        assert!(forge.seen.borrow().is_empty());
    }
}
