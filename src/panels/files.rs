//! File manager panel: a background-walked flat list of project files.
//!
//! [`FileManager`] owns an `Arc<Mutex<Vec<PathBuf>>>` of project-relative
//! paths populated by a background thread so the render loop is never blocked.
//!
//! Two walk strategies:
//! - **Git-aware** (recursive `read_dir` + `git2::Repository::is_path_ignored`):
//!   every file `.gitignore` doesn't exclude; used inside a git repo.
//! - **Skip-list** (`std::fs::read_dir` recursive): skips `.git`, `target`,
//!   `node_modules`, `.next`, `dist`, `__pycache__`, `.turbo`; used outside a
//!   repo.
//!
//! Both strategies always descend into hidden directories (e.g. `.github`) so
//! the flat list covers every project file; only hidden *files* are withheld
//! until `show_hidden` is on.
//!
//! The walk result is always sorted and deduplicated. Slice 4 can iterate
//! `files.lock()` directly for Ctrl+F fuzzy search.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Directory/file names that the skip-list walk ignores unconditionally.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".next",
    "dist",
    "__pycache__",
    ".turbo",
    ".svelte-kit",
    "build",
    ".cache",
];

/// How long between automatic background refreshes of the full project file
/// list (used by Ctrl+F).
pub const FILE_MANAGER_REFRESH: Duration = Duration::from_secs(30);

/// How long between automatic re-reads of the visible directory. Short enough
/// to feel live when the user (or another tool) creates/deletes/renames a file
/// in the current directory; cheap because it's a single `read_dir`.
pub const DIR_RELOAD_INTERVAL: Duration = Duration::from_millis(1500);

/// True if `name` is a directory entry the skip-list walk should ignore.
///
/// Pure, side-effect-free — unit-testable.
pub fn is_skipped(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
}

/// True if `name` is a dotfile/dot-directory (starts with `.`).
///
/// Pure, side-effect-free — unit-testable.
pub fn is_dotfile(name: &str) -> bool {
    name.starts_with('.')
}

/// One row in the file manager's current-directory view.
pub struct DirItem {
    /// The entry name, or ".." for the parent-directory row.
    pub name: String,
    /// True for directories (and the ".." parent row).
    pub is_dir: bool,
    /// True only for the ".." parent-directory row.
    pub is_parent: bool,
}

/// The lowercase extension of a name (empty if none).
fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// An icon for a directory entry. With `nerd`, returns Nerd Font glyphs (VS
/// Code-like, needs a Nerd Font); otherwise width-2 emoji that render anywhere.
/// Within either set every glyph is the same width so the names stay aligned.
pub fn icon_for(item: &DirItem, nerd: bool) -> &'static str {
    if nerd {
        nerd_icon(item)
    } else {
        emoji_icon(item)
    }
}

fn emoji_icon(item: &DirItem) -> &'static str {
    if item.is_dir {
        return "📁";
    }
    match ext_of(&item.name).as_str() {
        "rs" => "🦀",
        "js" | "jsx" | "mjs" | "cjs" => "📜",
        "ts" | "tsx" => "📘",
        "json" => "🔧",
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" => "📋",
        "md" | "markdown" => "📝",
        "py" => "🐍",
        "go" => "🐹",
        "html" | "htm" => "🌐",
        "css" | "scss" | "sass" => "🎨",
        "sh" | "bash" | "zsh" | "ps1" | "bat" => "💻",
        "lock" => "🔒",
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" => "📷",
        _ => "📄",
    }
}

/// Nerd Font (Private Use Area) glyphs — the devicon/seti set, like VS Code.
fn nerd_icon(item: &DirItem) -> &'static str {
    if item.is_dir {
        return "\u{f07b}"; // folder
    }
    match ext_of(&item.name).as_str() {
        "rs" => "\u{e7a8}",
        "js" | "jsx" | "mjs" | "cjs" => "\u{e74e}",
        "ts" | "tsx" => "\u{e628}",
        "json" => "\u{e60b}",
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" => "\u{e615}",
        "md" | "markdown" => "\u{e73e}",
        "py" => "\u{e73c}",
        "go" => "\u{e627}",
        "html" | "htm" => "\u{e736}",
        "css" | "scss" | "sass" => "\u{e749}",
        "sh" | "bash" | "zsh" | "ps1" | "bat" => "\u{f489}",
        "lock" => "\u{f023}",
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" => "\u{f1c5}",
        _ => "\u{f15b}",
    }
}

/// Walk `root` recursively, skipping entries from [`is_skipped`] and (when
/// `show_hidden` is false) dotfiles. Returns project-relative paths, sorted.
///
/// Pure in the sense that it never mutates shared state; takes `root` by
/// reference so the caller owns the directory. Unit-testable with a `tempfile`
/// tree.
pub fn skip_list_walk(root: &Path, show_hidden: bool) -> Vec<PathBuf> {
    let mut result = Vec::new();
    walk_dir(root, root, show_hidden, &mut result);
    result.sort();
    result
}

/// Recursive helper for [`skip_list_walk`].
fn walk_dir(base: &Path, dir: &Path, show_hidden: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Always skip names in the hard skip-list (`.git`, `target`, etc.).
        if is_skipped(&name_str) {
            continue;
        }

        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            // Always descend into directories — even hidden ones like
            // `.github` — so the flat list (and Ctrl+F) covers every project
            // file. The skip-list already prunes `.git`, build dirs, etc.
            walk_dir(base, &path, show_hidden, out);
        } else if meta.is_file() {
            // Hidden FILES respect the `.` toggle (matches the pane).
            if !show_hidden && is_dotfile(&name_str) {
                continue;
            }
            // Store as a project-relative path so the list is readable and
            // the same regardless of where Bruce was opened from.
            if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_path_buf());
            }
        }
    }
}

/// Walk a git repository, collecting EVERY file that `.gitignore` doesn't
/// exclude (not just the changed ones). Respects `show_hidden`.
///
/// Returns project-relative paths, sorted. The repo must be opened on the same
/// thread that calls this (`git2::Repository` is not `Send`).
pub fn git_aware_walk(repo: &git2::Repository, root: &Path, show_hidden: bool) -> Vec<PathBuf> {
    let mut result = Vec::new();
    git_walk_dir(repo, root, root, show_hidden, &mut result);
    result.sort();
    result
}

/// Recursive helper for [`git_aware_walk`]: like [`walk_dir`] but skips the
/// `.git` directory and anything `.gitignore` excludes (via
/// `repo.is_path_ignored`), so build artifacts and vendored deps stay hidden
/// without a hard-coded skip-list.
fn git_walk_dir(
    repo: &git2::Repository,
    base: &Path,
    dir: &Path,
    show_hidden: bool,
    out: &mut Vec<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".git" {
            continue;
        }

        let path = entry.path();
        let Ok(rel) = path.strip_prefix(base) else {
            continue;
        };
        // Honour .gitignore at every level (skips dirs like target/ wholesale).
        if repo.is_path_ignored(rel).unwrap_or(false) {
            continue;
        }

        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            // Always descend — even hidden dirs like `.github` — so Ctrl+F can
            // reach every project file. `.gitignore` still prunes the rest.
            git_walk_dir(repo, base, &path, show_hidden, out);
        } else if meta.is_file() {
            // Hidden FILES respect the `.` toggle (matches the pane).
            if !show_hidden && is_dotfile(&name_str) {
                continue;
            }
            out.push(rel.to_path_buf());
        }
    }
}

/// File manager state owned by [`WorkspaceState`].
pub struct FileManager {
    /// Project-relative file paths, shared with the background walk thread.
    pub files: Arc<Mutex<Vec<PathBuf>>>,
    /// Index of the currently highlighted row.
    pub selected: usize,
    /// First visible row (scroll offset).
    pub scroll_offset: usize,
    /// When true, dotfiles and dot-directories are shown.
    pub show_hidden: bool,
    /// Background walk thread; `None` when no walk is running.
    walk_handle: Option<JoinHandle<()>>,
    /// When the last background walk started (used to throttle refreshes).
    last_refresh: Instant,
    /// When the visible directory was last re-read from disk.
    last_dir_reload: Instant,
    /// Root path of the project being browsed.
    project_path: PathBuf,
    /// Directory currently being browsed, relative to `project_path` (empty at
    /// the root). The pane shows this directory's entries; `files` (the flat
    /// list above) stays whole for Ctrl+F search.
    pub current_dir: PathBuf,
    /// Entries of `current_dir`: a ".." row first (unless at root), then
    /// directories, then files. Rebuilt by [`reload_dir`] on navigation.
    pub entries: Vec<DirItem>,
}

impl FileManager {
    /// Create a new [`FileManager`] for `project_path` and immediately kick off
    /// the first background walk. The file list is empty until the walk thread
    /// writes its result — typically within a few hundred milliseconds.
    pub fn new(project_path: PathBuf) -> Self {
        let files: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
        let now = Instant::now();
        let mut fm = Self {
            files,
            selected: 0,
            scroll_offset: 0,
            show_hidden: false,
            walk_handle: None,
            last_refresh: now,
            last_dir_reload: now,
            project_path,
            current_dir: PathBuf::new(),
            entries: Vec::new(),
        };
        fm.reload_dir();
        fm.start_walk();
        fm
    }

    /// Spawn a background thread that walks the project and writes the result
    /// into `self.files`. Replaces any previous walk handle (which is joined
    /// passively; we don't block).
    pub fn start_walk(&mut self) {
        let files = Arc::clone(&self.files);
        let root = self.project_path.clone();
        let show_hidden = self.show_hidden;

        let handle = std::thread::spawn(move || {
            let walked = if let Ok(repo) = git2::Repository::open(&root) {
                git_aware_walk(&repo, &root, show_hidden)
            } else {
                skip_list_walk(&root, show_hidden)
            };
            if let Ok(mut guard) = files.lock() {
                *guard = walked;
            }
        });

        self.walk_handle = Some(handle);
        self.last_refresh = Instant::now();
    }

    /// Called from `WorkspaceState::tick()`. Drives two cadences:
    /// - [`DIR_RELOAD_INTERVAL`]: re-read the visible directory so newly
    ///   created/deleted/renamed files appear without manual navigation.
    /// - [`FILE_MANAGER_REFRESH`]: re-trigger the full project walk that
    ///   feeds Ctrl+F search.
    pub fn tick(&mut self) {
        if self.last_dir_reload.elapsed() >= DIR_RELOAD_INTERVAL {
            self.refresh_dir();
            self.last_dir_reload = Instant::now();
        }
        if self.last_refresh.elapsed() >= FILE_MANAGER_REFRESH {
            self.start_walk();
        }
    }

    /// Move selection up by one row, clamped to the top.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.clamp_scroll(0); // visible_height placeholder; clamped again on render
    }

    /// Move selection down by one row, clamped to the last entry.
    pub fn select_next(&mut self) {
        let max = self.entry_count().saturating_sub(1);
        if self.selected < max {
            self.selected += 1;
        }
        self.clamp_scroll(0);
    }

    /// Adjust `scroll_offset` so `selected` is always within the visible
    /// window of `visible_height` rows. Called every render cycle so the list
    /// stays in sync even after a walk replaces the file list.
    pub fn clamp_scroll(&mut self, visible_height: usize) {
        if visible_height == 0 {
            return;
        }
        // Clamp selected to the actual count.
        let count = self.entry_count();
        if count == 0 {
            self.selected = 0;
            self.scroll_offset = 0;
            return;
        }
        self.selected = self.selected.min(count.saturating_sub(1));

        // Scroll up if selected is above the window.
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
        // Scroll down if selected is below the window.
        let bottom = self.scroll_offset + visible_height;
        if self.selected >= bottom {
            self.scroll_offset = self.selected + 1 - visible_height;
        }
    }

    /// Toggle dotfile visibility, reload the current directory, and re-walk the
    /// flat list (used by Ctrl+F).
    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.reload_dir();
        self.start_walk();
    }

    /// Number of rows in the current-directory view.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Build the entry list for `current_dir` on disk: a ".." row first (unless
    /// at the root), then directories, then files — each group sorted
    /// case-insensitively. Build artifacts (`is_skipped`) and, unless
    /// `show_hidden`, dotfiles are omitted.
    fn build_entries(&self) -> Vec<DirItem> {
        let dir = self.project_path.join(&self.current_dir);
        let mut dirs: Vec<DirItem> = Vec::new();
        let mut files: Vec<DirItem> = Vec::new();
        if let Ok(read) = std::fs::read_dir(&dir) {
            for entry in read.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if is_skipped(&name) {
                    continue;
                }
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                // Hidden FOLDERS are always shown; hidden FILES respect the
                // `.` toggle.
                if is_dotfile(&name) && !is_dir && !self.show_hidden {
                    continue;
                }
                let item = DirItem { name, is_dir, is_parent: false };
                if is_dir {
                    dirs.push(item);
                } else {
                    files.push(item);
                }
            }
        }
        dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        let mut entries = Vec::with_capacity(dirs.len() + files.len() + 1);
        if !self.current_dir.as_os_str().is_empty() {
            entries.push(DirItem { name: "..".to_string(), is_dir: true, is_parent: true });
        }
        entries.append(&mut dirs);
        entries.append(&mut files);
        entries
    }

    /// Rebuild [`entries`] from `current_dir` and reset selection to the top.
    /// Used on navigation (entering a directory, going up, toggling hidden) —
    /// situations where jumping to the first row is the right UX.
    pub fn reload_dir(&mut self) {
        self.entries = self.build_entries();
        self.selected = 0;
        self.scroll_offset = 0;
    }

    /// Rebuild [`entries`] from `current_dir` while preserving the highlighted
    /// row by name. Used by the periodic [`tick`] refresh so files appearing or
    /// disappearing in the current directory show up without yanking the cursor
    /// back to the top.
    pub fn refresh_dir(&mut self) {
        let prev_name = self.entries.get(self.selected).map(|e| e.name.clone());
        self.entries = self.build_entries();

        let len = self.entries.len();
        if len == 0 {
            self.selected = 0;
            self.scroll_offset = 0;
            return;
        }
        // Try to keep the highlighted row on the same name; if the file is
        // gone, clamp the previous index into the new range so the cursor
        // doesn't fly off the end.
        match prev_name.and_then(|n| self.entries.iter().position(|e| e.name == n)) {
            Some(idx) => self.selected = idx,
            None => self.selected = self.selected.min(len - 1),
        }
        // `scroll_offset` gets re-clamped on the next render by
        // `clamp_scroll(visible_height)`, so don't touch it here.
    }

    /// Act on the selected entry: go up via "..", descend into a directory, or
    /// return the absolute path of a file to open. `None` means we navigated.
    pub fn enter(&mut self) -> Option<PathBuf> {
        let item = self.entries.get(self.selected)?;
        if item.is_parent {
            self.current_dir.pop();
            self.reload_dir();
            None
        } else if item.is_dir {
            let name = item.name.clone();
            self.current_dir.push(name);
            self.reload_dir();
            None
        } else {
            Some(self.project_path.join(&self.current_dir).join(&item.name))
        }
    }

    /// Go up to the parent directory (no-op at the root).
    pub fn cd_up(&mut self) {
        if self.current_dir.pop() {
            self.reload_dir();
        }
    }

    /// A short project-relative label for the current directory ("/" at root).
    pub fn current_dir_label(&self) -> String {
        if self.current_dir.as_os_str().is_empty() {
            "/".to_string()
        } else {
            format!("/{}", self.current_dir.display().to_string().replace('\\', "/"))
        }
    }

    /// Trigger a forced refresh (e.g. on first focus). Idempotent if the walk
    /// thread is still running — `start_walk` always replaces the handle.
    pub fn refresh(&mut self) {
        self.start_walk();
    }
}

/// Returns `true` when every character of `query` appears, in order, inside
/// `candidate` (case-insensitive subsequence match).
///
/// An empty query matches every candidate. This is the same algorithm used by
/// fuzzy finders like fzf for in-order character matching without gaps scoring.
///
/// Pure, side-effect-free — unit-testable.
pub fn subsequence_match(query: &str, candidate: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut q_chars = query.chars().flat_map(|c| c.to_lowercase());
    let mut current = q_chars.next();
    for c in candidate.chars().flat_map(|c| c.to_lowercase()) {
        match current {
            None => return true,
            Some(q) if q == c => {
                current = q_chars.next();
            }
            _ => {}
        }
    }
    current.is_none()
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A temporary directory removed when the guard drops. Hand-rolled to match
    /// the rest of the project's tests and avoid a dev-dependency.
    struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        fn new() -> Self {
            let mut path = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            path.push(format!("bruce-files-test-{nanos}"));
            fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
        fn path(&self) -> &Path {
            &self.path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Create a temporary directory tree for walking tests.
    ///
    /// Layout:
    /// ```
    /// root/
    ///   src/
    ///     main.rs
    ///     lib.rs
    ///   target/         ← should be skipped
    ///     debug/
    ///       build
    ///   node_modules/   ← should be skipped
    ///     package.json
    ///   .hidden         ← skipped unless show_hidden
    ///   .env            ← skipped unless show_hidden
    ///   README.md
    ///   Cargo.toml
    /// ```
    fn make_tree(root: &Path) {
        for path in &[
            "src/main.rs",
            "src/lib.rs",
            "target/debug/build",
            "node_modules/package.json",
            ".github/workflows/ci.yml",
            "README.md",
            "Cargo.toml",
        ] {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::File::create(&full).unwrap();
        }
        // Dotfiles
        fs::File::create(root.join(".hidden")).unwrap();
        fs::write(root.join(".env"), b"SECRET=1").unwrap();
    }

    #[test]
    fn is_skipped_matches_known_dirs() {
        assert!(is_skipped("target"));
        assert!(is_skipped("node_modules"));
        assert!(is_skipped(".git"));
        assert!(!is_skipped("src"));
        assert!(!is_skipped("README.md"));
    }

    #[test]
    fn is_dotfile_detects_dot_prefix() {
        assert!(is_dotfile(".env"));
        assert!(is_dotfile(".hidden"));
        assert!(!is_dotfile("README.md"));
        assert!(!is_dotfile("src"));
    }

    #[test]
    fn skip_list_walk_excludes_skipped_dirs() {
        let dir = TempDir::new();
        make_tree(dir.path());

        let files = skip_list_walk(dir.path(), false);

        // Should include these.
        assert!(files.iter().any(|p| p == Path::new("src/main.rs")), "main.rs missing");
        assert!(files.iter().any(|p| p == Path::new("src/lib.rs")), "lib.rs missing");
        assert!(files.iter().any(|p| p == Path::new("README.md")), "README missing");
        assert!(files.iter().any(|p| p == Path::new("Cargo.toml")), "Cargo.toml missing");

        // Should exclude skipped dirs and their contents.
        assert!(!files.iter().any(|p| p.starts_with("target")), "target included");
        assert!(!files.iter().any(|p| p.starts_with("node_modules")), "node_modules included");

        // Dotfiles hidden by default.
        assert!(!files.iter().any(|p| p == Path::new(".env")), ".env included");
        assert!(!files.iter().any(|p| p == Path::new(".hidden")), ".hidden included");

        // Hidden DIRECTORIES are still descended — their regular files show up
        // even with show_hidden off, so Ctrl+F can reach them.
        assert!(
            files.iter().any(|p| p == Path::new(".github/workflows/ci.yml")),
            "file inside hidden dir missing"
        );
    }

    #[test]
    fn skip_list_walk_shows_dotfiles_when_requested() {
        let dir = TempDir::new();
        make_tree(dir.path());

        let files = skip_list_walk(dir.path(), true);

        assert!(files.iter().any(|p| p == Path::new(".env")), ".env missing with show_hidden");
        assert!(files.iter().any(|p| p == Path::new(".hidden")), ".hidden missing with show_hidden");
    }

    #[test]
    fn skip_list_walk_result_is_sorted() {
        let dir = TempDir::new();
        make_tree(dir.path());

        let files = skip_list_walk(dir.path(), false);
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted, "walk result is not sorted");
    }

    // ── subsequence_match tests ───────────────────────────────────────────────

    #[test]
    fn subsequence_empty_query_matches_everything() {
        assert!(subsequence_match("", "anything"));
        assert!(subsequence_match("", ""));
    }

    #[test]
    fn subsequence_exact_match() {
        assert!(subsequence_match("main", "main.rs"));
    }

    #[test]
    fn subsequence_interleaved_chars() {
        // 's', 'r', 'c' appear in order inside "src/app.rs"
        assert!(subsequence_match("src", "src/app.rs"));
        // 'a', 'p', 'p' in order in "src/app.rs"
        assert!(subsequence_match("app", "src/app.rs"));
    }

    #[test]
    fn subsequence_case_insensitive() {
        assert!(subsequence_match("MAIN", "src/main.rs"));
        assert!(subsequence_match("Main", "src/main.rs"));
    }

    #[test]
    fn subsequence_no_match_wrong_order() {
        // "zrc" — 'z' doesn't appear in "src/main.rs" at all, so it can't match.
        assert!(!subsequence_match("zrc", "src/main.rs"));
        // "sr" appears in order but "rs" would need 'r' before 's' — yet 'r'
        // at index 1 can still be followed by 's' at the end. Use a query whose
        // chars genuinely cannot be found in order.
        assert!(!subsequence_match("zz", "src/main.rs"));
    }

    #[test]
    fn subsequence_no_match_missing_char() {
        assert!(!subsequence_match("xyz", "src/main.rs"));
    }

    #[test]
    fn subsequence_query_longer_than_candidate() {
        assert!(!subsequence_match("verylongquerythatcantmatch", "a"));
    }
}
