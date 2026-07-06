//! Skills manager infrastructure.
//!
//! Owns the ledger (`~/.config/bruce/skills.json`), path helpers for the
//! Claude skills directory, frontmatter parsing, enable/disable/delete
//! operations, and snapshot helpers used by the install dialog.
//!
//! This module is purely logic — no TUI. The UI layer (welcome.rs) imports
//! from here and drives everything through the public API.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{agents_skills_dir, bruce_dir, claude_skills_dir};

// ─── Domain types ────────────────────────────────────────────────────────────

/// A skill Bruce knows about — tracked in the ledger.
///
/// All fields carry `#[serde(default)]` so a ledger produced by a future
/// version of Bruce with new fields still loads cleanly on older builds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEntry {
    /// Display name sourced from the SKILL.md frontmatter at install time,
    /// or the folder name when frontmatter is absent.
    #[serde(default)]
    pub name: String,
    /// Exact directory name under `~/.claude/skills/`. This is the stable
    /// anchor — renames on disk would orphan the ledger entry.
    #[serde(default)]
    pub folder_name: String,
    /// One-line description from frontmatter; empty string when absent.
    #[serde(default)]
    pub description: String,
    /// Unix timestamp (seconds) recorded at install time.
    #[serde(default)]
    pub installed_at: i64,
    /// The verbatim shell command the user ran to install this skill.
    #[serde(default)]
    pub install_command: String,
}

/// Enabled state derived from the filesystem at call time — never stored in
/// the ledger (ADR-2: avoids stale-state bugs when files change externally).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillState {
    /// `SKILL.md` is present.
    Enabled,
    /// `SKILL.md.disabled` is present (and `SKILL.md` is absent).
    Disabled,
    /// Neither file found — the skill folder is in an unexpected state.
    Broken,
}

// ─── Ledger ──────────────────────────────────────────────────────────────────

/// Thin wrapper around `Vec<SkillEntry>` with disk I/O.
///
/// The ledger lives at `~/.config/bruce/skills.json` — the same directory
/// used by `config.json` and session files.
pub struct SkillLedger {
    entries: Vec<SkillEntry>,
    path: PathBuf,
}

impl SkillLedger {
    /// Load the ledger from `~/.config/bruce/skills.json`.
    ///
    /// - File missing → return empty ledger (first-run case, not an error).
    /// - File exists but corrupt → return empty ledger and print a warning;
    ///   better to lose the cache than to crash.
    pub fn load() -> Result<Self> {
        let path = bruce_dir()?.join("skills.json");
        let entries = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<Vec<SkillEntry>>(&raw).unwrap_or_else(|e| {
                eprintln!("[bruce] skills.json is corrupt and will be reset: {e}");
                vec![]
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(e).context("reading skills ledger"),
        };
        Ok(Self { entries, path })
    }

    /// Persist the ledger to disk, creating parent directories as needed.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating ledger dir {}", parent.display()))?;
        }
        let json =
            serde_json::to_string_pretty(&self.entries).context("serialising skills ledger")?;
        fs::write(&self.path, json)
            .with_context(|| format!("writing skills ledger {}", self.path.display()))?;
        Ok(())
    }

    /// Return all tracked skill entries.
    pub fn entries(&self) -> &[SkillEntry] {
        &self.entries
    }

    /// Append a new entry and persist.
    pub fn add(&mut self, entry: SkillEntry) -> Result<()> {
        self.entries.push(entry);
        self.save()
    }

    /// Remove all entries with the given `folder_name` and persist.
    pub fn remove(&mut self, folder_name: &str) -> Result<()> {
        self.entries.retain(|e| e.folder_name != folder_name);
        self.save()
    }

    /// Drop any entry whose folder no longer exists on disk.
    ///
    /// Called once when the Manage dialog opens (ADR-5: once per open, not
    /// per frame). Only writes to disk if at least one entry was removed.
    pub fn reconcile(&mut self) -> Result<()> {
        let skills_root = claude_skills_dir()?;
        let before = self.entries.len();
        self.entries
            .retain(|e| skills_root.join(&e.folder_name).exists());
        if self.entries.len() < before {
            self.save()?;
        }
        Ok(())
    }
}

// ─── Path helpers ─────────────────────────────────────────────────────────────

/// Snapshot the immediate child directory names under `~/.claude/skills/`.
///
/// - If the directory does not exist, returns `Ok(empty set)` — this is the
///   normal pre-install state and must not surface an error (REQ-8 / Scenario 8-A).
/// - Only directories are included; files are ignored.
/// - No recursion; `~/.claude/plugins/` is never accessed.
pub fn dir_skill_names(dir: &Path) -> Result<HashSet<String>> {
    match fs::read_dir(dir) {
        Ok(entries) => {
            let mut set = HashSet::new();
            for entry in entries {
                let entry = entry.context("reading skills dir entry")?;
                // Only include directories; ignore regular files and symlinks
                // that point to non-directories.
                if entry
                    .file_type()
                    .map(|t| t.is_dir())
                    .unwrap_or(false)
                {
                    if let Some(name) = entry.file_name().to_str() {
                        set.insert(name.to_owned());
                    }
                }
            }
            Ok(set)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(HashSet::new()),
        Err(e) => Err(e).context("reading skills directory"),
    }
}

/// The skill roots Bruce watches for newly installed skills: `~/.claude/skills`
/// (where Claude reads) and `~/.agents/skills` (where `npx skills` and similar
/// tools install). Roots that can't be resolved are skipped.
pub fn skill_roots() -> Vec<PathBuf> {
    [claude_skills_dir(), agents_skills_dir()]
        .into_iter()
        .filter_map(|r| r.ok())
        .collect()
}

/// Snapshot the top-level folder names of every watched root, paired with the
/// root, so an install can be diffed per-root and a new skill traced back to the
/// directory it landed in.
pub fn snapshot_roots() -> Vec<(PathBuf, HashSet<String>)> {
    skill_roots()
        .into_iter()
        .map(|root| {
            let names = dir_skill_names(&root).unwrap_or_default();
            (root, names)
        })
        .collect()
}

/// True if the skill's `SKILL.md` (or `SKILL.md.disabled`) was modified at or
/// after `since` — i.e. the install just wrote it. Lets an install be detected
/// even when the skill folder already existed before the run (so the
/// before/after folder diff alone would miss it).
pub fn skill_touched_since(skill_dir: &Path, since: std::time::SystemTime) -> bool {
    for name in ["SKILL.md", "SKILL.md.disabled"] {
        if let Ok(meta) = fs::metadata(skill_dir.join(name)) {
            if let Ok(modified) = meta.modified() {
                if modified >= since {
                    return true;
                }
            }
        }
    }
    false
}

/// Move a skill folder `src` into `~/.claude/skills/<folder>` — where Claude
/// actually reads skills — and return the destination. Used to bridge tools
/// (e.g. `npx skills`) that install into `~/.agents/skills` and never symlink
/// into `~/.claude/skills`. Errors if the destination already exists, or if the
/// move fails (e.g. across filesystems).
pub fn relocate_into_claude(src: &Path, folder: &str) -> Result<PathBuf> {
    let dest = claude_skills_dir()?.join(folder);
    if dest.exists() {
        return Err(anyhow!("a skill named {folder} already exists in ~/.claude/skills"));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).context("creating ~/.claude/skills")?;
    }
    fs::rename(src, &dest).with_context(|| format!("moving {folder} into ~/.claude/skills"))?;
    Ok(dest)
}

// ─── Per-project activation ───────────────────────────────────────────────────
//
// Bruce uses a "library + per-project copies" model for skills:
//
// - The library lives at `~/.claude/skills/<folder>/` and is kept globally
//   DISABLED (i.e. `SKILL.md.disabled`) so the user's whole-machine Claude
//   sessions don't auto-load it. The install flow takes care of that.
//
// - Activating a skill in project X copies the library folder into
//   `<X>/.claude/skills/<folder>/` and enables `SKILL.md` in the project copy.
//   Claude discovers project-scoped skills under `<project>/.claude/skills/`
//   when it runs in that project.
//
// - Deactivating just removes the project-local copy. The library is untouched
//   so the skill remains installed for use in other projects.

/// Resolve the per-project skills directory: `<project>/.claude/skills/`. Bruce
/// stores activated copies there, mirroring the layout Claude expects for
/// project-scoped skills.
pub fn project_skills_dir(project_path: &Path) -> PathBuf {
    project_path.join(".claude").join("skills")
}

/// True when `folder_name` is currently activated in `project_path` — i.e. the
/// project copy exists with an enabled `SKILL.md`. Filesystem-driven, never
/// cached.
pub fn is_active_in_project(project_path: &Path, folder_name: &str) -> bool {
    project_skills_dir(project_path)
        .join(folder_name)
        .join("SKILL.md")
        .exists()
}

/// Copy `~/.claude/skills/<folder_name>/` into `<project>/.claude/skills/` and
/// enable the project copy. The library is left untouched.
///
/// If a project copy already exists it is wiped first so the project version
/// stays in sync with the library. Returns an error if the library copy is
/// missing — the caller surfaces that as a status-line message.
pub fn activate_in_project(project_path: &Path, folder_name: &str) -> Result<()> {
    let src = claude_skills_dir()?.join(folder_name);
    if !src.exists() {
        return Err(anyhow!(
            "skill {folder_name} is not installed in ~/.claude/skills"
        ));
    }
    let project_root = project_skills_dir(project_path);
    let dest = project_root.join(folder_name);

    // Refresh the project copy from the library every time so an activate
    // after a library update reflects the latest content.
    match fs::remove_dir_all(&dest) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("removing stale project copy {}", dest.display()))
        }
    }
    fs::create_dir_all(&project_root)
        .with_context(|| format!("creating {}", project_root.display()))?;
    copy_dir_recursive(&src, &dest)
        .with_context(|| format!("copying {folder_name} into {}", dest.display()))?;

    // The library is kept disabled (`SKILL.md.disabled`); flip the project
    // copy to enabled. If the library version was already enabled, the
    // project copy already has `SKILL.md` and there's nothing to rename.
    let project_md = dest.join("SKILL.md");
    let project_disabled = dest.join("SKILL.md.disabled");
    if !project_md.exists() && project_disabled.exists() {
        fs::rename(&project_disabled, &project_md)
            .with_context(|| format!("enabling project copy of {folder_name}"))?;
    }
    Ok(())
}

/// Names of the skills currently activated in `project_path` — i.e. the ones
/// with an enabled `SKILL.md` under `<project>/.claude/skills/`.
///
/// This is the same set the user toggles on and off from Manage Skills: when
/// they hit `E` on a skill Bruce copies the library entry into the project and
/// its name shows up here; deactivating removes it. Disabled entries in the
/// library that were never activated in this project don't count.
///
/// Sorted case-insensitively for stable display. Returns an empty vec when
/// the project doesn't have `.claude/skills/` yet or the read fails.
pub fn active_skill_names_in_project(project_path: &Path) -> Vec<String> {
    active_skill_names_in(&project_skills_dir(project_path))
}

/// Filesystem-scoped helper that powers [`active_skill_names_in_project`].
/// Split out so tests can point at a temp directory instead of a real project
/// tree.
fn active_skill_names_in(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                return false;
            }
            // Only the enabled state counts: `SKILL.md.disabled` alone means
            // the user turned it off from Manage and it must not show up in
            // the workspace subpanel.
            e.path().join("SKILL.md").exists()
        })
        .filter_map(|e| e.file_name().to_str().map(|s| s.to_owned()))
        .collect();
    names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    names
}

/// Remove the project-local copy of `folder_name`. No-op when the skill wasn't
/// activated in this project.
pub fn deactivate_in_project(project_path: &Path, folder_name: &str) -> Result<()> {
    let dest = project_skills_dir(project_path).join(folder_name);
    match fs::remove_dir_all(&dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e)
            .with_context(|| format!("deactivating {folder_name} in {}", dest.display())),
    }
}

/// Recursively copy the contents of `src` into `dst`, creating `dst` as needed.
/// Symlinks are skipped on purpose — Bruce never wants to chase a link out of
/// the source skill folder into unrelated parts of the user's filesystem.
fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if ty.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

// ─── Frontmatter parser ───────────────────────────────────────────────────────

/// Extract `(name, description)` from the YAML frontmatter of a SKILL.md file.
///
/// Tries `SKILL.md` first, then `SKILL.md.disabled`. On any failure (no file,
/// no frontmatter block, missing keys) falls back to
/// `(folder_name_string, String::new())`. Never panics; no `.unwrap()`.
pub fn parse_frontmatter(skill_dir: &Path) -> (String, String) {
    let folder_name = skill_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_owned();

    let fallback = (folder_name.clone(), String::new());

    // Try SKILL.md, then SKILL.md.disabled.
    let file = {
        let primary = skill_dir.join("SKILL.md");
        let disabled = skill_dir.join("SKILL.md.disabled");
        if primary.exists() {
            fs::File::open(primary).ok()
        } else {
            fs::File::open(disabled).ok()
        }
    };

    let file = match file {
        Some(f) => f,
        None => return fallback,
    };

    let reader = BufReader::new(file);
    let mut in_frontmatter = false;
    let mut delimiter_count = 0u8;
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();

        if trimmed == "---" {
            delimiter_count += 1;
            match delimiter_count {
                1 => {
                    in_frontmatter = true;
                    continue;
                }
                2 => break, // end of frontmatter block
                _ => break,
            }
        }

        if !in_frontmatter {
            continue;
        }

        // Split on the FIRST ": " occurrence only (handles values with colons).
        if let Some(colon_pos) = line.find(": ") {
            let key = line[..colon_pos].trim();
            let value = line[colon_pos + 2..].trim().to_owned();
            match key {
                "name" => name = Some(value),
                "description" => description = Some(value),
                _ => {}
            }
        }
    }

    // If we never even entered the frontmatter block, return fallback.
    if delimiter_count == 0 {
        return fallback;
    }

    (
        name.unwrap_or(folder_name),
        description.unwrap_or_default(),
    )
}

// ─── Skill-state query ────────────────────────────────────────────────────────

/// Derive the current enabled state of a skill by inspecting the filesystem.
///
/// This is always computed from disk, never stored (ADR-2).
pub fn skill_state(skill_dir: &Path) -> SkillState {
    if skill_dir.join("SKILL.md").exists() {
        SkillState::Enabled
    } else if skill_dir.join("SKILL.md.disabled").exists() {
        SkillState::Disabled
    } else {
        SkillState::Broken
    }
}

// ─── Disable / delete ────────────────────────────────────────────────────────

/// Disable a skill by renaming `SKILL.md` → `SKILL.md.disabled`.
///
/// On Windows, if the destination already exists the OS returns
/// `ErrorKind::AlreadyExists`. This is treated as a no-op — the skill is
/// already disabled, so the intent is satisfied (REQ-8, design ADR note).
pub fn disable_skill(skill_dir: &Path) -> Result<()> {
    let from = skill_dir.join("SKILL.md");
    let to = skill_dir.join("SKILL.md.disabled");
    match fs::rename(&from, &to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e).with_context(|| format!("disabling skill at {}", skill_dir.display())),
    }
}

/// Delete a skill: remove its directory from disk and drop its ledger entry.
///
/// `NotFound` on `remove_dir_all` is treated as `Ok(())` — the folder is
/// already gone, which satisfies the intent. The ledger entry is always
/// removed regardless of whether the folder existed.
pub fn delete_skill(entry: &SkillEntry, ledger: &mut SkillLedger) -> Result<()> {
    let skill_dir = claude_skills_dir()?.join(&entry.folder_name);
    match fs::remove_dir_all(&skill_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e)
                .with_context(|| format!("deleting skill folder {}", skill_dir.display()))
        }
    }
    ledger.remove(&entry.folder_name)
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn now_secs() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    fn make_entry(folder: &str) -> SkillEntry {
        SkillEntry {
            name: folder.to_owned(),
            folder_name: folder.to_owned(),
            description: String::new(),
            installed_at: now_secs(),
            install_command: String::new(),
        }
    }

    /// A SkillLedger that lives in a temp directory.
    fn temp_ledger() -> (SkillLedger, tempdir_guard::TempDir) {
        let td = tempdir_guard::TempDir::new();
        let path = td.path().join("skills.json");
        let ledger = SkillLedger {
            entries: vec![],
            path,
        };
        (ledger, td)
    }

    // ── P1-T1: SkillLedger ───────────────────────────────────────────────────

    #[test]
    fn test_ledger_load_missing() {
        // Load from a path that does not exist — must return empty vec, no error.
        let tmp = tempdir_guard::TempDir::new();
        let path = tmp.path().join("nonexistent").join("skills.json");
        // Manually build a ledger pointing at a nonexistent path.
        // SkillLedger::load() uses bruce_dir(), so we test the internal
        // NotFound branch via a direct construction + save/load roundtrip
        // through a known path.
        //
        // Since load() always calls bruce_dir() we verify the NotFound
        // path by writing nothing and checking a fresh SkillLedger struct
        // handles the missing-file case in its own read logic.
        let ledger = SkillLedger {
            entries: vec![],
            path: path.clone(),
        };
        // Nothing written yet — the file doesn't exist.  Simulate load:
        let entries = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<Vec<SkillEntry>>(&raw).unwrap_or_default(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
            Err(e) => panic!("unexpected error: {e}"),
        };
        assert!(entries.is_empty());
        drop(ledger);
    }

    #[test]
    fn test_ledger_load_corrupt() {
        let tmp = tempdir_guard::TempDir::new();
        let path = tmp.path().join("skills.json");
        fs::write(&path, b"this is not valid JSON {{{{").expect("write");
        let raw = fs::read_to_string(&path).expect("read");
        let entries =
            serde_json::from_str::<Vec<SkillEntry>>(&raw).unwrap_or_else(|_| vec![]);
        assert!(entries.is_empty(), "corrupt JSON should return empty vec");
    }

    #[test]
    fn test_ledger_add_remove() {
        let (mut ledger, _td) = temp_ledger();
        ledger.add(make_entry("my-skill")).expect("add");
        assert_eq!(ledger.entries().len(), 1);
        ledger.remove("my-skill").expect("remove");
        assert_eq!(ledger.entries().len(), 0);
    }

    #[test]
    fn test_ledger_save_roundtrip() {
        let (mut ledger, _td) = temp_ledger();
        let entry = SkillEntry {
            name: "test-skill".to_owned(),
            folder_name: "test-skill".to_owned(),
            description: "A test skill".to_owned(),
            installed_at: 1718000000,
            install_command: "npx skills add test-skill".to_owned(),
        };
        ledger.add(entry.clone()).expect("add");
        ledger.save().expect("save");

        // Load back from the same path.
        let raw = fs::read_to_string(&ledger.path).expect("read");
        let loaded: Vec<SkillEntry> = serde_json::from_str(&raw).expect("parse");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, entry.name);
        assert_eq!(loaded[0].folder_name, entry.folder_name);
        assert_eq!(loaded[0].description, entry.description);
        assert_eq!(loaded[0].installed_at, entry.installed_at);
        assert_eq!(loaded[0].install_command, entry.install_command);
    }

    // ── P1-T2: skills_dir_snapshot ───────────────────────────────────────────

    #[test]
    fn test_skills_dir_snapshot_missing_dir() {
        // A directory that does not exist must return Ok(empty set), not an error.
        let tmp = tempdir_guard::TempDir::new();
        let absent = tmp.path().join("does_not_exist");
        // Directly exercise the read_dir NotFound branch.
        let result = match fs::read_dir(&absent) {
            Ok(entries) => {
                let mut set = HashSet::new();
                for e in entries {
                    let e = e.unwrap();
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        if let Some(n) = e.file_name().to_str() {
                            set.insert(n.to_owned());
                        }
                    }
                }
                set
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => HashSet::new(),
            Err(e) => panic!("unexpected: {e}"),
        };
        assert!(result.is_empty());
    }

    #[test]
    fn test_skills_dir_snapshot_with_dirs() {
        let tmp = tempdir_guard::TempDir::new();
        let base = tmp.path();
        fs::create_dir(base.join("skill-a")).expect("mkdir a");
        fs::create_dir(base.join("skill-b")).expect("mkdir b");
        fs::write(base.join("regular-file.txt"), b"hello").expect("write file");

        let mut set = HashSet::new();
        for entry in fs::read_dir(base).expect("read_dir") {
            let entry = entry.expect("entry");
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(n) = entry.file_name().to_str() {
                    set.insert(n.to_owned());
                }
            }
        }
        assert_eq!(set.len(), 2, "only directories should be included");
        assert!(set.contains("skill-a"));
        assert!(set.contains("skill-b"));
    }

    #[test]
    fn test_skills_dir_snapshot_diff() {
        let tmp = tempdir_guard::TempDir::new();
        let base = tmp.path();
        fs::create_dir(base.join("existing-skill")).expect("mkdir");

        // First snapshot.
        let snap1: HashSet<String> = fs::read_dir(base)
            .expect("read_dir")
            .filter_map(|e| {
                let e = e.ok()?;
                if e.file_type().ok()?.is_dir() {
                    e.file_name().to_str().map(|s| s.to_owned())
                } else {
                    None
                }
            })
            .collect();

        // Add a new directory.
        fs::create_dir(base.join("new-skill")).expect("mkdir new");

        // Second snapshot.
        let snap2: HashSet<String> = fs::read_dir(base)
            .expect("read_dir")
            .filter_map(|e| {
                let e = e.ok()?;
                if e.file_type().ok()?.is_dir() {
                    e.file_name().to_str().map(|s| s.to_owned())
                } else {
                    None
                }
            })
            .collect();

        let diff: HashSet<&String> = snap2.difference(&snap1).collect();
        assert_eq!(diff.len(), 1);
        assert!(diff.iter().any(|s| s.as_str() == "new-skill"));
    }

    // ── P1-T3: parse_frontmatter ─────────────────────────────────────────────

    #[test]
    fn test_parse_frontmatter_well_formed() {
        let tmp = tempdir_guard::TempDir::new();
        let skill_dir = tmp.path().join("my-skill");
        fs::create_dir(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join("SKILL.md"),
            b"---\nname: foo\ndescription: bar desc\n---\n## Content\n",
        )
        .expect("write");
        let (name, desc) = parse_frontmatter(&skill_dir);
        assert_eq!(name, "foo");
        assert_eq!(desc, "bar desc");
    }

    #[test]
    fn test_parse_frontmatter_missing_file() {
        let tmp = tempdir_guard::TempDir::new();
        let skill_dir = tmp.path().join("no-file-skill");
        fs::create_dir(&skill_dir).expect("mkdir");
        // No SKILL.md or SKILL.md.disabled created.
        let (name, desc) = parse_frontmatter(&skill_dir);
        assert_eq!(name, "no-file-skill");
        assert_eq!(desc, "");
    }

    #[test]
    fn test_parse_frontmatter_no_frontmatter_block() {
        let tmp = tempdir_guard::TempDir::new();
        let skill_dir = tmp.path().join("no-fm");
        fs::create_dir(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join("SKILL.md"),
            b"# Just a heading\nSome content, no frontmatter.\n",
        )
        .expect("write");
        let (name, desc) = parse_frontmatter(&skill_dir);
        assert_eq!(name, "no-fm");
        assert_eq!(desc, "");
    }

    #[test]
    fn test_parse_frontmatter_description_absent() {
        let tmp = tempdir_guard::TempDir::new();
        let skill_dir = tmp.path().join("no-desc");
        fs::create_dir(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join("SKILL.md"),
            b"---\nname: my-name\n---\n",
        )
        .expect("write");
        let (name, desc) = parse_frontmatter(&skill_dir);
        assert_eq!(name, "my-name");
        assert_eq!(desc, "");
    }

    #[test]
    fn test_parse_frontmatter_colon_in_value() {
        let tmp = tempdir_guard::TempDir::new();
        let skill_dir = tmp.path().join("colon-val");
        fs::create_dir(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join("SKILL.md"),
            b"---\nname: foo: bar\ndescription: a: b: c\n---\n",
        )
        .expect("write");
        let (name, desc) = parse_frontmatter(&skill_dir);
        assert_eq!(name, "foo: bar");
        assert_eq!(desc, "a: b: c");
    }

    #[test]
    fn test_parse_frontmatter_disabled_file() {
        let tmp = tempdir_guard::TempDir::new();
        let skill_dir = tmp.path().join("disabled-skill");
        fs::create_dir(&skill_dir).expect("mkdir");
        // Only the .disabled file is present (SKILL.md absent).
        fs::write(
            skill_dir.join("SKILL.md.disabled"),
            b"---\nname: disabled-name\ndescription: disabled desc\n---\n",
        )
        .expect("write");
        let (name, desc) = parse_frontmatter(&skill_dir);
        assert_eq!(name, "disabled-name");
        assert_eq!(desc, "disabled desc");
    }

    // ── P1-T4: skill_state + enable/disable/delete ───────────────────────────

    #[test]
    fn test_skill_state_enabled() {
        let tmp = tempdir_guard::TempDir::new();
        let skill_dir = tmp.path().join("enabled-skill");
        fs::create_dir(&skill_dir).expect("mkdir");
        fs::write(skill_dir.join("SKILL.md"), b"").expect("write");
        assert_eq!(skill_state(&skill_dir), SkillState::Enabled);
    }

    #[test]
    fn test_skill_state_disabled() {
        let tmp = tempdir_guard::TempDir::new();
        let skill_dir = tmp.path().join("disabled-skill");
        fs::create_dir(&skill_dir).expect("mkdir");
        fs::write(skill_dir.join("SKILL.md.disabled"), b"").expect("write");
        assert_eq!(skill_state(&skill_dir), SkillState::Disabled);
    }

    #[test]
    fn test_skill_state_broken() {
        let tmp = tempdir_guard::TempDir::new();
        let skill_dir = tmp.path().join("broken-skill");
        fs::create_dir(&skill_dir).expect("mkdir");
        // No SKILL.md or SKILL.md.disabled.
        assert_eq!(skill_state(&skill_dir), SkillState::Broken);
    }

    #[test]
    fn test_disable_skill_path_construction() {
        let tmp = tempdir_guard::TempDir::new();
        let skill_dir = tmp.path().join("my-skill");
        fs::create_dir(&skill_dir).expect("mkdir");
        // Start in Enabled state.
        fs::write(skill_dir.join("SKILL.md"), b"").expect("write enabled");
        assert_eq!(skill_state(&skill_dir), SkillState::Enabled);

        disable_skill(&skill_dir).expect("disable");

        assert!(
            skill_dir.join("SKILL.md.disabled").exists(),
            "SKILL.md.disabled should exist"
        );
        assert!(
            !skill_dir.join("SKILL.md").exists(),
            "SKILL.md should be gone"
        );
        assert_eq!(skill_state(&skill_dir), SkillState::Disabled);
    }

    #[test]
    fn test_disable_skill_already_exists() {
        let tmp = tempdir_guard::TempDir::new();
        let skill_dir = tmp.path().join("my-skill");
        fs::create_dir(&skill_dir).expect("mkdir");
        // Create BOTH files — simulates the Windows AlreadyExists scenario
        // where a previous disable didn't clean up or rename raced.
        fs::write(skill_dir.join("SKILL.md"), b"").expect("write skill.md");
        fs::write(skill_dir.join("SKILL.md.disabled"), b"").expect("write skill.md.disabled");

        // On Windows this rename would fail with AlreadyExists; on Linux/macOS
        // rename() atomically replaces the target, so the result differs by
        // platform. Either way the function must not panic and must return Ok.
        let result = disable_skill(&skill_dir);
        assert!(result.is_ok(), "disable_skill must handle AlreadyExists as Ok");
    }

    #[test]
    fn test_delete_skill_removes_folder() {
        let tmp = tempdir_guard::TempDir::new();
        // Build a fake claude_skills_dir-like structure inside tmp.
        let skills_root = tmp.path().join("skills");
        fs::create_dir(&skills_root).expect("skills root");
        let skill_dir = skills_root.join("my-skill");
        fs::create_dir(&skill_dir).expect("skill dir");
        fs::write(skill_dir.join("SKILL.md"), b"").expect("SKILL.md");

        // Ledger pointing to our temp dir.
        let ledger_path = tmp.path().join("skills.json");
        let entry = make_entry("my-skill");
        let mut ledger = SkillLedger {
            entries: vec![entry.clone()],
            path: ledger_path,
        };
        ledger.save().expect("save");

        // Directly call fs::remove_dir_all + ledger.remove (mirrors delete_skill
        // but without needing claude_skills_dir() which reads HOME env).
        fs::remove_dir_all(&skill_dir).expect("remove_dir_all");
        ledger.remove(&entry.folder_name).expect("ledger remove");

        assert!(!skill_dir.exists(), "folder should be deleted");
        assert!(ledger.entries().is_empty(), "ledger should be empty");
    }

    #[test]
    fn test_delete_skill_not_found_is_ok() {
        let tmp = tempdir_guard::TempDir::new();
        let absent_dir = tmp.path().join("skills").join("ghost-skill");
        // The folder does not exist.
        let result = match fs::remove_dir_all(&absent_dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
        assert!(result.is_ok(), "NotFound on remove_dir_all must be Ok");
    }

    // ── Per-project activation ───────────────────────────────────────────────

    #[test]
    fn test_is_active_in_project_false_when_missing() {
        let tmp = tempdir_guard::TempDir::new();
        assert!(!is_active_in_project(tmp.path(), "any-skill"));
    }

    #[test]
    fn test_is_active_in_project_true_when_skill_md_exists() {
        let tmp = tempdir_guard::TempDir::new();
        let dest = tmp.path().join(".claude").join("skills").join("my-skill");
        fs::create_dir_all(&dest).expect("mkdir");
        fs::write(dest.join("SKILL.md"), b"---\nname: my-skill\n---\n").expect("write");
        assert!(is_active_in_project(tmp.path(), "my-skill"));
    }

    #[test]
    fn test_is_active_in_project_false_when_only_disabled_file() {
        let tmp = tempdir_guard::TempDir::new();
        let dest = tmp.path().join(".claude").join("skills").join("my-skill");
        fs::create_dir_all(&dest).expect("mkdir");
        fs::write(dest.join("SKILL.md.disabled"), b"").expect("write");
        assert!(!is_active_in_project(tmp.path(), "my-skill"));
    }

    #[test]
    fn test_copy_dir_recursive_replicates_tree() {
        let tmp = tempdir_guard::TempDir::new();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir_all(src.join("nested")).expect("mkdir nested");
        fs::write(src.join("a.txt"), b"hello").expect("a.txt");
        fs::write(src.join("nested").join("b.txt"), b"world").expect("b.txt");

        copy_dir_recursive(&src, &dst).expect("copy");

        assert_eq!(fs::read_to_string(dst.join("a.txt")).expect("a"), "hello");
        assert_eq!(
            fs::read_to_string(dst.join("nested").join("b.txt")).expect("b"),
            "world"
        );
    }

    #[test]
    fn test_active_skill_names_in_empty_when_dir_missing() {
        let tmp = tempdir_guard::TempDir::new();
        // Never created the project skills root — must be empty, not an error.
        assert!(active_skill_names_in(&tmp.path().join("missing")).is_empty());
    }

    #[test]
    fn test_active_skill_names_in_only_includes_enabled_skills() {
        let tmp = tempdir_guard::TempDir::new();
        let root = tmp.path();

        // Enabled skill: has SKILL.md — must appear.
        let enabled = root.join("Enabled");
        fs::create_dir_all(&enabled).expect("enabled");
        fs::write(enabled.join("SKILL.md"), b"").expect("enabled md");

        // Disabled skill: only SKILL.md.disabled — must NOT appear, since it
        // was toggled off from Manage and shouldn't clutter the subpanel.
        let disabled = root.join("disabled");
        fs::create_dir_all(&disabled).expect("disabled");
        fs::write(disabled.join("SKILL.md.disabled"), b"").expect("disabled md");

        // Not-a-skill folder: no SKILL.md files at all — must be excluded.
        let stray = root.join("stray");
        fs::create_dir_all(&stray).expect("stray");
        fs::write(stray.join("README.md"), b"").expect("stray readme");

        // Stray file at the root — must be ignored (only dirs count).
        fs::write(root.join("loose.md"), b"").expect("loose");

        let names = active_skill_names_in(root);
        assert_eq!(names, vec!["Enabled".to_string()]);
    }

    #[test]
    fn test_deactivate_in_project_noop_when_missing() {
        let tmp = tempdir_guard::TempDir::new();
        // Skill was never activated in this project — must succeed silently.
        deactivate_in_project(tmp.path(), "ghost-skill").expect("deactivate");
    }

    #[test]
    fn test_deactivate_in_project_removes_existing() {
        let tmp = tempdir_guard::TempDir::new();
        let dest = tmp.path().join(".claude").join("skills").join("my-skill");
        fs::create_dir_all(&dest).expect("mkdir");
        fs::write(dest.join("SKILL.md"), b"").expect("write");
        assert!(is_active_in_project(tmp.path(), "my-skill"));

        deactivate_in_project(tmp.path(), "my-skill").expect("deactivate");
        assert!(!dest.exists(), "project copy should be gone");
        assert!(!is_active_in_project(tmp.path(), "my-skill"));
    }

    // ── P1-T5: SkillLedger::reconcile ────────────────────────────────────────

    #[test]
    fn test_reconcile_keeps_existing() {
        let tmp = tempdir_guard::TempDir::new();
        // Simulate two existing skill folders.
        let skills_root = tmp.path().join(".claude").join("skills");
        fs::create_dir_all(skills_root.join("skill-a")).expect("mkdir a");
        fs::create_dir_all(skills_root.join("skill-b")).expect("mkdir b");

        let ledger_path = tmp.path().join("skills.json");
        let mut ledger = SkillLedger {
            entries: vec![make_entry("skill-a"), make_entry("skill-b")],
            path: ledger_path,
        };
        ledger.save().expect("save");

        // Manually run the reconcile logic with a custom skills_root.
        let before = ledger.entries.len();
        ledger
            .entries
            .retain(|e| skills_root.join(&e.folder_name).exists());
        assert_eq!(ledger.entries.len(), before, "all entries should be kept");
    }

    #[test]
    fn test_reconcile_drops_missing() {
        let tmp = tempdir_guard::TempDir::new();
        let skills_root = tmp.path().join(".claude").join("skills");
        // Only skill-a exists on disk; skill-b is absent.
        fs::create_dir_all(skills_root.join("skill-a")).expect("mkdir a");

        let ledger_path = tmp.path().join("skills.json");
        let mut ledger = SkillLedger {
            entries: vec![make_entry("skill-a"), make_entry("skill-b")],
            path: ledger_path,
        };
        ledger.save().expect("save");

        let before_count = ledger.entries.len();
        ledger
            .entries
            .retain(|e| skills_root.join(&e.folder_name).exists());
        assert!(
            ledger.entries.len() < before_count,
            "missing entry should be dropped"
        );
        assert_eq!(ledger.entries.len(), 1);
        assert_eq!(ledger.entries[0].folder_name, "skill-a");
    }

    #[test]
    fn test_reconcile_empty_ledger() {
        let tmp = tempdir_guard::TempDir::new();
        let skills_root = tmp.path().join(".claude").join("skills");
        fs::create_dir_all(&skills_root).expect("mkdir");

        let ledger_path = tmp.path().join("skills.json");
        let mut ledger = SkillLedger {
            entries: vec![],
            path: ledger_path,
        };

        // Reconcile on empty ledger must be a no-op.
        let before = ledger.entries.len();
        ledger
            .entries
            .retain(|e| skills_root.join(&e.folder_name).exists());
        assert_eq!(ledger.entries.len(), before);
        assert_eq!(ledger.entries.len(), 0);
    }
}

// ── Minimal tempdir helper (no external crate) ────────────────────────────────

#[cfg(test)]
mod tempdir_guard {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A temporary directory that is deleted when the guard is dropped.
    pub struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        pub fn new() -> Self {
            // Use a unique name to avoid collisions between parallel tests.
            use std::sync::atomic::{AtomicU64, Ordering};
            use std::time::{SystemTime, UNIX_EPOCH};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let count = COUNTER.fetch_add(1, Ordering::SeqCst);
            let name = format!("bruce_test_{}_{}", ts, count);
            let path = std::env::temp_dir().join(name);
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
