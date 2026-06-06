//! Read-only git data for the Git pane, gathered with `git2` (libgit2).
//!
//! [`load`] never panics and never unwraps: any failure collapses into a
//! [`GitView`] variant the UI can render. A live file-watcher refresh lands in
//! step 6; for now the data is a snapshot taken when the workspace opens.

use std::path::Path;

use git2::{Branch, BranchType, ErrorCode, Repository, Status, StatusOptions};

/// How many entries to keep per section; the rest is summarised as a count.
const MAX_BRANCHES: usize = 6;
const MAX_COMMITS: usize = 6;
const MAX_FILES: usize = 12;

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
    /// Local branches, capped at [`MAX_BRANCHES`].
    pub branches: Vec<BranchInfo>,
    /// Most recent commits, newest first, capped at [`MAX_COMMITS`].
    pub commits: Vec<CommitInfo>,
    /// Changed files, capped at [`MAX_FILES`].
    pub files: Vec<FileStatus>,
    /// Changed files beyond the cap.
    pub extra_files: usize,
    /// Current branch ahead/behind its upstream (0/0 if no upstream).
    pub ahead: usize,
    pub behind: usize,
    /// Total files with staged (index) and unstaged (work-tree) changes.
    pub staged: usize,
    pub unstaged: usize,
}

/// One local branch and its tracking info.
pub struct BranchInfo {
    pub name: String,
    /// Whether this is the checked-out branch.
    pub is_head: bool,
    /// Upstream tracking info, if the branch has an upstream configured.
    pub upstream: Option<UpstreamInfo>,
}

/// Ahead/behind counts against a branch's upstream.
pub struct UpstreamInfo {
    pub ahead: usize,
    pub behind: usize,
    /// Remote name (e.g. `origin`).
    pub remote: String,
}

/// One commit in the log.
pub struct CommitInfo {
    /// Abbreviated commit hash (7 hex chars).
    pub short: String,
    /// First line of the commit message.
    pub summary: String,
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

/// Open the repository containing `path` and read a snapshot of its state.
pub fn load(path: &Path) -> GitView {
    let repo = match Repository::discover(path) {
        Ok(repo) => repo,
        Err(e) if e.code() == ErrorCode::NotFound => return GitView::NotARepo,
        Err(e) => return GitView::Error(e.message().to_string()),
    };

    let branch = current_branch(&repo);
    let branches = collect_branches(&repo);
    let commits = collect_log(&repo);
    let (files, extra_files, staged, unstaged) = collect_worktree(&repo);

    // Footer ahead/behind reflects the checked-out branch's upstream.
    let (ahead, behind) = branches
        .iter()
        .find(|b| b.is_head)
        .and_then(|b| b.upstream.as_ref())
        .map(|u| (u.ahead, u.behind))
        .unwrap_or((0, 0));

    GitView::Repo(GitInfo {
        branch,
        branches,
        commits,
        files,
        extra_files,
        ahead,
        behind,
        staged,
        unstaged,
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

/// Collect local branches with their upstream tracking info.
fn collect_branches(repo: &Repository) -> Vec<BranchInfo> {
    let branches = match repo.branches(Some(BranchType::Local)) {
        Ok(branches) => branches,
        Err(_) => return Vec::new(),
    };

    let mut out = Vec::new();
    for item in branches {
        let Ok((branch, _)) = item else { continue };
        let Ok(Some(name)) = branch.name() else { continue };
        let info = BranchInfo {
            name: name.to_string(),
            is_head: branch.is_head(),
            upstream: upstream_info(repo, &branch),
        };
        out.push(info);
        if out.len() >= MAX_BRANCHES {
            break;
        }
    }

    // Show the checked-out branch first.
    out.sort_by_key(|b| !b.is_head);
    out
}

/// Ahead/behind counts for a branch against its upstream, if any.
fn upstream_info(repo: &Repository, branch: &Branch) -> Option<UpstreamInfo> {
    let upstream = branch.upstream().ok()?;
    let local_oid = branch.get().target()?;
    let upstream_oid = upstream.get().target()?;
    let (ahead, behind) = repo.graph_ahead_behind(local_oid, upstream_oid).ok()?;
    let remote = upstream
        .name()
        .ok()
        .flatten()
        .and_then(|full| full.split('/').next().map(str::to_string))
        .unwrap_or_else(|| "origin".to_string());

    Some(UpstreamInfo {
        ahead,
        behind,
        remote,
    })
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

/// Collect changed files plus staged/unstaged totals.
///
/// Returns `(capped files, overflow count, staged total, unstaged total)`.
fn collect_worktree(repo: &Repository) -> (Vec<FileStatus>, usize, usize, usize) {
    let mut opts = StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);

    let statuses = match repo.statuses(Some(&mut opts)) {
        Ok(statuses) => statuses,
        Err(_) => return (Vec::new(), 0, 0, 0),
    };

    let staged_mask = Status::INDEX_NEW
        | Status::INDEX_MODIFIED
        | Status::INDEX_DELETED
        | Status::INDEX_RENAMED
        | Status::INDEX_TYPECHANGE;
    let unstaged_mask = Status::WT_NEW
        | Status::WT_MODIFIED
        | Status::WT_DELETED
        | Status::WT_RENAMED
        | Status::WT_TYPECHANGE;

    let mut staged = 0;
    let mut unstaged = 0;
    for entry in statuses.iter() {
        let status = entry.status();
        if status.intersects(staged_mask) {
            staged += 1;
        }
        if status.intersects(unstaged_mask) {
            unstaged += 1;
        }
    }

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

    (files, total.saturating_sub(MAX_FILES), staged, unstaged)
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

/// Abbreviate an object id to 7 hex characters (git's default short hash).
fn short_oid(oid: git2::Oid) -> String {
    let full = oid.to_string();
    full.get(..7).unwrap_or(&full).to_string()
}
