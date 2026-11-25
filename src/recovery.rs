//! Crash recovery: turns that were running when crack stopped.
//!
//! While a turn runs, `running:<session>` holds the project; the starting snapshot
//! is kept under `turn-before:<session>`. Both are cleared when the turn ends, so
//! anything left at startup is a turn that never finished.

use crate::session::Session;

pub fn mark_running(session: &str, project: &str) {
    crate::store::get().set_kv(&format!("running:{session}"), project.to_owned());
}

pub fn clear(session: &str) {
    let store = crate::store::get();
    store.set_kv(&format!("running:{session}"), String::new());
    store.set_kv(&format!("turn-before:{session}"), String::new());
}

/// Sessions in this project whose last turn never finished, newest first.
pub fn interrupted(project: &str) -> Vec<String> {
    let mut ids: Vec<String> = crate::store::get()
        .kv_prefix("running:")
        .into_iter()
        .filter(|(_, value)| value == project)
        .map(|(key, _)| key.trim_start_matches("running:").to_owned())
        .collect();
    ids.sort_by(|a, b| b.cmp(a));
    ids
}

/// Repairs an interrupted session so it can be used again, with an undo record for
/// what the unfinished turn changed.
pub fn repair(id: &str, cwd: &std::path::Path) -> anyhow::Result<Session> {
    let mut session = Session::load(id)?;
    let changes = crate::store::get()
        .kv(&format!("turn-before:{id}"))
        .filter(|tree| !tree.is_empty())
        .and_then(|before| {
            let snapshots =
                crate::checkpoint::Snapshots::open(&crate::context::project_root(cwd), id)?;
            let after = snapshots.snapshot().ok()?;
            let files = snapshots.changes(&before, &after).ok()?;
            Some((before, after, files))
        });
    session.repair_interrupted(changes);
    session.save()?;
    clear(id);
    tracing::info!(session = id, "repaired an interrupted turn");
    Ok(session)
}

/// When a process started, in clock ticks since boot (field 22 of /proc/<pid>/stat), so
/// a reused pid isn't mistaken for the one recorded.
#[cfg(target_os = "linux")]
fn start_time(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) can hold spaces; fields after it follow the last ')'.
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split_whitespace().nth(19).map(str::to_owned)
}

/// Records a running shell command's process group, so that if crack is killed before
/// the command ends, the next start can stop what it left running.
pub fn track_group(pgid: u32) {
    #[cfg(target_os = "linux")]
    if let Some(started) = start_time(pgid) {
        crate::store::get().set_kv(
            &format!("pgroup:{pgid}"),
            format!("{} {started}", std::process::id()),
        );
    }
    #[cfg(not(target_os = "linux"))]
    let _ = pgid;
}

pub fn untrack_group(pgid: u32) {
    crate::store::get().set_kv(&format!("pgroup:{pgid}"), String::new());
}

/// Stops process groups left by a crack that no longer runs. Returns how many.
pub fn reap_orphans() -> usize {
    let mut reaped = 0;
    #[cfg(target_os = "linux")]
    for (key, value) in crate::store::get().kv_prefix("pgroup:") {
        let Some((owner, started)) = value.split_once(' ') else {
            continue;
        };
        let (Ok(pgid), Ok(owner)) = (
            key.trim_start_matches("pgroup:").parse::<u32>(),
            owner.parse::<i32>(),
        ) else {
            continue;
        };
        // Its crack is still running (this one or another window): leave it alone.
        if unsafe { libc::kill(owner, 0) } == 0 {
            continue;
        }
        if start_time(pgid).as_deref() == Some(started) {
            unsafe {
                libc::killpg(pgid as i32, libc::SIGKILL);
            }
            reaped += 1;
            tracing::info!(
                pgid,
                "stopped a shell command left by a crack that was killed"
            );
        }
        untrack_group(pgid);
    }
    reaped
}
