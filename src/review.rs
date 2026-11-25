//! Reviewing a file change hunk by hunk before it's written (the "review edits" mode):
//! what an `edit` or `write` call would make the file, split into hunks, and the file
//! with only the hunks the user kept.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use similar::{DiffOp, TextDiff};

/// A change the model proposes: the file before and after.
#[derive(Clone, Debug)]
pub struct Proposal {
    pub path: PathBuf,
    pub old: String,
    pub new: String,
}

/// What `edit` or `write` would turn the file into, if it can tell. `None` leaves the
/// call to the tool (which reports why it can't apply, e.g. `old_string` not found).
pub fn proposal(tool: &str, input: &Value, cwd: &Path) -> Option<Proposal> {
    let raw = input.get("path")?.as_str()?;
    let path = crate::config::expand_home(raw);
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let current = std::fs::read_to_string(&path).ok();
    match tool {
        "write" => Some(Proposal {
            old: current.unwrap_or_default(),
            new: input.get("content")?.as_str()?.to_owned(),
            path,
        }),
        "edit" => {
            let old_string = input.get("old_string")?.as_str()?;
            let new_string = input.get("new_string")?.as_str()?;
            let current = current?;
            if old_string.is_empty() {
                return None;
            }
            let all = input.get("replace_all").and_then(Value::as_bool) == Some(true);
            let count = current.matches(old_string).count();
            let new = match (count, all) {
                (0, _) => return None,
                (_, true) => current.replace(old_string, new_string),
                (1, false) => current.replacen(old_string, new_string, 1),
                _ => return None,
            };
            Some(Proposal {
                path,
                old: current,
                new,
            })
        }
        _ => None,
    }
}

/// One contiguous change, with a little context for display.
#[derive(Clone, Debug, PartialEq)]
pub struct Hunk {
    /// 1-based line in the old file where the change starts.
    pub line: usize,
    /// Display lines: `(' ' | '-' | '+', text)`.
    pub lines: Vec<(char, String)>,
}

/// Runs of changed lines: each maximal run of non-equal diff operations is one hunk.
fn change_runs(old: &str, new: &str) -> (Vec<String>, Vec<String>, Vec<Vec<DiffOp>>) {
    let old_lines: Vec<String> = old.split_inclusive('\n').map(str::to_owned).collect();
    let new_lines: Vec<String> = new.split_inclusive('\n').map(str::to_owned).collect();
    let old_refs: Vec<&str> = old_lines.iter().map(String::as_str).collect();
    let new_refs: Vec<&str> = new_lines.iter().map(String::as_str).collect();
    let diff = TextDiff::from_slices(&old_refs, &new_refs);
    let mut runs: Vec<Vec<DiffOp>> = Vec::new();
    let mut current: Vec<DiffOp> = Vec::new();
    for op in diff.ops() {
        if matches!(op, DiffOp::Equal { .. }) {
            if !current.is_empty() {
                runs.push(std::mem::take(&mut current));
            }
        } else {
            current.push(*op);
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    (old_lines, new_lines, runs)
}

pub fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    const CONTEXT: usize = 2;
    let (old_lines, new_lines, runs) = change_runs(old, new);
    let strip = |s: &String| s.trim_end_matches(['\n', '\r']).to_owned();
    runs.iter()
        .map(|run| {
            let first = run[0].old_range().start;
            let last = run.last().unwrap().old_range().end;
            let mut lines = Vec::new();
            for line in &old_lines[first.saturating_sub(CONTEXT)..first] {
                lines.push((' ', strip(line)));
            }
            for op in run {
                for i in op.old_range() {
                    lines.push(('-', strip(&old_lines[i])));
                }
                for i in op.new_range() {
                    lines.push(('+', strip(&new_lines[i])));
                }
            }
            for line in &old_lines[last..(last + CONTEXT).min(old_lines.len())] {
                lines.push((' ', strip(line)));
            }
            Hunk {
                line: first + 1,
                lines,
            }
        })
        .collect()
}

/// The file with only the selected hunks applied (the rest stay as they were).
pub fn apply(old: &str, new: &str, selected: &[bool]) -> String {
    let (old_lines, new_lines, runs) = change_runs(old, new);
    let mut out = String::new();
    let mut at = 0; // next old line to copy
    for (k, run) in runs.iter().enumerate() {
        let start = run[0].old_range().start;
        let end = run.last().unwrap().old_range().end;
        for line in &old_lines[at..start] {
            out.push_str(line);
        }
        if selected.get(k).copied().unwrap_or(false) {
            for op in run {
                for i in op.new_range() {
                    out.push_str(&new_lines[i]);
                }
            }
        } else {
            for line in &old_lines[start..end] {
                out.push_str(line);
            }
        }
        at = end;
    }
    for line in &old_lines[at..] {
        out.push_str(line);
    }
    out
}

/// The `write` input that applies only the kept hunks, with a note for the model.
pub fn partial_input(proposal: &Proposal, cwd: &Path, selected: &[bool]) -> Value {
    let hunks = hunks(&proposal.old, &proposal.new);
    let kept: Vec<String> = selected
        .iter()
        .enumerate()
        .filter(|(_, k)| **k)
        .map(|(i, _)| (i + 1).to_string())
        .collect();
    let rejected: Vec<String> = hunks
        .iter()
        .enumerate()
        .filter(|(i, _)| !selected.get(*i).copied().unwrap_or(false))
        .map(|(i, h)| format!("{} (at line {})", i + 1, h.line))
        .collect();
    let note = format!(
        "The user reviewed this change and applied only some of it: kept change(s) {} of {}; \
         rejected {}. The file now has only the kept changes. Don't redo the rejected ones \
         unless the user asks; read the file if you need its current text.",
        if kept.is_empty() {
            "none".into()
        } else {
            kept.join(", ")
        },
        hunks.len(),
        rejected.join(", ")
    );
    let shown = proposal
        .path
        .strip_prefix(cwd)
        .unwrap_or(&proposal.path)
        .display()
        .to_string();
    json!({
        "path": shown,
        "content": apply(&proposal.old, &proposal.new, selected),
        "__review_note": note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = "a\nb\nc\nd\ne\nf\ng\nh\n";
    const NEW: &str = "a\nB\nc\nd\ne\nf\nG\nh\nI\n";

    #[test]
    fn splits_into_hunks_and_applies_a_selection() {
        let hunks = hunks(OLD, NEW);
        assert_eq!(hunks.len(), 3);
        assert_eq!(hunks[0].line, 2);
        assert!(hunks[0].lines.contains(&('-', "b".into())));
        assert!(hunks[0].lines.contains(&('+', "B".into())));
        assert_eq!(apply(OLD, NEW, &[true, true, true]), NEW);
        assert_eq!(apply(OLD, NEW, &[false, false, false]), OLD);
        assert_eq!(
            apply(OLD, NEW, &[true, false, true]),
            "a\nB\nc\nd\ne\nf\ng\nh\nI\n"
        );
    }

    #[test]
    fn proposes_edits_like_the_edit_tool() {
        let dir = std::env::temp_dir().join(format!("crack-review-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), OLD).unwrap();
        let edit = json!({"path": "f.txt", "old_string": "b\n", "new_string": "B\n"});
        let p = proposal("edit", &edit, &dir).unwrap();
        assert_eq!(p.new, OLD.replacen("b\n", "B\n", 1));
        // Ambiguous or missing text is left to the tool to report.
        assert!(
            proposal(
                "edit",
                &json!({"path": "f.txt", "old_string": "zzz", "new_string": "y"}),
                &dir
            )
            .is_none()
        );
        let input = partial_input(
            &proposal("write", &json!({"path": "f.txt", "content": NEW}), &dir).unwrap(),
            &dir,
            &[false, true, false],
        );
        assert_eq!(input["content"], "a\nb\nc\nd\ne\nf\nG\nh\n");
        assert!(
            input["__review_note"]
                .as_str()
                .unwrap()
                .contains("kept change(s) 2 of 3")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
