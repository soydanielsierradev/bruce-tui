//! Read-only git data for the Git pane, gathered with `git2` (libgit2).
//!
//! [`load`] never panics and never unwraps: any failure collapses into a
//! [`GitView`] variant the UI can render. A live file-watcher refresh lands in
//! step 6; for now the data is a snapshot taken when the workspace opens.

use std::path::Path;

use git2::{ErrorCode, Repository, Status, StatusOptions};

/// How many changed files / commits to keep; the rest is summarised as a count.
const MAX_FILES: usize = 12;
const MAX_COMMITS: usize = 8;

/// What the Git pane should display.
pub enum GitView {
    /// A repository was found and read.
    Repo(GitInfo),
    /// The path is not inside a git repository.
    NotARepo,
    /// libgit2 reported an error while reading the repository.
    Error(String),
}

/// A snapshot of the interesting bits of a repository's state.
pub struct GitInfo {
    /// Current branch shorthand (or a marker for detached / unborn HEAD).
    pub branch: String,
    /// Changed files, capped at [`MAX_FILES`].
    pub files: Vec<FileStatus>,
    /// Changed files beyond the cap.
    pub extra_files: usize,
    /// Most recent commits, newest first, capped at [`MAX_COMMITS`].
    pub commits: Vec<CommitInfo>,
}

/// One changed file in the working tree or index.
pub struct FileStatus {
    /// Single-character status mark (`M`, `A`, `D`, `R`, `?`).
    pub mark: char,
    /// Path relative to the repo root.
    pub path: String,
    /// Whether the change is staged (in the index).
    pub staged: bool,
}

/// One commit in the log.
pub struct CommitInfo {
    /// Abbreviated commit hash (7 hex chars).
    pub short: String,
    /// First line of the commit message.
    pub summary: String,
}

/// Open the repository containing `path` and read a snapshot of its state.
pub fn load(path: &Path) -> GitView {
    let repo = match Repository::discover(path) {
        Ok(repo) => repo,
        Err(e) if e.code() == ErrorCode::NotFound => return GitView::NotARepo,
        Err(e) => return GitView::Error(e.message().to_string()),
    };

    let branch = current_branch(&repo);
    let (files, extra_files) = collect_status(&repo);
    let commits = collect_log(&repo);

    GitView::Repo(GitInfo {
        branch,
        files,
        extra_files,
        commits,
    })
}

/// Branch shorthand, falling back to readable markers for edge cases.
fn current_branch(repo: &Repository) -> String {
    match repo.head() {
        Ok(head) => {
            if repo.head_detached().unwrap_or(false) {
                head.target()
                    .map(|oid| format!("detached @ {}", short_oid(oid)))
                    .unwrap_or_else(|| "detached".to_string())
            } else {
                head.shorthand().unwrap_or("HEAD").to_string()
            }
        }
        // A fresh repo with no commits has an unborn HEAD: repo.head() errors,
        // but HEAD still points symbolically at the branch-to-be.
        Err(_) => unborn_branch(repo),
    }
}

/// Branch name for an unborn HEAD (repo with no commits yet), read from the
/// symbolic HEAD reference so we still show e.g. `master`.
fn unborn_branch(repo: &Repository) -> String {
    match repo.find_reference("HEAD") {
        Ok(head) => head
            .symbolic_target()
            .and_then(|target| target.strip_prefix("refs/heads/"))
            .map(|name| format!("{name} (no commits)"))
            .unwrap_or_else(|| "(no commits yet)".to_string()),
        Err(_) => "(no commits yet)".to_string(),
    }
}

/// Collect changed files, returning the capped list plus an overflow count.
fn collect_status(repo: &Repository) -> (Vec<FileStatus>, usize) {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);

    let statuses = match repo.statuses(Some(&mut opts)) {
        Ok(statuses) => statuses,
        Err(_) => return (Vec::new(), 0),
    };

    let total = statuses.len();
    let files = statuses
        .iter()
        .take(MAX_FILES)
        .map(|entry| {
            let (mark, staged) = mark_for(entry.status());
            FileStatus {
                mark,
                path: entry.path().unwrap_or("<non-utf8 path>").to_string(),
                staged,
            }
        })
        .collect();

    (files, total.saturating_sub(MAX_FILES))
}

/// Map a libgit2 status bitset to a single mark, preferring staged state.
fn mark_for(status: Status) -> (char, bool) {
    if status.contains(Status::INDEX_NEW) {
        ('A', true)
    } else if status.contains(Status::INDEX_MODIFIED) {
        ('M', true)
    } else if status.contains(Status::INDEX_DELETED) {
        ('D', true)
    } else if status.contains(Status::INDEX_RENAMED) {
        ('R', true)
    } else if status.contains(Status::WT_NEW) {
        ('?', false)
    } else if status.contains(Status::WT_MODIFIED) {
        ('M', false)
    } else if status.contains(Status::WT_DELETED) {
        ('D', false)
    } else if status.contains(Status::WT_RENAMED) {
        ('R', false)
    } else {
        ('•', false)
    }
}

/// Walk HEAD's history and collect the most recent commits.
fn collect_log(repo: &Repository) -> Vec<CommitInfo> {
    let mut walk = match repo.revwalk() {
        Ok(walk) => walk,
        Err(_) => return Vec::new(),
    };
    if walk.push_head().is_err() {
        return Vec::new();
    }

    let mut commits = Vec::new();
    for oid in walk.take(MAX_COMMITS) {
        let Ok(oid) = oid else { continue };
        let Ok(commit) = repo.find_commit(oid) else {
            continue;
        };
        commits.push(CommitInfo {
            short: short_oid(oid),
            summary: commit.summary().unwrap_or("").to_string(),
        });
    }
    commits
}

/// Abbreviate an object id to 7 hex characters (git's default short hash).
fn short_oid(oid: git2::Oid) -> String {
    let full = oid.to_string();
    full.get(..7).unwrap_or(&full).to_string()
}
