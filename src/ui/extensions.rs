//! Extensions (`/extensions`): popular skill packs and MCP servers, installed with one
//! click. Nothing is installed until the user picks it.

use gpui::*;

use super::picker::{PickerItem, PickerKind};
use super::view::CrackView;
use crate::catalog::{self, Status};

fn item(
    id: String,
    title: String,
    description: String,
    category: &str,
    current: bool,
) -> PickerItem {
    PickerItem {
        id,
        title,
        description: Some(description),
        category: Some(category.to_owned()),
        footer: None,
        current,
        disabled: false,
    }
}

impl CrackView {
    pub(super) fn extension_items(&self) -> Vec<PickerItem> {
        let mut items: Vec<PickerItem> = catalog::CATALOG
            .iter()
            .map(|entry| {
                let status = catalog::status(entry, &self.ws.config);
                let installed = status != Status::NotInstalled;
                let detail = match &status {
                    Status::Skills { skills, .. } => format!(" \u{b7} {} skills", skills.len()),
                    _ => String::new(),
                };
                item(
                    entry.id.into(),
                    entry.name.into(),
                    format!(
                        "{} \u{b7} {} \u{b7} {}{detail} \u{2014} {}",
                        entry.kind(),
                        entry.author,
                        entry.license,
                        entry.description
                    ),
                    if installed { "Installed" } else { "Available" },
                    installed,
                )
            })
            .collect();
        // Installed first.
        items.sort_by_key(|i| !i.current);
        items
    }

    pub(super) fn extension_action_items(&self) -> Vec<PickerItem> {
        let Some(entry) = self.extension_target.and_then(catalog::find) else {
            return Vec::new();
        };
        let status = catalog::status(entry, &self.ws.config);
        let mut items = Vec::new();
        let source = match &entry.source {
            catalog::Source::Skills { repo, .. } => format!(
                "Downloads from github.com/{repo}. Skills are third-party instructions the model follows."
            ),
            catalog::Source::Mcp { url: Some(url), .. } => {
                format!("Adds a hosted MCP server: {url}")
            }
            catalog::Source::Mcp { command, .. } => {
                format!("Adds an MCP server that runs `{}`", command.join(" "))
            }
        };
        match status {
            Status::NotInstalled => {
                items.push(item(
                    "install".into(),
                    format!("Install {}", entry.name),
                    source,
                    "Action",
                    false,
                ));
            }
            Status::Skills { commit, skills } => {
                items.push(item(
                    "update".into(),
                    "Update to the latest version".into(),
                    format!(
                        "Installed at {}: {}",
                        &commit[..commit.len().min(8)],
                        skills.join(", ")
                    ),
                    "Action",
                    false,
                ));
                items.push(item(
                    "remove".into(),
                    "Remove".into(),
                    String::new(),
                    "Action",
                    false,
                ));
            }
            Status::Mcp => {
                items.push(item(
                    "remove".into(),
                    "Remove".into(),
                    source,
                    "Action",
                    false,
                ));
            }
        }
        items.push(item(
            "page".into(),
            "Open its page".into(),
            format!("{} ({})", entry.homepage, entry.license),
            "Action",
            false,
        ));
        items
    }

    /// Enter on an extension: what to do with it.
    pub(super) fn open_extension(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.extension_target = catalog::find(id).map(|e| e.id);
        self.open_picker(PickerKind::ExtensionAction, window, cx);
    }

    pub(super) fn extension_action(&mut self, action: &str, cx: &mut Context<Self>) {
        let Some(entry) = self.extension_target.take().and_then(catalog::find) else {
            return;
        };
        if action == "page" {
            cx.open_url(entry.homepage);
            return;
        }
        let remove = action == "remove";
        self.show_toast(
            format!(
                "{} {}\u{2026}",
                if remove { "Removing" } else { "Installing" },
                entry.name
            ),
            false,
            cx,
        );
        // git runs off the UI thread.
        let task = crate::runtime::spawn(async move {
            tokio::task::spawn_blocking(move || {
                if remove {
                    catalog::remove(entry)
                } else {
                    catalog::install(entry)
                }
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                match result {
                    Ok(Ok(Ok(message))) => {
                        // Pick up the new skills and MCP servers.
                        if let Ok(config) = crate::config::Config::load(&view.ws.cwd) {
                            view.ws.config = config;
                        }
                        view.ws.reload_context();
                        view.connect_mcp(None, cx);
                        view.show_toast(message, false, cx);
                    }
                    Ok(Ok(Err(error))) => view.show_toast(format!("{error:#}"), true, cx),
                    _ => view.show_toast(format!("Couldn't change {}.", entry.name), true, cx),
                }
                cx.notify();
            });
        })
        .detach();
    }
}
