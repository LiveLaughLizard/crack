//! Undo, redo, rewinding to a message, and the session diff.

use gpui::*;

use super::picker::{PickerItem, PickerKind};
use super::view::CrackView;
use crate::checkpoint::{Checkpoint, Snapshots};
use crate::providers::Message;
use crate::session::Entry;

/// A turn taken back by undo, kept so redo can put it back.
pub struct Redo {
    session: String,
    messages: Vec<Message>,
    entries: Vec<Entry>,
    checkpoint: Checkpoint,
}

/// The diff viewer's contents.
pub struct DiffView {
    pub title: String,
    pub lines: Vec<SharedString>,
    pub scroll: ScrollHandle,
}

/// What to rewind.
#[derive(Clone, Copy, PartialEq)]
enum Scope {
    Both,
    Conversation,
    Files,
}

impl CrackView {
    fn snapshots(&self) -> Option<Snapshots> {
        Snapshots::open(
            &crate::context::project_root(&self.ws.cwd),
            &self.session.id,
        )
    }

    fn busy(&mut self, cx: &mut Context<Self>) -> bool {
        if self.running.is_some() {
            self.show_toast("Wait for the reply to finish first.", true, cx);
        }
        self.running.is_some()
    }

    /// `/undo`: asks before taking back the last turn.
    pub(super) fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy(cx) {
            return;
        }
        let Some(last) = self.session.checkpoints.last() else {
            self.show_toast("Nothing to undo.", false, cx);
            return;
        };
        self.rewind_target = Some(last.entries);
        self.open_picker(PickerKind::Undo, window, cx);
    }

    pub(super) fn undo_items(&self) -> Vec<PickerItem> {
        let Some(target) = self.rewind_target else {
            return Vec::new();
        };
        let Some(index) = self
            .session
            .checkpoints
            .iter()
            .position(|c| c.entries == target)
        else {
            return Vec::new();
        };
        let turns = &self.session.checkpoints[index..];
        let files = changed_files(turns);
        let compacted = turns[0].compacted;
        let latest = turns.len() == 1;
        let files_text = describe_files(&files);
        let item = |id: &str, title: String, description: String, disabled: bool| PickerItem {
            id: id.into(),
            title,
            description: Some(description),
            category: None,
            footer: None,
            current: false,
            disabled,
        };
        let mut items = Vec::new();
        let what = if latest {
            "the last turn"
        } else {
            "everything from here on"
        };
        if !compacted {
            items.push(item(
                "both",
                format!("Undo {what}: conversation and files"),
                files_text.clone(),
                false,
            ));
            items.push(item(
                "conversation",
                "Conversation only".into(),
                "Files stay as they are now".into(),
                false,
            ));
        }
        items.push(item(
            "files",
            "Files only".into(),
            if compacted {
                format!("{files_text} (the conversation was compacted since, so it stays)")
            } else {
                files_text.clone()
            },
            files.is_empty(),
        ));
        items.push(item("cancel", "Cancel".into(), String::new(), false));
        items
    }

    /// Opens the rewind choices for the turn starting at transcript entry `entry`.
    pub(super) fn rewind_to(&mut self, entry: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy(cx) {
            return;
        }
        if !self.session.checkpoints.iter().any(|c| c.entries == entry) {
            self.show_toast(
                "This message can't be restored (it predates checkpoints).",
                true,
                cx,
            );
            return;
        }
        self.rewind_target = Some(entry);
        self.open_picker(PickerKind::Undo, window, cx);
    }

    /// The choice from the undo / rewind picker.
    pub(super) fn confirm_rewind(&mut self, choice: &str, cx: &mut Context<Self>) {
        let Some(target) = self.rewind_target.take() else {
            return;
        };
        let scope = match choice {
            "both" => Scope::Both,
            "conversation" => Scope::Conversation,
            "files" => Scope::Files,
            _ => return,
        };
        let Some(index) = self
            .session
            .checkpoints
            .iter()
            .position(|c| c.entries == target)
        else {
            return;
        };
        let turns: Vec<Checkpoint> = self.session.checkpoints[index..].to_vec();
        let first = turns[0].clone();

        let mut conflicts = Vec::new();
        let mut restored = 0;
        if scope != Scope::Conversation {
            let files = changed_files(&turns);
            if !files.is_empty() {
                let (Some(snapshots), Some(before), Some(expected)) = (
                    self.snapshots(),
                    turns
                        .iter()
                        .find(|t| !t.before.is_empty())
                        .map(|t| t.before.clone()),
                    turns
                        .iter()
                        .rev()
                        .find(|t| !t.after.is_empty())
                        .map(|t| t.after.clone()),
                ) else {
                    self.show_toast(
                        "File snapshots aren't available (is git installed?).",
                        true,
                        cx,
                    );
                    return;
                };
                match snapshots.restore(&before, &expected, &files) {
                    Ok(result) => {
                        restored = result.changed.len();
                        conflicts = result.conflicts;
                    }
                    Err(error) => {
                        self.show_toast(format!("Couldn't restore files: {error:#}"), true, cx);
                        return;
                    }
                }
            }
        }
        if scope != Scope::Files && !first.compacted {
            // Only the newest turn can be redone; rewinding further clears redo.
            self.redo = (turns.len() == 1 && conflicts.is_empty()).then(|| Redo {
                session: self.session.id.clone(),
                messages: self.session.messages[first.messages.min(self.session.messages.len())..]
                    .to_vec(),
                entries: self.session.entries[first.entries.min(self.session.entries.len())..]
                    .to_vec(),
                checkpoint: first.clone(),
            });
            self.session.rewind(first.messages, first.entries);
            let prompt = first.prompt.clone();
            self.input
                .update(cx, |input, cx| input.set_text(prompt, cx));
        } else {
            self.redo = None;
        }
        let tab = self.tab_id;
        self.save_tab(tab);

        if conflicts.is_empty() {
            let text = match scope {
                Scope::Conversation => "Rewound the conversation; files are unchanged.".to_owned(),
                _ if restored == 0 => "Rewound the conversation.".to_owned(),
                _ => format!(
                    "Restored {restored} file{}.",
                    if restored == 1 { "" } else { "s" }
                ),
            };
            self.show_toast(text, false, cx);
        } else {
            let names: Vec<&str> = conflicts.iter().map(|(p, _)| p.as_str()).collect();
            self.show_toast(
                format!(
                    "Restored {restored}; left {} alone because they changed outside crack: {}",
                    conflicts.len(),
                    names.join(", ")
                ),
                true,
                cx,
            );
            let lines = conflicts
                .iter()
                .flat_map(|(_, d)| d.lines().map(|l| l.to_owned().into()))
                .collect();
            self.diff_view = Some(DiffView {
                title: "Changed outside crack since (not restored)".into(),
                lines,
                scroll: ScrollHandle::new(),
            });
        }
        cx.notify();
    }

    /// `/redo`: puts back the turn the last undo took.
    pub(super) fn redo(&mut self, cx: &mut Context<Self>) {
        if self.busy(cx) {
            return;
        }
        let Some(redo) = self.redo.take() else {
            self.show_toast("Nothing to redo.", false, cx);
            return;
        };
        let unchanged = redo.session == self.session.id
            && self.session.messages.len() == redo.checkpoint.messages
            && self.session.entries.len() == redo.checkpoint.entries;
        if !unchanged {
            self.show_toast(
                "Can't redo: the conversation has moved on since the undo.",
                true,
                cx,
            );
            return;
        }
        let files: Vec<String> = redo
            .checkpoint
            .files
            .iter()
            .map(|(_, p)| p.clone())
            .collect();
        if !files.is_empty() {
            let Some(snapshots) = self.snapshots() else {
                self.show_toast("File snapshots aren't available.", true, cx);
                return;
            };
            match snapshots.restore(&redo.checkpoint.after, &redo.checkpoint.before, &files) {
                Ok(result) if !result.conflicts.is_empty() => {
                    let names: Vec<&str> =
                        result.conflicts.iter().map(|(p, _)| p.as_str()).collect();
                    self.show_toast(
                        format!(
                            "Redid the conversation; left alone (changed since): {}",
                            names.join(", ")
                        ),
                        true,
                        cx,
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    self.show_toast(format!("Couldn't redo files: {error:#}"), true, cx);
                    return;
                }
            }
        }
        self.session
            .reapply(redo.messages, redo.entries, redo.checkpoint);
        self.input
            .update(cx, |input, cx| input.set_text(String::new(), cx));
        let tab = self.tab_id;
        self.save_tab(tab);
        self.show_toast("Redone.", false, cx);
        cx.notify();
    }

    /// `/diff`: everything this session changed in the project, as it is now.
    pub(super) fn show_session_diff(&mut self, cx: &mut Context<Self>) {
        let turns = &self.session.checkpoints;
        let files = changed_files(turns);
        let Some(before) = turns
            .iter()
            .find(|t| !t.before.is_empty())
            .map(|t| t.before.clone())
        else {
            self.show_toast("This session hasn't changed any files.", false, cx);
            return;
        };
        let Some(snapshots) = self.snapshots() else {
            self.show_toast(
                "File snapshots aren't available (is git installed?).",
                true,
                cx,
            );
            return;
        };
        let diff = snapshots
            .snapshot()
            .and_then(|now| snapshots.diff(&before, &now, &files));
        match diff {
            Ok(diff) if diff.trim().is_empty() => self.show_toast(
                "No differences: the files are back to how they started.",
                false,
                cx,
            ),
            Ok(diff) => {
                self.diff_view = Some(DiffView {
                    title: format!(
                        "Changes this session ({} file{})",
                        files.len(),
                        if files.len() == 1 { "" } else { "s" }
                    ),
                    lines: diff.lines().map(|l| l.to_owned().into()).collect(),
                    scroll: ScrollHandle::new(),
                });
            }
            Err(error) => self.show_toast(format!("Couldn't diff: {error:#}"), true, cx),
        }
        cx.notify();
    }
}

/// Every path the turns changed, in first-seen order.
fn changed_files(turns: &[Checkpoint]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for (_, path) in turns.iter().flat_map(|t| &t.files) {
        if !files.contains(path) {
            files.push(path.clone());
        }
    }
    files
}

fn describe_files(files: &[String]) -> String {
    match files.len() {
        0 => "no files changed".into(),
        1..=4 => format!("restores {}", files.join(", ")),
        n => format!("restores {}, and {} more", files[..3].join(", "), n - 3),
    }
}

impl CrackView {
    pub(super) fn recover_items(&self) -> Vec<PickerItem> {
        let mut items = Vec::new();
        for (id, title) in &self.interrupted {
            items.push(PickerItem {
                id: format!("resume:{id}"),
                title: format!("Continue: {title}"),
                description: Some(
                    "open it and ask the model to check the files and carry on".into(),
                ),
                category: None,
                footer: None,
                current: false,
                disabled: false,
            });
        }
        items.push(PickerItem {
            id: "dismiss".into(),
            title: "Leave them".into(),
            description: Some(
                "they're saved with a note; undo restores any files the turn changed".into(),
            ),
            category: None,
            footer: None,
            current: false,
            disabled: false,
        });
        items
    }

    pub(super) fn recover(&mut self, choice: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = choice.strip_prefix("resume:") else {
            self.interrupted.clear();
            return;
        };
        self.interrupted.retain(|(i, _)| i != id);
        match self.tab_with_session(id) {
            Some(tab) => self.switch_tab(tab, window, cx),
            None => match crate::session::Session::load(id) {
                Ok(session) => {
                    if !self.session.is_empty() {
                        self.new_tab(window, cx);
                    }
                    self.session = session;
                    self.follow = true;
                }
                Err(error) => {
                    self.show_toast(format!("{error:#}"), true, cx);
                    return;
                }
            },
        }
        self.submit_text(
            "crack closed before you finished the last request. Check the current state first (files may be partly changed), then continue."
                .into(),
            window,
            cx,
        );
    }
}
