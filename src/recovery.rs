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
