//! Extensions (`/extensions`): popular skill packs, plugins and MCP servers, plus the
//! plugins of any marketplace the user added. Nothing is installed until the user picks
//! it, and a plugin's hooks and MCP servers need the user's approval of exactly what
//! they run.

use gpui::*;

use super::picker::{PickerItem, PickerKind};
use super::view::CrackView;
use crate::catalog::{self, Source, Status};
use crate::plugins;

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

fn info(text: String) -> PickerItem {
    PickerItem {
        disabled: true,
        ..item(
            String::new(),
            text,
            String::new(),
            "Runs on your machine",
            false,
        )
    }
}

/// What a plugin install is waiting on: the user's approval of its hooks.
pub(super) struct PluginApproval {
    /// A fetched plugin (install or update); `None` approves an installed one.
    pub pending: Option<plugins::Pending>,
    pub key: String,
    pub review: plugins::Review,
    /// The catalog entry it came from, to retire an older skills-only copy.
    pub catalog_id: Option<&'static str>,
}

/// An installed plugin's state, by `name@marketplace`.
fn installed(key: &str) -> Option<plugins::Installed> {
    plugins::State::load().plugins.get(key).cloned()
}

impl CrackView {
    pub(super) fn extension_items(&self) -> Vec<PickerItem> {
        let state = plugins::State::load();
        let waiting: Vec<String> = plugins::untrusted().into_iter().map(|(k, _)| k).collect();
        let mut items: Vec<PickerItem> = Vec::new();
        for entry in catalog::CATALOG {
            let status = catalog::status(entry, &self.ws.config);
            let installed = status != Status::NotInstalled;
            let detail = match &status {
                Status::Skills { skills, .. } => format!(" \u{b7} {} skills", skills.len()),
                Status::Plugin {
                    key,
                    version,
                    enabled,
                } => format!(
                    " \u{b7} {version}{}{}",
                    if *enabled { "" } else { " \u{b7} disabled" },
                    if waiting.contains(key) {
                        " \u{b7} hooks need approval"
                    } else {
                        ""
                    }
                ),
                _ => String::new(),
            };
            items.push(item(
                format!("cat:{}", entry.id),
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
            ));
        }
        // Plugins from marketplaces the user added (the catalog's own are listed above).
        for listing in plugins::listings(&state) {
            let source = &state.marketplaces[&listing.marketplace].source;
            if catalog::CATALOG.iter().any(|e| {
                matches!(e.source, Source::Plugin { repo } if repo == source)
                    && e.id == listing.name
            }) {
                continue;
            }
            let key = format!("{}@{}", listing.name, listing.marketplace);
            let plugin = state.plugins.get(&key);
            let detail = match plugin {
                Some(p) => format!(
                    " \u{b7} {}{}{}",
                    p.version,
                    if p.enabled { "" } else { " \u{b7} disabled" },
                    if waiting.contains(&key) {
                        " \u{b7} hooks need approval"
                    } else {
                        ""
                    }
                ),
                None if !listing.version.is_empty() => format!(" \u{b7} {}", listing.version),
                None => String::new(),
            };
            items.push(item(
                format!("plugin:{key}"),
                listing.name.clone(),
                format!(
                    "plugin \u{b7} {}{detail} \u{2014} {}",
                    listing.marketplace, listing.description
                ),
                if plugin.is_some() {
                    "Installed"
                } else {
                    "From your marketplaces"
                },
                plugin.is_some(),
            ));
        }
        items.sort_by_key(|i| !i.current);
        items.push(item(
            "add-marketplace".into(),
            "Add a marketplace\u{2026}".into(),
            "A repository with .claude-plugin/marketplace.json: owner/repo, a git URL or a folder"
                .into(),
            "Marketplaces",
            false,
        ));
        for (name, market) in &state.marketplaces {
            items.push(item(
                format!("market:{name}"),
                name.clone(),
                market.source.clone(),
                "Marketplaces",
                false,
            ));
        }
        items
    }

    pub(super) fn extension_action_items(&self) -> Vec<PickerItem> {
        let Some(target) = self.extension_target.clone() else {
            return Vec::new();
        };
        let mut items = Vec::new();
        let action = |id: &str, title: String, description: String| {
            item(id.into(), title, description, "Action", false)
        };
        let plugin_actions = |items: &mut Vec<PickerItem>, key: &str| {
            let Some(p) = installed(key) else {
                return false;
            };
            items.push(action(
                "update",
                "Update to the latest version".into(),
                format!("Installed: {}", p.version),
            ));
            if plugins::untrusted().iter().any(|(k, _)| k == key) {
                items.push(action(
                    "trust",
                    "Review and allow its hooks".into(),
                    "Its hooks and MCP servers are off until you allow them".into(),
                ));
            }
            if p.enabled {
                items.push(action(
                    "disable",
                    "Disable".into(),
                    "Keep it installed but unused".into(),
                ));
            } else {
                items.push(action("enable", "Enable".into(), String::new()));
            }
            items.push(action("remove", "Remove".into(), String::new()));
            true
        };
        if let Some(id) = target.strip_prefix("cat:") {
            let Some(entry) = catalog::find(id) else {
                return Vec::new();
            };
            let source = match &entry.source {
                Source::Skills { repo, .. } => format!(
                    "Downloads from github.com/{repo}. Skills are third-party instructions the model follows."
                ),
                Source::Plugin { repo } => format!(
                    "Adds github.com/{repo} as a marketplace and installs the plugin. You approve any hooks it runs."
                ),
                Source::Mcp { url: Some(url), .. } => format!("Adds a hosted MCP server: {url}"),
                Source::Mcp { command, .. } => {
                    format!("Adds an MCP server that runs `{}`", command.join(" "))
                }
            };
            match catalog::status(entry, &self.ws.config) {
                Status::NotInstalled => {
                    items.push(action("install", format!("Install {}", entry.name), source))
                }
                Status::Skills { commit, skills } => {
                    items.push(action(
                        "update",
                        "Update to the latest version".into(),
                        format!(
                            "Installed at {}: {}",
                            &commit[..commit.len().min(8)],
                            skills.join(", ")
                        ),
                    ));
                    items.push(action("remove", "Remove".into(), String::new()));
                }
                Status::Mcp => items.push(action("remove", "Remove".into(), source)),
                Status::Plugin { key, .. } => {
                    plugin_actions(&mut items, &key);
                }
            }
            items.push(action(
                "page",
                "Open its page".into(),
                format!("{} ({})", entry.homepage, entry.license),
            ));
        } else if let Some(key) = target.strip_prefix("plugin:") {
            if !plugin_actions(&mut items, key) {
                items.push(action(
                    "install",
                    format!("Install {key}"),
                    "You approve any hooks or MCP servers it runs".into(),
                ));
            }
        } else if let Some(name) = target.strip_prefix("market:") {
            items.push(action(
                "market-update",
                "Update the plugin list".into(),
                String::new(),
            ));
            items.push(action(
                "market-remove",
                "Remove this marketplace".into(),
                "Its plugins must be removed first".into(),
            ));
            let _ = name;
        }
        items
    }

    /// Enter on an extension: what to do with it.
    pub(super) fn open_extension(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if id == "add-marketplace" {
            self.close_picker(window, cx);
            self.input.update(cx, |input, cx| {
                input.set_text("/plugin marketplace add ".to_owned(), cx)
            });
            self.show_toast(
                "Type the marketplace (owner/repo, a git URL or a folder), then enter.",
                false,
                cx,
            );
            return;
        }
        self.extension_target = Some(id.to_owned());
        self.open_picker(PickerKind::ExtensionAction, window, cx);
    }

    pub(super) fn extension_action(
        &mut self,
        action: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.extension_target.take() else {
            return;
        };
        let entry = target.strip_prefix("cat:").and_then(catalog::find);
        if action == "page" {
            if let Some(entry) = entry {
                cx.open_url(entry.homepage);
            }
            return;
        }
        // Which plugin this is about: installed key, or what to install.
        let plugin_key = match (entry, target.strip_prefix("plugin:")) {
            (Some(e), _) => match catalog::status(e, &self.ws.config) {
                Status::Plugin { key, .. } => Some(key),
                _ => None,
            },
            (None, Some(key)) => Some(key.to_owned()),
            _ => None,
        };
        let is_plugin = target.starts_with("plugin:")
            || entry.is_some_and(|e| matches!(e.source, Source::Plugin { .. }));
        match action {
            "install" | "update" if is_plugin => {
                let spec = plugin_key.clone();
                self.fetch_plugin(entry, spec, window, cx);
            }
            "trust" => {
                if let Some(key) = plugin_key
                    && let Some((_, review)) =
                        plugins::untrusted().into_iter().find(|(k, _)| *k == key)
                {
                    self.plugin_approval = Some(PluginApproval {
                        pending: None,
                        key,
                        review,
                        catalog_id: None,
                    });
                    self.open_picker(PickerKind::PluginApproval, window, cx);
                }
            }
            "enable" | "disable" | "remove" if is_plugin => {
                let Some(key) = plugin_key else { return };
                let result = match action {
                    "enable" => plugins::set_enabled(&key, true),
                    "disable" => plugins::set_enabled(&key, false),
                    _ => plugins::uninstall(&key),
                };
                self.after_extension_change(result, cx);
            }
            "market-update" | "market-remove" => {
                let Some(name) = target.strip_prefix("market:").map(str::to_owned) else {
                    return;
                };
                let remove = action == "market-remove";
                self.run_extension_job(
                    move || {
                        if remove {
                            plugins::remove_marketplace(&name).map(|_| format!("Removed {name}."))
                        } else {
                            plugins::update_marketplace(&name).map(|_| format!("Updated {name}."))
                        }
                    },
                    cx,
                );
            }
            _ => {
                let Some(entry) = entry else { return };
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
                self.run_extension_job(
                    move || {
                        if remove {
                            catalog::remove(entry)
                        } else {
                            catalog::install(entry)
                        }
                    },
                    cx,
                );
            }
        }
    }

    /// Fetches a plugin in the background, then asks about its hooks (if any).
    fn fetch_plugin(
        &mut self,
        entry: Option<&'static catalog::Entry>,
        spec: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.show_toast("Fetching the plugin\u{2026}", false, cx);
        let task = crate::runtime::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let spec = match (spec, entry) {
                    (Some(spec), _) => spec,
                    (None, Some(entry)) => catalog::plugin_spec(entry)?,
                    (None, None) => anyhow::bail!("nothing to install"),
                };
                plugins::prepare(&spec)
            })
            .await
        });
        let catalog_id = entry.map(|e| e.id);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |view, window, cx| match result {
                Ok(Ok(Ok(pending))) => {
                    let already = installed(&pending.key)
                        .and_then(|p| p.trusted)
                        .is_some_and(|t| t == pending.review.fingerprint);
                    if pending.review.runs_code() && !already {
                        view.plugin_approval = Some(PluginApproval {
                            key: pending.key.clone(),
                            review: pending.review.clone(),
                            pending: Some(pending),
                            catalog_id,
                        });
                        view.open_picker(PickerKind::PluginApproval, window, cx);
                    } else {
                        let result = plugins::commit_install(pending, false);
                        if result.is_ok()
                            && let Some(id) = catalog_id
                        {
                            catalog::retire_skill_pack(id);
                        }
                        view.after_extension_change(result, cx);
                    }
                }
                Ok(Ok(Err(error))) => view.show_toast(format!("{error:#}"), true, cx),
                _ => view.show_toast("Couldn't fetch the plugin.", true, cx),
            });
        })
        .detach();
    }

    pub(super) fn plugin_approval_items(&self) -> Vec<PickerItem> {
        let Some(approval) = &self.plugin_approval else {
            return Vec::new();
        };
        let mut items: Vec<PickerItem> = approval
            .review
            .hooks
            .iter()
            .map(|h| info(format!("hook  {h}")))
            .chain(
                approval
                    .review
                    .mcp
                    .iter()
                    .map(|m| info(format!("MCP   {m}"))),
            )
            .collect();
        let installing = approval.pending.is_some();
        items.push(item(
            "allow".into(),
            if installing {
                format!("Allow these and install {}", approval.key)
            } else {
                format!("Allow these for {}", approval.key)
            },
            "They run with your permissions whenever their events happen".into(),
            "Decide",
            false,
        ));
        if installing {
            items.push(item(
                "without".into(),
                "Install with hooks and MCP servers off".into(),
                "Skills, commands and agents still work; allow the rest later".into(),
                "Decide",
                false,
            ));
        }
        items.push(item(
            "cancel".into(),
            "Cancel".into(),
            String::new(),
            "Decide",
            false,
        ));
        items
    }

    pub(super) fn plugin_approval_choice(&mut self, choice: &str, cx: &mut Context<Self>) {
        let Some(approval) = self.plugin_approval.take() else {
            return;
        };
        if choice == "cancel" {
            return;
        }
        let allow = choice == "allow";
        let result = match approval.pending {
            Some(pending) => {
                let result = plugins::commit_install(pending, allow);
                if result.is_ok()
                    && let Some(id) = approval.catalog_id
                {
                    catalog::retire_skill_pack(id);
                }
                result
            }
            None if allow => {
                plugins::trust(&approval.key).map(|_| format!("Allowed {}'s hooks.", approval.key))
            }
            None => return,
        };
        self.after_extension_change(result, cx);
    }

    /// Runs blocking extension work off the UI thread, then reloads.
    fn run_extension_job(
        &mut self,
        job: impl FnOnce() -> anyhow::Result<String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let task = crate::runtime::spawn(async move { tokio::task::spawn_blocking(job).await });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |view, cx| {
                let result = match result {
                    Ok(Ok(result)) => result,
                    _ => Err(anyhow::anyhow!("it stopped unexpectedly")),
                };
                view.after_extension_change(result, cx);
            });
        })
        .detach();
    }

    /// After installing, removing or toggling: pick up skills, commands, hooks and MCP
    /// servers right away, and say what happened.
    pub(super) fn after_extension_change(
        &mut self,
        result: anyhow::Result<String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(message) => {
                if let Ok(config) = crate::config::Config::load(&self.ws.cwd) {
                    self.ws.config = config;
                }
                self.ws.reload_context();
                self.connect_mcp(None, cx);
                self.show_toast(message, false, cx);
            }
            Err(error) => self.show_toast(format!("{error:#}"), true, cx),
        }
        cx.notify();
    }

    /// `/plugin ...` in the prompt: marketplaces and plugins by name.
    pub(super) fn plugin_slash(&mut self, args: &str, window: &mut Window, cx: &mut Context<Self>) {
        let words: Vec<String> = args.split_whitespace().map(str::to_owned).collect();
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        match words.as_slice() {
            [] | ["list"] => self.open_picker(PickerKind::Extensions, window, cx),
            ["marketplace", "add", source] => {
                let source = source.to_string();
                self.show_toast(format!("Adding {source}\u{2026}"), false, cx);
                self.run_extension_job(
                    move || plugins::add_marketplace(&source).map(|n| format!("Added the {n} marketplace. Its plugins are in /extensions.")),
                    cx,
                );
            }
            ["marketplace", "remove", name] => {
                let name = name.to_string();
                self.run_extension_job(move || plugins::remove_marketplace(&name).map(|_| format!("Removed {name}.")), cx);
            }
            ["install" | "update", spec] => self.fetch_plugin(None, Some(spec.to_string()), window, cx),
            ["enable", spec] => self.after_extension_change(plugins::set_enabled(spec, true), cx),
            ["disable", spec] => self.after_extension_change(plugins::set_enabled(spec, false), cx),
            ["remove" | "uninstall", spec] => self.after_extension_change(plugins::uninstall(spec), cx),
            _ => self.show_toast(
                "/plugin [marketplace add|remove <source> | install|update|enable|disable|remove <name>]",
                true,
                cx,
            ),
        }
    }
}
