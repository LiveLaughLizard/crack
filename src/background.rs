//! Background shell commands: `bash` with `run_in_background` starts a command and
//! returns at once; `bash_output` reads what it printed since the last look and
//! `kill_shell` stops it. Dev servers, watchers and long builds run this way.
//!
//! Output goes to a log file in the data folder. Each command runs in its own process
//! group, recorded for crash cleanup like foreground commands, and is stopped when its
//! session's tab closes, when a headless run ends and when crack exits.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

/// The most output one `bash_output` call returns (the middle is cut beyond this).
const MAX_READ: usize = 30_000;

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Running,
    Exited(Option<i32>),
    Killed,
}

#[derive(Clone, Debug)]
pub struct Shell {
    pub id: String,
    pub session: String,
    pub command: String,
    pub log: PathBuf,
    pub pgid: Option<u32>,
    pub started: Instant,
    pub status: Status,
    /// Bytes of the log already returned by `bash_output`.
    read: u64,
    /// The model has been told it finished.
    reported: bool,
}

static SHELLS: Mutex<BTreeMap<String, Shell>> = Mutex::new(BTreeMap::new());
static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

fn shells() -> std::sync::MutexGuard<'static, BTreeMap<String, Shell>> {
    SHELLS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Starts `argv` in the background. Returns the shell id.
pub fn start(
    argv: &[String],
    command: &str,
    workdir: &std::path::Path,
    session: &str,
    path_env: std::ffi::OsString,
) -> anyhow::Result<String> {
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!("bg{n}");
    let dir = crate::config::data_dir().join("background");
    std::fs::create_dir_all(&dir)?;
    let log = dir.join(format!("{}-{id}.log", std::process::id()));
    let file = std::fs::File::create(&log)?;
    let err = file.try_clone()?;
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(workdir)
        .stdin(std::process::Stdio::null())
        .stdout(file)
        .stderr(err)
        .env("CRACK", "1")
        .env("PATH", path_env)
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .process_group(0)
        .spawn()?;
    let pgid = child.id();
    if let Some(pgid) = pgid {
        crate::recovery::track_group(pgid);
    }
    shells().insert(
        id.clone(),
        Shell {
            id: id.clone(),
            session: session.to_owned(),
            command: command.to_owned(),
            log,
            pgid,
            started: Instant::now(),
            status: Status::Running,
            read: 0,
            reported: false,
        },
    );
    let watch = id.clone();
    tokio::spawn(async move {
        let status = child.wait().await.ok().and_then(|s| s.code());
        if let Some(pgid) = pgid {
            crate::recovery::untrack_group(pgid);
        }
        if let Some(shell) = shells().get_mut(&watch)
            && shell.status == Status::Running
        {
            shell.status = Status::Exited(status);
        }
    });
    Ok(id)
}

fn describe(status: &Status) -> String {
    match status {
        Status::Running => "running".into(),
        Status::Exited(Some(code)) => format!("exited with code {code}"),
        Status::Exited(None) => "ended by a signal".into(),
        Status::Killed => "stopped".into(),
    }
}

/// New output since the last call (lines matching `filter`, when given).
pub fn output(id: &str, session: &str, filter: Option<&str>) -> anyhow::Result<String> {
    let (log, from, status) = {
        let shells = shells();
        let shell = shells
            .get(id)
            .filter(|s| s.session == session)
            .ok_or_else(|| anyhow::anyhow!("no background shell '{id}' in this session"))?;
        (shell.log.clone(), shell.read, shell.status.clone())
    };
    let bytes = std::fs::read(&log).unwrap_or_default();
    let start = (from as usize).min(bytes.len());
    let mut text = String::from_utf8_lossy(&bytes[start..]).into_owned();
    if let Some(shell) = shells().get_mut(id) {
        shell.read = bytes.len() as u64;
        if status != Status::Running {
            shell.reported = true;
        }
    }
    if let Some(pattern) = filter {
        let regex = regex::Regex::new(pattern)
            .map_err(|e| anyhow::anyhow!("filter isn't a valid regex: {e}"))?;
        text = text
            .lines()
            .filter(|l| regex.is_match(l))
            .collect::<Vec<_>>()
            .join("\n");
    }
    if text.len() > MAX_READ {
        text = crate::tools::truncate_middle(&text, MAX_READ);
    }
    let body = if text.trim().is_empty() {
        "(no new output)".to_owned()
    } else {
        text.trim_end().to_owned()
    };
    Ok(format!("{body}\n\n[{id}: {}]", describe(&status)))
}

fn signal_group(pgid: u32, signal: i32) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(pgid as i32, signal);
    }
    #[cfg(not(unix))]
    let _ = (pgid, signal);
}

/// Stops a background shell: SIGTERM, then SIGKILL a moment later.
pub fn kill(id: &str, session: &str) -> anyhow::Result<String> {
    let pgid = {
        let mut shells = shells();
        let shell = shells
            .get_mut(id)
            .filter(|s| s.session == session)
            .ok_or_else(|| anyhow::anyhow!("no background shell '{id}' in this session"))?;
        if shell.status != Status::Running {
            return Ok(format!("{id} already {}.", describe(&shell.status)));
        }
        shell.status = Status::Killed;
        shell.reported = true;
        shell.pgid
    };
    if let Some(pgid) = pgid {
        signal_group(pgid, libc::SIGTERM);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            signal_group(pgid, libc::SIGKILL);
        });
    }
    Ok(format!("Stopped {id}."))
}

/// Stops every background shell of a session (its tab closed or its run ended).
pub fn kill_session(session: &str) {
    let ids: Vec<String> = shells()
        .values()
        .filter(|s| s.session == session && s.status == Status::Running)
        .map(|s| s.id.clone())
        .collect();
    for id in ids {
        let _ = kill(&id, session);
    }
}

/// Stops every background shell (crack is exiting). Immediate SIGKILL: there's no
/// later moment to wait for.
pub fn kill_all() {
    for shell in shells().values_mut() {
        if shell.status == Status::Running {
            shell.status = Status::Killed;
            if let Some(pgid) = shell.pgid {
                signal_group(pgid, libc::SIGKILL);
                crate::recovery::untrack_group(pgid);
            }
        }
    }
}

/// A session's background shells, newest first.
pub fn list(session: &str) -> Vec<Shell> {
    let mut list: Vec<Shell> = shells()
        .values()
        .filter(|s| s.session == session)
        .cloned()
        .collect();
    list.sort_by_key(|s| std::cmp::Reverse(s.started));
    list
}

/// For the model before each turn: what's running and what finished unseen.
pub fn reminder(session: &str) -> Option<String> {
    let mut lines = Vec::new();
    for shell in shells().values_mut().filter(|s| s.session == session) {
        match shell.status {
            Status::Running => lines.push(format!(
                "- {} running for {}s: {}",
                shell.id,
                shell.started.elapsed().as_secs(),
                shell.command
            )),
            _ if !shell.reported => {
                shell.reported = true;
                lines.push(format!(
                    "- {} {} (read the rest with bash_output): {}",
                    shell.id,
                    describe(&shell.status),
                    shell.command
                ));
            }
            _ => {}
        }
    }
    (!lines.is_empty()).then(|| {
        format!(
            "<background_shells>\nBackground shell commands:\n{}\n</background_shells>",
            lines.join("\n")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_read_and_stop() {
        let argv: Vec<String> = ["bash", "-c", "echo one; echo two; sleep 30"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let id = start(&argv, "printer", &std::env::temp_dir(), "s-test", path).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let first = output(&id, "s-test", None).unwrap();
        assert!(first.contains("one\ntwo"), "{first}");
        assert!(first.contains("running"));
        // Only new output the second time.
        assert!(
            output(&id, "s-test", None)
                .unwrap()
                .starts_with("(no new output)")
        );
        // Other sessions can't see it.
        assert!(output(&id, "other", None).is_err());
        assert!(reminder("s-test").unwrap().contains("running"));
        kill(&id, "s-test").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(list("s-test")[0].status, Status::Killed);
    }

    #[tokio::test]
    async fn finished_shells_are_reported_once() {
        let argv: Vec<String> = ["bash", "-c", "echo done; exit 3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let id = start(&argv, "quick", &std::env::temp_dir(), "s-done", path).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let note = reminder("s-done").unwrap();
        assert!(note.contains("exited with code 3"), "{note}");
        assert!(reminder("s-done").is_none());
        assert!(
            output(&id, "s-done", Some("done"))
                .unwrap()
                .contains("done")
        );
    }
}
