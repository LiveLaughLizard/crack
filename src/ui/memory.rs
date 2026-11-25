//! The Memory panel: view, review, pin, edit and delete what crack remembers about
//! this project; `/remember`; and exporting to `.crack/MEMORY.md`.

use gpui::*;

use super::picker::{PickerItem, PickerKind};
use super::view::CrackView;

fn item(
    id: String,
    title: String,
    description: Option<String>,
    category: Option<&str>,
) -> PickerItem {
    PickerItem {
        id,
        title,
        description,
        category: category.map(str::to_owned),
        footer: None,
        current: false,
        disabled: false,
    }
}

impl CrackView {
    fn project(&self) -> String {
        crate::memory::project_key(&self.ws.cwd)
    }

    pub(super) fn memory_items(&self) -> Vec<PickerItem> {
        let list = crate::memory::get()
            .list(&self.project())
            .unwrap_or_default();
        if list.is_empty() {
            return vec![PickerItem {
                disabled: true,
                ..item(
                    "none".into(),
                    "Nothing remembered yet".into(),
                    Some(
                        "The model saves facts as it works; /remember <text> adds one yourself."
                            .into(),
                    ),
                    None,
                )
            }];
        }
        list.into_iter()
            .map(|m| {
                let mut notes = vec![m.kind.clone()];
                if m.project.is_empty() {
                    notes.push("global".into());
                }
                if m.pinned {
                    notes.push("pinned".into());
                }
                let when = chrono::DateTime::from_timestamp(m.created, 0)
                    .map(|d| d.with_timezone(&chrono::Local).format("%b %-d").to_string())
                    .unwrap_or_default();
                notes.push(when);
                let category = if m.reviewed {
                    "Remembered"
                } else {
                    "New: review these"
                };
                item(
                    format!("mem:{}", m.id),
                    m.text,
                    Some(notes.join(" \u{b7} ")),
                    Some(category),
                )
            })
            .collect()
    }

    pub(super) fn memory_action_items(&self) -> Vec<PickerItem> {
        let Some(memory) = self
            .memory_target
            .and_then(|id| crate::memory::get().get(id).ok().flatten())
        else {
            return Vec::new();
        };
        let mut items = Vec::new();
        if !memory.reviewed {
            items.push(item(
                "keep".into(),
                "Keep".into(),
                Some("Mark it reviewed".into()),
                None,
            ));
        }
        items.push(if memory.pinned {
            item(
                "unpin".into(),
                "Unpin".into(),
                Some("Recall it only when relevant".into()),
                None,
            )
        } else {
            item(
                "pin".into(),
                "Pin".into(),
                Some("Remind the model of it every time".into()),
                None,
            )
        });
        items.push(item(
            "edit".into(),
            "Edit".into(),
            Some("Put it in the prompt to change and save again".into()),
            None,
        ));
        if !memory.session.is_empty() {
            items.push(item(
                "source".into(),
                "Open the session it came from".into(),
                memory
                    .reason
                    .is_empty()
                    .then_some(String::new())
                    .or(Some(format!("why: {}", memory.reason))),
                None,
            ));
        }
        items.push(item("delete".into(), "Delete".into(), None, None));
        items
    }

    /// Enter on a memory: what to do with it.
    pub(super) fn open_memory(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = id.strip_prefix("mem:").and_then(|n| n.parse().ok()) else {
            return;
        };
        self.memory_target = Some(id);
        self.open_picker(PickerKind::MemoryAction, window, cx);
    }

    pub(super) fn memory_action(
        &mut self,
        action: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.memory_target.take() else {
            return;
        };
        let memories = crate::memory::get();
        let result = match action {
            "keep" => memories.mark_reviewed(id),
            "pin" => memories.set_pinned(id, true),
            "unpin" => memories.set_pinned(id, false),
            "delete" => memories.delete(id),
            "edit" => {
                if let Ok(Some(memory)) = memories.get(id) {
                    let _ = memories.delete(id);
                    let text = format!("/remember {}", memory.text);
                    self.input.update(cx, |input, cx| input.set_text(text, cx));
                    self.show_toast("Edit it and press enter to save it again.", false, cx);
                }
                return;
            }
            "source" => {
                if let Ok(Some(memory)) = memories.get(id) {
                    match crate::session::Session::load(&memory.session) {
                        Ok(session) => {
                            self.new_tab(window, cx);
                            self.session = session;
                            self.follow = true;
                        }
                        Err(_) => self.show_toast("That session was deleted.", true, cx),
                    }
                }
                return;
            }
            _ => return,
        };
        if let Err(error) = result {
            self.show_toast(format!("{error:#}"), true, cx);
        }
        // Back to the list, so several can be reviewed in a row.
        self.open_picker(PickerKind::Memory, window, cx);
    }

    /// `/remember <text>`: the user's own memory, reviewed already.
    pub(super) fn remember(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = text.trim();
        if text.is_empty() {
            self.show_toast("Write what to remember: /remember <text>", true, cx);
            return;
        }
        if crate::policy::looks_secret(text) {
            self.show_toast(
                "That looks like a secret; memories aren't the place for credentials.",
                true,
                cx,
            );
            return;
        }
        match crate::memory::get().add(
            &self.project(),
            "project",
            text,
            "",
            &self.session.id,
            false,
        ) {
            Ok(_) => self.show_toast("Remembered.", false, cx),
            Err(error) => self.show_toast(format!("{error:#}"), true, cx),
        }
    }

    /// Writes `.crack/MEMORY.md` for the team to review and commit.
    pub(super) fn export_memory(&mut self, cx: &mut Context<Self>) {
        let root = crate::context::project_root(&self.ws.cwd);
        let path = root.join(".crack/MEMORY.md");
        let result = crate::memory::get()
            .export(&self.project())
            .and_then(|text| {
                std::fs::create_dir_all(root.join(".crack"))?;
                std::fs::write(&path, text)?;
                Ok(())
            });
        match result {
            Ok(()) => self.show_toast(
                format!(
                    "Wrote {}",
                    path.strip_prefix(&root).unwrap_or(&path).display()
                ),
                false,
                cx,
            ),
            Err(error) => self.show_toast(format!("Couldn't export: {error:#}"), true, cx),
        }
    }

    /// ctrl-d twice on a memory in the panel.
    pub(super) fn delete_memory(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(n) = id.strip_prefix("mem:").and_then(|n| n.parse().ok()) {
            let _ = crate::memory::get().delete(n);
            if let Some((picker, _)) = &self.picker {
                let id = id.to_owned();
                picker.update(cx, |picker, cx| picker.remove_item(&id, cx));
            }
        }
    }
}
