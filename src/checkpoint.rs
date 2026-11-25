//! Checkpoints: snapshots of the project's files around each turn, for undo, redo and
//! rewinding to any message.
//!
//! Snapshots live in a private git repository per project under crack's data folder
//! (the project's own `.git` is never read or written). A snapshot is `git add -A` into a
//! private index followed by `git write-tree`: it respects `.gitignore`, deduplicates
//! content, and after the first one only re-reads files whose size or time changed.
//!
//! A turn's checkpoint holds the tree from before its first change and the tree from
//! when it ended. Restoring checks every file against the tree crack last left it in,
//! and refuses files someone changed since, rather than overwrite their work.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// One turn: where it starts in the conversation, and the files it changed with the
/// snapshots on either side (empty trees when it changed nothing).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Model messages before the turn's user message.
    pub messages: usize,
    /// Transcript entries before the turn's user message.
    pub entries: usize,
    /// What the user asked, to put back in the prompt on undo.
    pub prompt: String,
    pub before: String,
    pub after: String,
    /// `(status, path)`: `A` added, `M` modified, `D` deleted.
    pub files: Vec<(char, String)>,
    /// The conversation was compacted after this turn began, so it can't be rewound
    /// to here (files still can).
    #[serde(default)]
    pub compacted: bool,
}

/// A project's snapshot store.
#[derive(Clone, Debug)]
pub struct Snapshots {
    git_dir: PathBuf,
    index: PathBuf,
    work_tree: PathBuf,
}

/// What a restore did.
#[derive(Debug, Default, PartialEq)]
pub struct Restored {
    pub changed: Vec<String>,
    /// Files left alone because they changed outside crack, with a diff of how.
    pub conflicts: Vec<(String, String)>,
}

static GIT_LOCK: Mutex<()> = Mutex::new(());

impl Snapshots {
    /// The snapshot store for `project`, created on first use. `None` without git.
    pub fn open(project: &Path, session: &str) -> Option<Snapshots> {
        Self::open_in(
            &crate::config::data_dir().join("snapshots"),
            project,
            session,
        )
    }

    pub fn open_in(root: &Path, project: &Path, session: &str) -> Option<Snapshots> {
        crate::policy::which("git")?;
        let project = project.canonicalize().ok()?;
        let key = {
            use sha2::Digest;
            hex::encode(&sha2::Sha256::digest(project.display().to_string().as_bytes())[..8])
        };
        let git_dir = root.join(key);
        let snapshots = Snapshots {
            index: git_dir.join(format!("index-{session}")),
            git_dir,
            work_tree: project,
        };
        if !snapshots.git_dir.join("HEAD").exists() {
            std::fs::create_dir_all(&snapshots.git_dir).ok()?;
            let ok = Command::new("git")
                .args(["init", "-q", "--bare"])
                .arg(&snapshots.git_dir)
                .status()
                .ok()?
                .success();
            if !ok {
                return None;
            }
            // Keep the store out of the user's way: no automatic gc output, no hooks.
            let _ = snapshots.git(&["config", "gc.auto", "0"]);
            let _ = snapshots.git(&["config", "core.autocrlf", "false"]);
            let _ = std::fs::write(
                snapshots.git_dir.join("crack-project"),
                snapshots.work_tree.display().to_string(),
            );
        }
        // Start from another session's index, so the first snapshot can skip unchanged files.
        if !snapshots.index.exists()
            && let Some(newest) = std::fs::read_dir(&snapshots.git_dir)
                .ok()?
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("index-"))
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        {
            let _ = std::fs::copy(newest.path(), &snapshots.index);
        }
        Some(snapshots)
    }

    fn command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .current_dir(&self.work_tree)
            .env("GIT_INDEX_FILE", &self.index)
            // Never pick up the user's hooks, templates or global excludes.
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.excludesFile=/dev/null",
                "-c",
                "core.quotepath=false",
            ]);
        command
    }

    fn git(&self, args: &[&str]) -> anyhow::Result<String> {
        let output = self.command().args(args).output()?;
        if !output.status.success() {
            anyhow::bail!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Records the project's files and returns the tree id.
    pub fn snapshot(&self) -> anyhow::Result<String> {
        let _guard = GIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        self.git(&["add", "--all", "--ignore-errors", "--", "."])?;
        Ok(self.git(&["write-tree"])?.trim().to_owned())
    }

    /// Files that differ between two trees: `(status, path)`.
    pub fn changes(&self, from: &str, to: &str) -> anyhow::Result<Vec<(char, String)>> {
        let out = self.git(&[
            "diff-tree",
            "-r",
            "--no-renames",
            "--name-status",
            "-z",
            from,
            to,
        ])?;
        let mut fields = out.split('\0').filter(|s| !s.is_empty());
        let mut files = Vec::new();
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            files.push((status.chars().next().unwrap_or('M'), path.to_owned()));
        }
        Ok(files)
    }

    /// A unified diff between two trees, optionally for some paths only.
    pub fn diff(&self, from: &str, to: &str, paths: &[String]) -> anyhow::Result<String> {
        let mut args = vec![
            "diff",
            "--no-color",
            "--no-renames",
            "--no-ext-diff",
            from,
            to,
            "--",
        ];
        args.extend(paths.iter().map(String::as_str));
        self.git(&args)
    }

    /// Puts `paths` back to how they are in `target`, but only where the file is still
    /// as crack left it in `expected`; files missing from `target` are deleted.
    pub fn restore(
        &self,
        target: &str,
        expected: &str,
        paths: &[String],
    ) -> anyhow::Result<Restored> {
        let now = self.snapshot()?;
        let drifted: std::collections::HashSet<String> = self
            .changes(expected, &now)?
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        let mut restored = Restored::default();
        for path in paths {
            if path.starts_with(".git/") || path == ".git" {
                continue;
            }
            if drifted.contains(path) {
                let diff = self
                    .diff(expected, &now, std::slice::from_ref(path))
                    .unwrap_or_default();
                restored.conflicts.push((path.clone(), diff));
                continue;
            }
            let file = self.work_tree.join(path);
            match self.blob(target, path)? {
                Some((mode, bytes)) => {
                    if let Some(parent) = file.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    if mode == "120000" {
                        let _ = std::fs::remove_file(&file);
                        #[cfg(unix)]
                        std::os::unix::fs::symlink(
                            String::from_utf8_lossy(&bytes).as_ref(),
                            &file,
                        )?;
                    } else {
                        std::fs::write(&file, &bytes)?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let exec = if mode == "100755" { 0o755 } else { 0o644 };
                            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(exec))?;
                        }
                    }
                }
                None => {
                    if std::fs::symlink_metadata(&file).is_ok() {
                        std::fs::remove_file(&file)?;
                        remove_empty_parents(&file, &self.work_tree);
                    }
                }
            }
            restored.changed.push(path.clone());
        }
        Ok(restored)
    }

    /// A file's mode and bytes in a tree, or `None` if it isn't there.
    fn blob(&self, tree: &str, path: &str) -> anyhow::Result<Option<(String, Vec<u8>)>> {
        let listing = self.git(&["ls-tree", "-z", tree, "--", path])?;
        let Some(entry) = listing.split('\0').find(|e| !e.is_empty()) else {
            return Ok(None);
        };
        let (meta, _) = entry.split_once('\t').unwrap_or((entry, ""));
        let mut parts = meta.split_whitespace();
        let mode = parts.next().unwrap_or("100644").to_owned();
        let _kind = parts.next();
        let id = parts.next().unwrap_or_default();
        let output = self.command().args(["cat-file", "blob", id]).output()?;
        if !output.status.success() {
            anyhow::bail!("couldn't read {path} from the snapshot");
        }
        Ok(Some((mode, output.stdout)))
    }
}

fn remove_empty_parents(file: &Path, root: &Path) {
    let mut dir = file.parent();
    while let Some(d) = dir {
        if d == root || std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

/// A finished turn's snapshots: `(before, after, changed files)`.
pub type TurnChanges = (String, String, Vec<(char, String)>);

/// Watches one turn: snapshots lazily before the first change.
#[derive(Debug, Default)]
pub struct Tracker {
    snapshots: Option<Snapshots>,
    before: Mutex<Option<String>>,
}

impl Tracker {
    pub fn new(snapshots: Option<Snapshots>) -> Tracker {
        Tracker {
            snapshots,
            before: Mutex::new(None),
        }
    }

    /// Call before a tool may change files.
    pub fn before_change(&self) {
        let Some(snapshots) = &self.snapshots else {
            return;
        };
        let mut before = self.before.lock().unwrap_or_else(|e| e.into_inner());
        if before.is_none() {
            match snapshots.snapshot() {
                Ok(tree) => *before = Some(tree),
                Err(error) => eprintln!("crack: couldn't snapshot files for undo: {error:#}"),
            }
        }
    }

    /// Ends the turn: `(before, after, files)` if anything was snapshotted and changed.
    pub fn finish(&self) -> Option<TurnChanges> {
        let snapshots = self.snapshots.as_ref()?;
        let before = self
            .before
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        let after = snapshots.snapshot().ok()?;
        let files = snapshots.changes(&before, &after).ok()?;
        (!files.is_empty()).then_some((before, after, files))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (PathBuf, Snapshots) {
        let root = std::env::temp_dir().join(format!("crack-cp-{}", uuid::Uuid::new_v4()));
        let project = root.join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(project.join("gone.txt"), "delete me\n").unwrap();
        std::fs::write(project.join("data.bin"), [0u8, 159, 146, 150, 255, 0, 10]).unwrap();
        std::fs::write(project.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(project.join("target")).unwrap();
        std::fs::write(project.join("target/out"), "ignored").unwrap();
        let snapshots =
            Snapshots::open_in(&root.join("store"), &project, "s1").expect("git is installed");
        (root, snapshots)
    }

    fn read(s: &Snapshots, path: &str) -> Option<Vec<u8>> {
        std::fs::read(s.work_tree.join(path)).ok()
    }

    #[test]
    fn undo_and_redo_round_trip() {
        let (root, s) = setup();
        let tracker = Tracker::new(Some(s.clone()));
        let original_main = read(&s, "src/main.rs");
        let original_bin = read(&s, "data.bin");

        tracker.before_change();
        // A turn edits, creates, deletes, and rewrites a binary file.
        std::fs::write(
            s.work_tree.join("src/main.rs"),
            "fn main() { println!(\"hi\"); }\n",
        )
        .unwrap();
        std::fs::create_dir_all(s.work_tree.join("src/new")).unwrap();
        std::fs::write(s.work_tree.join("src/new/lib.rs"), "pub fn x() {}\n").unwrap();
        std::fs::remove_file(s.work_tree.join("gone.txt")).unwrap();
        std::fs::write(s.work_tree.join("data.bin"), [1u8, 2, 3]).unwrap();
        std::fs::write(s.work_tree.join("target/out"), "still ignored").unwrap();
        let (before, after, files) = tracker.finish().unwrap();
        let mut statuses: Vec<_> = files.iter().map(|(c, p)| format!("{c} {p}")).collect();
        statuses.sort();
        assert_eq!(
            statuses,
            [
                "A src/new/lib.rs",
                "D gone.txt",
                "M data.bin",
                "M src/main.rs"
            ]
        );
        let edited_main = read(&s, "src/main.rs");

        // Undo: back to exactly the old bytes; the new file and its folder are gone.
        let paths: Vec<String> = files.iter().map(|(_, p)| p.clone()).collect();
        let undone = s.restore(&before, &after, &paths).unwrap();
        assert!(undone.conflicts.is_empty());
        assert_eq!(read(&s, "src/main.rs"), original_main);
        assert_eq!(read(&s, "data.bin"), original_bin);
        assert_eq!(read(&s, "gone.txt").as_deref(), Some(&b"delete me\n"[..]));
        assert!(!s.work_tree.join("src/new").exists());
        assert_eq!(
            read(&s, "target/out").as_deref(),
            Some(&b"still ignored"[..])
        );

        // Redo: forward again, byte for byte.
        let redone = s.restore(&after, &before, &paths).unwrap();
        assert!(redone.conflicts.is_empty());
        assert_eq!(read(&s, "src/main.rs"), edited_main);
        assert_eq!(read(&s, "data.bin").as_deref(), Some(&[1u8, 2, 3][..]));
        assert!(read(&s, "gone.txt").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn files_changed_since_are_not_clobbered() {
        let (root, s) = setup();
        let tracker = Tracker::new(Some(s.clone()));
        tracker.before_change();
        std::fs::write(s.work_tree.join("src/main.rs"), "crack's change\n").unwrap();
        std::fs::write(s.work_tree.join("gone.txt"), "crack's other change\n").unwrap();
        let (before, after, files) = tracker.finish().unwrap();
        // The user edits one of the files afterwards.
        std::fs::write(s.work_tree.join("src/main.rs"), "the user's own work\n").unwrap();
        let paths: Vec<String> = files.iter().map(|(_, p)| p.clone()).collect();
        let restored = s.restore(&before, &after, &paths).unwrap();
        assert_eq!(restored.changed, ["gone.txt"]);
        assert_eq!(restored.conflicts.len(), 1);
        assert_eq!(restored.conflicts[0].0, "src/main.rs");
        assert!(restored.conflicts[0].1.contains("+the user's own work"));
        assert_eq!(
            read(&s, "src/main.rs").as_deref(),
            Some(&b"the user's own work\n"[..])
        );
        assert_eq!(read(&s, "gone.txt").as_deref(), Some(&b"delete me\n"[..]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn turns_without_changes_leave_no_checkpoint() {
        let (root, s) = setup();
        let tracker = Tracker::new(Some(s));
        assert!(tracker.finish().is_none());
        tracker.before_change();
        assert!(tracker.finish().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
