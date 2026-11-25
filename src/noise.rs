//! Trims noisy shell output before the model sees it: build and test logs, progress
//! bars, repeated lines. Nothing is lost: the full output is saved to a file the note
//! points at, and no line that looks like an error, warning or failure is ever dropped.
//!
//! Trimming happens once, before the output enters the conversation, so it never
//! disturbs the prompt cache.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Output shorter than this is left alone.
const MIN_LINES: usize = 120;
/// Lines of context kept after an important line.
const AFTER: usize = 6;
/// Lines kept from the end (summaries live there).
const TAIL: usize = 25;
/// Lines kept from the start.
const HEAD: usize = 5;

/// A line worth keeping wherever it is.
fn important(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    [
        "error",
        "warning",
        "fail",
        "panic",
        "exception",
        "traceback",
        "assert",
        "fatal",
        "undefined",
        "cannot ",
        "can't ",
        "not found",
        "denied",
        "expected",
        "--> ",
        "   at ",
        "segmentation",
        "timed out",
        "abort",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// Progress chatter build tools print by the hundred.
fn chatter(line: &str) -> bool {
    let t = line.trim_start();
    [
        "Compiling ",
        "Checking ",
        "Downloading ",
        "Downloaded ",
        "Fresh ",
        "Updating ",
        "Locking ",
        "Adding ",
        "Building ",
        "Blocking ",
        "Documenting ",
        "Installing ",
        "Resolving ",
        "Fetching ",
        "Unpacking ",
        "Collecting ",
        "Using cached ",
        "Requirement already satisfied",
        "npm http ",
        "npm timing ",
        "go: downloading ",
        "test result: ok",
        "running ",
        "Running ",
    ]
    .iter()
    .any(|p| t.starts_with(p))
        || (t.starts_with("test ") && t.ends_with(" ... ok"))
        || t.starts_with("ok  \t")
        || t.ends_with(" PASSED")
        || t.ends_with(" passed")
        || t == "."
}

/// Removes ANSI escapes and keeps the final state of `\r`-redrawn progress lines.
fn plain(output: &str) -> String {
    let ansi = regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07").expect("valid");
    let text = ansi.replace_all(output, "");
    text.lines()
        .map(|l| l.rsplit('\r').find(|s| !s.trim().is_empty()).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapses runs of identical lines into one line with a count.
fn collapse(lines: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut run = 1;
    for line in lines {
        if out.last() == Some(&line) {
            run += 1;
            continue;
        }
        if run > 1 {
            if let Some(last) = out.last_mut() {
                last.push_str(&format!("  [repeated {run} times]"));
            }
            run = 1;
        }
        out.push(line);
    }
    if run > 1
        && let Some(last) = out.last_mut()
    {
        last.push_str(&format!("  [repeated {run} times]"));
    }
    out
}

static SAVED: AtomicUsize = AtomicUsize::new(0);

/// The trimmed output with a note on where the full output is, or `None` when it's
/// short enough to keep whole.
pub fn trim(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() < MIN_LINES {
        return None;
    }
    let cleaned = collapse(plain(output).lines().map(str::to_owned).collect());
    let n = cleaned.len();
    let mut keep = vec![false; n];
    for flag in keep.iter_mut().take(HEAD) {
        *flag = true;
    }
    for flag in keep.iter_mut().skip(n.saturating_sub(TAIL)) {
        *flag = true;
    }
    for (i, line) in cleaned.iter().enumerate() {
        if important(line) {
            for flag in keep.iter_mut().skip(i).take(AFTER + 1) {
                *flag = true;
            }
            // A line of lead-in before it, unless that's chatter.
            if i > 0 && !chatter(&cleaned[i - 1]) {
                keep[i - 1] = true;
            }
        }
    }
    // Chatter goes even near an error, unless it's itself important.
    for (i, line) in cleaned.iter().enumerate() {
        if keep[i] && chatter(line) && !important(line) && i >= HEAD && i < n - TAIL.min(n) {
            keep[i] = false;
        }
    }
    let kept = keep.iter().filter(|k| **k).count();
    if kept + 20 >= lines.len() {
        return None;
    }
    let mut out = Vec::new();
    let mut skipped = 0;
    for (line, k) in cleaned.iter().zip(&keep) {
        if *k {
            if skipped > 0 {
                out.push(format!("[... {skipped} lines ...]"));
                skipped = 0;
            }
            out.push(line.clone());
        } else {
            skipped += 1;
        }
    }
    if skipped > 0 {
        out.push(format!("[... {skipped} lines ...]"));
    }
    let saved = save(output);
    let note = match saved {
        Some(path) => format!(
            "[Output trimmed from {} to {} lines: errors, warnings and the end are kept. Full output: {}]",
            lines.len(),
            out.len(),
            path.display()
        ),
        None => format!(
            "[Output trimmed from {} to {} lines: errors, warnings and the end are kept.]",
            lines.len(),
            out.len()
        ),
    };
    Some(format!("{}\n{note}", out.join("\n")))
}

/// Where full outputs go; reading from here never asks.
pub fn dir() -> std::path::PathBuf {
    crate::config::data_dir().join("tool-output")
}

fn save(output: &str) -> Option<std::path::PathBuf> {
    let dir = dir();
    std::fs::create_dir_all(&dir).ok()?;
    let n = SAVED.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("{}-{n}.log", std::process::id()));
    std::fs::write(&path, output).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failing `cargo test` run: lots of compiling, many passing tests, two failures.
    fn cargo_log() -> String {
        let mut log = String::new();
        for i in 0..900 {
            log.push_str(&format!("   Compiling crate-{i} v1.0.{i}\n"));
        }
        log.push_str("    Finished `test` profile in 40s\n     Running unittests src/lib.rs\n");
        for i in 0..1000 {
            log.push_str(&format!("test module::case_{i} ... ok\n"));
        }
        log.push_str("test parse::rejects_empty ... FAILED\ntest parse::handles_unicode ... FAILED\n\nfailures:\n\n");
        log.push_str("---- parse::rejects_empty stdout ----\nthread 'parse::rejects_empty' panicked at src/parse.rs:42:9:\nassertion `left == right` failed\n  left: 1\n right: 0\n\n");
        log.push_str("---- parse::handles_unicode stdout ----\nthread 'parse::handles_unicode' panicked at src/parse.rs:57:5:\nindex out of bounds\n\n");
        log.push_str("failures:\n    parse::handles_unicode\n    parse::rejects_empty\n\ntest result: FAILED. 1000 passed; 2 failed; 0 ignored\n\nerror: test failed, to rerun pass `--lib`\n");
        log
    }

    #[test]
    fn a_failing_cargo_test_keeps_every_failure_in_few_lines() {
        let log = cargo_log();
        assert!(log.lines().count() > 1_900);
        let trimmed = trim(&log).expect("long logs are trimmed");
        let lines = trimmed.lines().count();
        assert!(lines < 150, "{lines} lines:\n{trimmed}");
        for needle in [
            "parse::rejects_empty ... FAILED",
            "parse::handles_unicode ... FAILED",
            "panicked at src/parse.rs:42:9",
            "assertion `left == right` failed",
            "index out of bounds",
            "test result: FAILED. 1000 passed; 2 failed",
            "error: test failed",
        ] {
            assert!(trimmed.contains(needle), "missing {needle}");
        }
        assert!(!trimmed.contains("Compiling crate-500"));
        // The full log is saved and named.
        let path = trimmed
            .rsplit("Full output: ")
            .next()
            .unwrap()
            .trim_end_matches(']');
        assert_eq!(std::fs::read_to_string(path).unwrap(), log);
    }

    #[test]
    fn short_output_and_progress_bars() {
        assert!(trim("hello\nworld\n").is_none());
        assert_eq!(plain("\x1b[32mok\x1b[0m"), "ok");
        assert_eq!(plain("10%\r50%\r100% done"), "100% done");
        assert_eq!(
            collapse(vec!["a".into(), "a".into(), "a".into(), "b".into()]),
            vec!["a  [repeated 3 times]".to_owned(), "b".to_owned()]
        );
    }
}
