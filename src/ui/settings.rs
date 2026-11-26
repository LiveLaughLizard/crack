//! Settings (`/settings`): theme, font, default effort, mode and permissions, by mouse
//! or keyboard. Choices apply at once; theme and font are remembered in crack's state,
//! permissions are written to the user's own config file.

use gpui::*;

use super::picker::{PickerItem, PickerKind};
use super::theme;
use super::view::CrackView;

fn item(id: &str, title: &str, value: String, current: bool) -> PickerItem {
    PickerItem {
        id: id.into(),
        title: title.into(),
        description: Some(value),
        category: None,
        footer: None,
        current,
        disabled: false,
    }
}

impl CrackView {
    fn permission(&self, key: &str) -> &str {
        let p = &self.ws.config.permissions;
        let value = match key {
            "edits" => p.edits.as_deref(),
            "bash" => p.bash.as_deref(),
            _ => p.webfetch.as_deref(),
        };
        value.unwrap_or(if key == "bash" { "ask" } else { "allow" })
    }

    pub(super) fn settings_items(&self) -> Vec<PickerItem> {
        let sandbox = crate::policy::Sandbox::resolve(&self.ws.config.permissions);
        let effort = self.selection.effort.as_str().to_owned();
        let mut items = vec![
            item(
                "theme",
                "Theme",
                if theme::is_light() {
                    "light".into()
                } else {
                    "dark".into()
                },
                false,
            ),
            item("font", "Font", theme::font().to_string(), false),
            item("effort", "Reasoning effort", effort, false),
            item(
                "explorer_model",
                "Explorer model (explore subagents)",
                self.ws
                    .config
                    .explorer_model
                    .clone()
                    .unwrap_or_else(|| "auto: a smaller model from the same provider".into()),
                false,
            ),
            item("mode", "Mode", self.selection.mode.label().into(), false),
            item(
                "edits",
                "File edits in the project",
                self.permission("edits").into(),
                false,
            ),
            item(
                "bash",
                "Shell commands not on the safe list",
                format!("{} (the sandbox lets them run)", self.permission("bash")),
                false,
            ),
        ];
        if cfg!(feature = "cloud") {
            items.push(item(
                "webfetch",
                "Web fetches",
                self.permission("webfetch").into(),
                false,
            ));
        }
        if cfg!(feature = "cloud") {
            let on_off = |v: Option<bool>| if v.unwrap_or(false) { "on" } else { "off" };
            items.push(item(
                "server_compaction",
                "Claude: server-side compaction (beta)",
                on_off(self.ws.config.server_compaction).into(),
                false,
            ));
            items.push(item(
                "clear_tool_results",
                "Claude: clear old tool results (beta)",
                on_off(self.ws.config.clear_tool_results).into(),
                false,
            ));
        }
        if !cfg!(feature = "force-sandbox") {
            items.push(item(
                "sandbox",
                "Sandbox",
                if sandbox.wanted {
                    "on".into()
                } else {
                    "off".into()
                },
                false,
            ));
        }
        items.push(item(
            "network",
            "Network inside the sandbox",
            if sandbox.network {
                "allowed".into()
            } else {
                "blocked".into()
            },
            false,
        ));
        for i in &mut items {
            i.category = Some(match i.id.as_str() {
                "theme" | "font" => "Appearance".into(),
                "effort" | "explorer_model" | "mode" | "server_compaction"
                | "clear_tool_results" => "Agent".into(),
                _ => "Permissions".into(),
            });
        }
        items
    }

    pub(super) fn settings_value_items(&self, cx: &App) -> Vec<PickerItem> {
        let choice = |id: &str, title: &str, current: bool| item(id, title, String::new(), current);
        match self.settings_target.as_deref() {
            Some("theme") => vec![
                choice("dark", "Dark", !theme::is_light()),
                choice("light", "Light", theme::is_light()),
            ],
            Some("font") => {
                let installed = cx.text_system().all_font_names();
                let current = theme::font();
                theme::MONOSPACE_FONTS
                    .iter()
                    .filter(|f| installed.iter().any(|i| i == *f))
                    .map(|f| choice(f, f, current.as_ref() == *f))
                    .chain(std::iter::once(choice(
                        "monospace",
                        "System monospace",
                        current.as_ref() == "monospace",
                    )))
                    .collect()
            }
            Some("mode") => {
                let mode = self.selection.mode;
                #[allow(unused_mut)]
                let mut list = vec![
                    choice(
                        "normal",
                        "Normal: ask before risky actions",
                        mode == crate::policy::Mode::Normal,
                    ),
                    choice(
                        "accept-edits",
                        "Accept edits: file edits never ask",
                        mode == crate::policy::Mode::AcceptEdits,
                    ),
                    choice(
                        "review",
                        "Review edits: see each change and keep it hunk by hunk",
                        mode == crate::policy::Mode::Review,
                    ),
                ];
                #[cfg(feature = "yolo")]
                list.push(choice(
                    "yolo",
                    "YOLO: run everything except deny rules",
                    mode == crate::policy::Mode::Yolo,
                ));
                list
            }
            Some(key @ ("edits" | "webfetch")) => {
                let now = self.permission(key);
                vec![
                    choice("allow", "Allow", now == "allow"),
                    choice("ask", "Ask first", now == "ask"),
                ]
            }
            Some("explorer_model") => {
                let now = self.ws.config.explorer_model.clone();
                let mut list = vec![item(
                    "auto",
                    "Auto",
                    "a smaller, faster model from the main model's provider".into(),
                    now.is_none(),
                )];
                list.extend(
                    self.model_items()
                        .into_iter()
                        .filter(|m| {
                            m.category.as_deref() != Some("Recent") && !m.id.starts_with("connect:")
                        })
                        .map(|mut m| {
                            m.current = now.as_deref() == Some(m.id.as_str());
                            m
                        }),
                );
                list
            }
            Some("bash") => {
                let now = self.permission("bash");
                vec![
                    choice("ask", "Ask first", now == "ask"),
                    choice("allow", "Allow", now == "allow"),
                ]
            }
            _ => Vec::new(),
        }
    }

    /// Enter on a setting: open its choices (or toggle it).
    pub(super) fn open_setting(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        match id {
            "effort" => self.open_picker(PickerKind::Effort, window, cx),
            "sandbox" => {
                self.toggle_sandbox(false, cx);
                self.open_picker(PickerKind::Settings, window, cx);
            }
            "network" => {
                self.toggle_sandbox(true, cx);
                self.open_picker(PickerKind::Settings, window, cx);
            }
            key @ ("server_compaction" | "clear_tool_results") => {
                let config = &mut self.ws.config;
                let slot = if key == "server_compaction" {
                    &mut config.server_compaction
                } else {
                    &mut config.clear_tool_results
                };
                let on = !slot.unwrap_or(false);
                *slot = Some(on);
                match crate::config::set_user_top_value(key, on.into()) {
                    Ok(path) => self.show_toast(format!("Saved to {}", path.display()), false, cx),
                    Err(error) => self.show_toast(format!("Couldn't save: {error:#}"), true, cx),
                }
                self.open_picker(PickerKind::Settings, window, cx);
            }
            other => {
                self.settings_target = Some(other.to_owned());
                self.open_picker(PickerKind::SettingsValue, window, cx);
            }
        }
    }

    pub(super) fn apply_setting(
        &mut self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(key) = self.settings_target.take() else {
            return;
        };
        match key.as_str() {
            "theme" => {
                theme::set_light(value == "light");
                crate::state::State::update(|s| s.theme = Some(value.to_owned()));
                cx.refresh_windows();
            }
            "font" => {
                theme::set_font(value);
                crate::state::State::update(|s| s.font = Some(value.to_owned()));
                cx.refresh_windows();
            }
            "mode" => match value {
                "accept-edits" => self.set_mode(crate::policy::Mode::AcceptEdits, cx),
                "review" => self.set_mode(crate::policy::Mode::Review, cx),
                #[cfg(feature = "yolo")]
                "yolo" => self.set_mode(crate::policy::Mode::Yolo, cx),
                _ => self.set_mode(crate::policy::Mode::Normal, cx),
            },
            "explorer_model" => {
                let saved = if value == "auto" {
                    self.ws.config.explorer_model = None;
                    crate::config::remove_user_top_value(&["explorer_model", "subagent_model"])
                } else {
                    self.ws.config.explorer_model = Some(value.to_owned());
                    crate::config::remove_user_top_value(&["subagent_model"]).and_then(|_| {
                        crate::config::set_user_top_value("explorer_model", value.into())
                    })
                };
                match saved {
                    Ok(path) => self.show_toast(format!("Saved to {}", path.display()), false, cx),
                    Err(error) => self.show_toast(format!("Couldn't save: {error:#}"), true, cx),
                }
            }
            key @ ("edits" | "bash" | "webfetch") => {
                let permissions = &mut self.ws.config.permissions;
                let slot = match key {
                    "edits" => &mut permissions.edits,
                    "bash" => &mut permissions.bash,
                    _ => &mut permissions.webfetch,
                };
                *slot = Some(value.to_owned());
                match crate::config::set_user_value("permissions", key, value.into()) {
                    Ok(path) => self.show_toast(format!("Saved to {}", path.display()), false, cx),
                    Err(error) => self.show_toast(format!("Couldn't save: {error:#}"), true, cx),
                }
            }
            _ => {}
        }
        self.open_picker(PickerKind::Settings, window, cx);
    }
}
