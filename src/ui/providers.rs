//! The Providers dialog: connect model providers with API keys, cloud sign-in or a
//! local server, by mouse or keyboard.

use super::input::{InputEvent, TextInput};
use super::theme;
use crate::config::{self, Config, ProviderConfig, Setting};
use crate::providers::Registry;
use crate::providers::catalog::{self, Entry, Field, FieldSpec, Group};
use crate::providers::registry::{provider_config, test_connection};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, Hsla, InteractiveElement, IntoElement, KeyBinding, MouseButton, ParentElement,
    Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
    actions, div, px,
};
use std::path::PathBuf;

actions!(
    providers,
    [
        Close, NextField, PrevField, Save, Test, ListUp, ListDown, OpenForm
    ]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Providers");
    let list = Some("ProvidersList");
    cx.bind_keys([
        KeyBinding::new("escape", Close, ctx),
        KeyBinding::new("tab", NextField, ctx),
        KeyBinding::new("shift-tab", PrevField, ctx),
        KeyBinding::new("ctrl-s", Save, ctx),
        KeyBinding::new("ctrl-t", Test, ctx),
        KeyBinding::new("up", ListUp, list),
        KeyBinding::new("down", ListDown, list),
        KeyBinding::new("enter", OpenForm, list),
        KeyBinding::new("right", OpenForm, list),
    ]);
}

pub enum ProvidersEvent {
    /// Settings were saved or removed: the new configuration and providers.
    Changed {
        config: Box<Config>,
        registry: Box<Registry>,
        connected: Option<String>,
    },
    Dismiss,
}

const NEW: &str = "+new";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Info,
    Busy,
    Good,
    Bad,
}

struct Row {
    id: String,
    name: String,
    group: Group,
    connected: bool,
    detail: String,
}

struct Form {
    id: String,
    entry: Entry,
    inputs: Vec<(FieldSpec, Entity<TextInput>)>,
    _subscriptions: Vec<Subscription>,
}

pub struct ProvidersDialog {
    cwd: PathBuf,
    config: Config,
    registry: Registry,
    http: reqwest::Client,
    rows: Vec<Row>,
    selected: usize,
    list_focus: FocusHandle,
    list_scroll: ScrollHandle,
    form: Option<Form>,
    status: Option<(String, Tone)>,
    /// A link shown with the status, e.g. to install a missing CLI.
    status_link: Option<(String, String)>,
    busy: bool,
}

impl EventEmitter<ProvidersEvent> for ProvidersDialog {}

impl Focusable for ProvidersDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.list_focus.clone()
    }
}

fn status_dot(connected: bool) -> (&'static str, Hsla) {
    if connected {
        ("●", theme::success())
    } else {
        ("○", theme::muted())
    }
}

impl ProvidersDialog {
    pub fn new(
        cwd: PathBuf,
        config: Config,
        registry: Registry,
        http: reqwest::Client,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut dialog = Self {
            cwd,
            config,
            registry,
            http,
            rows: Vec::new(),
            selected: 0,
            list_focus: cx.focus_handle(),
            list_scroll: ScrollHandle::new(),
            form: None,
            status: None,
            status_link: None,
            busy: false,
        };
        dialog.rebuild_rows();
        if let Some(index) = initial
            .as_deref()
            .and_then(|id| dialog.rows.iter().position(|r| r.id == id))
        {
            dialog.selected = index;
        } else if let Some(first_unconnected) = dialog.rows.iter().position(|r| !r.connected) {
            // With nothing chosen, start on a provider that still needs setting up.
            if !dialog.rows.iter().any(|r| r.connected) {
                dialog.selected = first_unconnected;
            }
        }
        dialog.load_form(window, cx);
        if initial.is_some() {
            dialog.focus_first_field(window, cx);
        } else {
            window.focus(&dialog.list_focus);
        }
        dialog
    }

    fn rebuild_rows(&mut self) {
        let mut rows: Vec<Row> = self
            .registry
            .providers
            .iter()
            .map(|p| {
                let entry = catalog::entry(&p.id);
                let detail = if p.available {
                    let models = p.models.len();
                    match &p.source {
                        Some(source) if !p.local => format!("{models} models · {source}"),
                        _ => format!("{models} models"),
                    }
                } else if p.local {
                    "not running".into()
                } else {
                    "not connected".into()
                };
                Row {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    group: entry.group,
                    connected: p.available,
                    detail,
                }
            })
            .collect();
        rows.sort_by_key(|r| r.group);
        rows.push(Row {
            id: NEW.into(),
            name: "Add an OpenAI-compatible server".into(),
            group: Group::Local,
            connected: false,
            detail: "vLLM, llama.cpp, LiteLLM, gateways".into(),
        });
        self.rows = rows;
        self.selected = self.selected.min(self.rows.len() - 1);
    }

    fn selected_id(&self) -> String {
        self.rows[self.selected].id.clone()
    }

    /// Builds the form for the selected provider, prefilled from its settings.
    fn load_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.selected_id();
        let is_new = id == NEW;
        let entry = if is_new {
            catalog::entry(NEW)
        } else {
            catalog::entry(&id)
        };
        let fields = if is_new {
            catalog::custom_fields()
        } else {
            entry.fields.clone()
        };
        let effective = if is_new {
            ProviderConfig::default()
        } else {
            provider_config(&self.config, &id)
        };
        let default = provider_config(&Config::default(), &id);
        let saved_key = config::load_credentials()
            .into_iter()
            .find(|(p, _)| *p == id)
            .map(|(_, k)| k);

        let mut inputs = Vec::new();
        let mut subscriptions = Vec::new();
        for spec in fields {
            let value: String = match spec.field {
                Field::ApiKey => saved_key.clone().unwrap_or_default(),
                Field::BaseUrl => effective
                    .base_url
                    .clone()
                    .filter(|u| Some(u) != default.base_url.as_ref() || is_new)
                    .unwrap_or_default(),
                Field::Region => effective.region.clone().unwrap_or_default(),
                Field::Project => effective.project.clone().unwrap_or_default(),
                Field::Profile => effective.profile.clone().unwrap_or_default(),
                Field::Models => {
                    let user = self.config.provider.get(&id).map(|p| p.models.clone());
                    user.unwrap_or_default().join(", ")
                }
                Field::Name => effective
                    .name
                    .clone()
                    .filter(|_| !is_new)
                    .unwrap_or_default(),
            };
            let mut placeholder = spec.placeholder.to_owned();
            // An empty Models field means the built-in list; show it.
            if spec.field == Field::Models && !is_new && !effective.models.is_empty() {
                placeholder = effective.models.join(", ");
            }
            if spec.field == Field::ApiKey
                && saved_key.is_none()
                && let Some(var) = effective
                    .api_key_env
                    .iter()
                    .find(|v| std::env::var(v).is_ok_and(|x| !x.is_empty()))
            {
                placeholder = format!("Using ${var} from the environment");
            }
            let masked = spec.field == Field::ApiKey;
            let input = cx.new(|cx| {
                let mut input = TextInput::new(false, placeholder, cx);
                input.set_text(value, cx);
                input.set_masked(masked, cx);
                input
            });
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                |this: &mut Self, input, event: &InputEvent, window, cx| match event {
                    InputEvent::Submit => this.save(window, cx),
                    InputEvent::Up => this.move_field(input, -1, window, cx),
                    InputEvent::Down => this.move_field(input, 1, window, cx),
                    InputEvent::Changed | InputEvent::PasteImage(_) => {}
                },
            ));
            inputs.push((spec, input));
        }
        self.form = Some(Form {
            id,
            entry,
            inputs,
            _subscriptions: subscriptions,
        });
        self.status = None;
        self.status_link = None;
        cx.notify();
    }

    fn focus_first_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, input)) = self.form.as_ref().and_then(|f| f.inputs.first()) {
            window.focus(&input.focus_handle(cx));
        }
        cx.notify();
    }

    fn focused_field(&self, window: &Window, cx: &App) -> Option<usize> {
        self.form
            .as_ref()?
            .inputs
            .iter()
            .position(|(_, input)| input.focus_handle(cx).is_focused(window))
    }

    fn move_field(
        &mut self,
        from: &Entity<TextInput>,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &self.form else { return };
        let Some(index) = form.inputs.iter().position(|(_, i)| i == from) else {
            return;
        };
        // Tab past the last field wraps to the first; up from the first goes to the list.
        let len = form.inputs.len() as isize;
        let next = if delta > 0 && index as isize + delta >= len {
            0
        } else {
            index as isize + delta
        };
        if next < 0 {
            window.focus(&self.list_focus);
        } else if let Some((_, input)) = form.inputs.get(next as usize) {
            window.focus(&input.focus_handle(cx));
        }
        cx.notify();
    }

    /// The list child that draws row `index`: rows plus the group headers before it.
    fn child_index(&self, index: usize) -> usize {
        let mut headers = 0;
        let mut last = None;
        for row in &self.rows[..=index] {
            if last != Some(row.group) {
                headers += 1;
                last = Some(row.group);
            }
        }
        index + headers
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index == self.selected && self.form.is_some() {
            return;
        }
        self.selected = index;
        self.list_scroll.scroll_to_item(self.child_index(index));
        self.load_form(window, cx);
    }

    fn field_value(&self, field: Field, cx: &App) -> Option<String> {
        let form = self.form.as_ref()?;
        form.inputs
            .iter()
            .find(|(spec, _)| spec.field == field)
            .map(|(_, input)| input.read(cx).text().trim().to_owned())
    }

    /// The provider's settings with the form's values applied.
    fn form_config(&self, cx: &App) -> (String, ProviderConfig) {
        let form = self.form.as_ref().expect("a form is open");
        let mut id = form.id.clone();
        let mut config = if id == NEW {
            let name = self.field_value(Field::Name, cx).unwrap_or_default();
            id = slug(&name);
            ProviderConfig {
                kind: Some("openai".into()),
                name: Some(if name.is_empty() {
                    "OpenAI-compatible".into()
                } else {
                    name
                }),
                discover: Some(true),
                ..Default::default()
            }
        } else {
            provider_config(&self.config, &id)
        };
        let text = |field| self.field_value(field, cx).filter(|v| !v.is_empty());
        if let Some(key) = text(Field::ApiKey) {
            config.api_key = Some(key);
        } else if form.inputs.iter().any(|(s, _)| s.field == Field::ApiKey) {
            // An emptied key field falls back to the environment.
            config.api_key = None;
        }
        let default = provider_config(&Config::default(), &id);
        for (field, slot) in [
            (Field::BaseUrl, &mut config.base_url),
            (Field::Region, &mut config.region),
            (Field::Project, &mut config.project),
            (Field::Profile, &mut config.profile),
        ] {
            if form.inputs.iter().any(|(s, _)| s.field == field) {
                *slot = text(field).or_else(|| match field {
                    Field::BaseUrl => default.base_url.clone(),
                    Field::Region => default.region.clone(),
                    Field::Project => default.project.clone(),
                    _ => None,
                });
            }
        }
        if form.id == NEW && text(Field::ApiKey).is_none() {
            config.auth = Some("none".into());
        }
        if let Some(models) = text(Field::Models) {
            config.models = split_list(&models);
        }
        (id, config)
    }

    fn set_status(&mut self, text: impl Into<String>, tone: Tone, cx: &mut Context<Self>) {
        self.status = Some((text.into(), tone));
        self.status_link = None;
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.form.is_none() {
            return;
        }
        let form = self.form.as_ref().unwrap();
        let is_new = form.id == NEW;
        if is_new
            && self
                .field_value(Field::BaseUrl, cx)
                .unwrap_or_default()
                .is_empty()
        {
            self.set_status("Add the server's base URL first.", Tone::Bad, cx);
            return;
        }
        let (id, effective) = self.form_config(cx);
        let default = provider_config(&Config::default(), &id);
        let mut settings: Vec<(&str, Setting)> = Vec::new();
        for (spec, input) in &form.inputs {
            let value = input.read(cx).text().trim().to_owned();
            match spec.field {
                Field::ApiKey => {}
                Field::Models => settings.push(("models", Setting::List(split_list(&value)))),
                Field::Name => settings.push(("name", Setting::Text(Some(value)))),
                // Values equal to the built-in default aren't written out.
                Field::BaseUrl if Some(&value) == default.base_url.as_ref() && !is_new => {
                    settings.push(("base_url", Setting::Text(None)))
                }
                field => settings.push((field.key(), Setting::Text(Some(value)))),
            }
        }
        if is_new {
            settings.push(("kind", Setting::Text(Some("openai".into()))));
            if effective.auth.as_deref() == Some("none") {
                settings.push(("auth", Setting::Text(Some("none".into()))));
            }
        }
        let key = form
            .inputs
            .iter()
            .any(|(s, _)| s.field == Field::ApiKey)
            .then(|| self.field_value(Field::ApiKey, cx).unwrap_or_default());

        if let Err(error) = config::save_provider(&id, &settings, key.as_deref()) {
            self.set_status(format!("Couldn't save: {error:#}"), Tone::Bad, cx);
            return;
        }
        self.reload(Some(id), window, cx);
    }

    fn disconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.form else { return };
        if form.id == NEW {
            return;
        }
        let id = form.id.clone();
        if let Err(error) = config::forget_provider(&id) {
            self.set_status(format!("Couldn't remove: {error:#}"), Tone::Bad, cx);
            return;
        }
        self.reload(None, window, cx);
        self.set_status(
            "Removed the saved settings for this provider.",
            Tone::Info,
            cx,
        );
    }

    /// Re-reads the config, rebuilds the providers and lists their models.
    fn reload(&mut self, connected: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let config = match Config::load(&self.cwd) {
            Ok(config) => config,
            Err(error) => {
                self.set_status(format!("{error:#}"), Tone::Bad, cx);
                return;
            }
        };
        let mut registry = Registry::load(&config);
        self.busy = true;
        self.set_status("Saved. Checking the connection…", Tone::Busy, cx);
        let client = self.http.clone();
        let task = crate::runtime::spawn(async move {
            registry.discover(&client).await;
            registry
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(registry) = task.await else { return };
            let _ = this.update_in(cx, |dialog, window, cx| {
                dialog.busy = false;
                dialog.config = config.clone();
                dialog.registry = registry.clone();
                let selected_id = connected.clone().unwrap_or_else(|| dialog.selected_id());
                dialog.rebuild_rows();
                if let Some(index) = dialog.rows.iter().position(|r| r.id == selected_id) {
                    dialog.selected = index;
                }
                // Show the saved provider as it now is (a new server becomes its own row).
                dialog.load_form(window, cx);
                window.focus(&dialog.list_focus);
                match connected.as_deref().and_then(|id| registry.find(id)) {
                    Some(p) if p.available => {
                        let text = format!(
                            "Connected to {} ✓  {} models available.",
                            p.name,
                            p.models.len()
                        );
                        dialog.set_status(text, Tone::Good, cx);
                    }
                    Some(p) => {
                        let why = p
                            .note
                            .clone()
                            .unwrap_or_else(|| "it isn't answering".into());
                        dialog.set_status(
                            format!("Saved, but not connected yet: {why}."),
                            Tone::Bad,
                            cx,
                        );
                    }
                    None => {}
                }
                cx.emit(ProvidersEvent::Changed {
                    config: Box::new(config),
                    registry: Box::new(registry),
                    connected,
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn test(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.form.is_none() {
            return;
        }
        let (id, config) = self.form_config(cx);
        let model = config.models.first().cloned();
        self.busy = true;
        self.set_status("Testing the connection…", Tone::Busy, cx);
        let client = self.http.clone();
        let task = crate::runtime::spawn(async move {
            test_connection(&id, &config, model.as_deref(), &client).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |dialog, cx| {
                dialog.busy = false;
                match result {
                    Ok(Ok(message)) => dialog.set_status(
                        format!("{message} Press Save to keep it."),
                        Tone::Good,
                        cx,
                    ),
                    Ok(Err(error)) => dialog.set_status(format!("{error:#}"), Tone::Bad, cx),
                    Err(error) => dialog.set_status(format!("{error}"), Tone::Bad, cx),
                }
            });
        })
        .detach();
    }

    /// Runs the provider's browser sign-in command (gcloud / aws sso).
    fn sign_in(&mut self, cx: &mut Context<Self>) {
        let Some(sign_in) = self.form.as_ref().and_then(|f| f.entry.sign_in.clone()) else {
            return;
        };
        if self.busy {
            return;
        }
        let profile = self
            .field_value(Field::Profile, cx)
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "default".into());
        let args: Vec<String> = sign_in
            .command
            .iter()
            .map(|a| a.replace("{profile}", &profile))
            .collect();
        self.busy = true;
        self.set_status(
            format!(
                "Running `{}`. Finish signing in in your browser…",
                args.join(" ")
            ),
            Tone::Busy,
            cx,
        );
        let install = sign_in.install_url;
        let program = args[0].clone();
        let task = crate::runtime::spawn(async move {
            tokio::process::Command::new(&args[0])
                .args(&args[1..])
                .stdin(std::process::Stdio::null())
                .output()
                .await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |dialog, cx| {
                dialog.busy = false;
                match result {
                    Ok(Ok(output)) if output.status.success() => {
                        dialog.set_status("Signed in ✓  Testing the connection…", Tone::Good, cx);
                        dialog.test(cx);
                    }
                    Ok(Ok(output)) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        let last = stderr
                            .lines()
                            .rev()
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or("failed");
                        dialog.set_status(format!("Sign-in failed: {last}"), Tone::Bad, cx);
                    }
                    _ => {
                        dialog.set_status(format!("`{program}` isn't installed."), Tone::Bad, cx);
                        let name = match program.as_str() {
                            "aws" => "the AWS CLI".to_owned(),
                            "gcloud" => "the Google Cloud CLI".to_owned(),
                            other => format!("`{other}`"),
                        };
                        dialog.status_link = Some((format!("Install {name}"), install.to_owned()));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}

fn split_list(value: &str) -> Vec<String> {
    value
        .split([',', '\n'])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// A provider id from a display name: `GPU box` -> `gpu-box`.
fn slug(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "server".into()
    } else {
        slug
    }
}

fn button(
    id: &'static str,
    label: impl Into<SharedString>,
    hint: Option<&'static str>,
    primary: bool,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let mut el = div()
        .id(id)
        .flex()
        .flex_row()
        .gap(px(8.))
        .px(px(14.))
        .py(px(5.))
        .rounded(px(4.))
        .border_1()
        .child(label.into());
    if let Some(hint) = hint {
        el = el.child(div().text_color(theme::muted()).child(hint));
    }
    if !enabled {
        return el.border_color(theme::border()).text_color(theme::muted());
    }
    el = if primary {
        el.bg(theme::chip())
            .border_color(theme::chip())
            .text_color(theme::on_chip())
    } else {
        el.border_color(theme::selected_bg())
            .text_color(theme::text())
    };
    el.cursor_pointer().hover(|d| d.bg(theme::selected_bg()))
}

fn link(id: (&'static str, usize), label: &str, url: &str) -> AnyElement {
    let url = url.to_owned();
    div()
        .id(id)
        .flex()
        .flex_row()
        .gap(px(4.))
        .text_color(theme::md_link())
        .cursor_pointer()
        .hover(|d| d.text_color(theme::primary()))
        .child(div().underline().child(label.to_owned()))
        .child("↗")
        .on_click(move |_, _, cx| cx.open_url(&url))
        .into_any_element()
}

impl Render for ProvidersDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_focused = self.list_focus.is_focused(window);
        let focused_field = self.focused_field(window, cx);

        // Left: the providers, grouped.
        let mut list = div()
            .id("providers-list")
            .key_context("ProvidersList")
            .track_focus(&self.list_focus)
            .on_action(cx.listener(|this, _: &ListUp, window, cx| {
                let index = this.selected.saturating_sub(1);
                this.select(index, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ListDown, window, cx| {
                let index = (this.selected + 1).min(this.rows.len() - 1);
                this.select(index, window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &OpenForm, window, cx| this.focus_first_field(window, cx)),
            )
            .w(px(300.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .flex()
            .flex_col()
            .py(px(6.))
            .bg(theme::raised());
        let mut last_group = None;
        for (index, row) in self.rows.iter().enumerate() {
            if last_group != Some(row.group) {
                last_group = Some(row.group);
                list = list.child(
                    div()
                        .px(px(16.))
                        .pt(px(10.))
                        .pb(px(2.))
                        .text_color(theme::muted())
                        .font_weight(FontWeight::BOLD)
                        .text_size(theme::SMALL_SIZE)
                        .child(row.group.title()),
                );
            }
            let selected = index == self.selected;
            let (dot, dot_color) = if row.id == NEW {
                ("+", theme::primary())
            } else {
                status_dot(row.connected)
            };
            list = list.child(
                div()
                    .id(("provider-row", index))
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    .pr(px(12.))
                    .py(px(4.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme::selected_bg()))
                    .when(!selected, |d| d.hover(|d| d.bg(theme::raised_high())))
                    .child(
                        div()
                            .w(px(3.))
                            .h(px(38.))
                            .when(selected && list_focused, |d| d.bg(theme::primary())),
                    )
                    .child(div().pt(px(1.)).text_color(dot_color).child(dot))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(div().truncate().child(row.name.clone()))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(theme::SMALL_SIZE)
                                    .text_color(if row.connected {
                                        theme::success()
                                    } else {
                                        theme::muted()
                                    })
                                    .child(row.detail.clone()),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select(index, window, cx);
                        window.focus(&this.list_focus);
                    })),
            );
        }

        // Right: the selected provider's form.
        let mut detail = div()
            .id("provider-detail")
            .flex_1()
            .min_w_0()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(14.))
            .px(px(24.))
            .py(px(18.));
        // Buttons and status stay in view below the scrolling form.
        let mut actions = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .px(px(24.))
            .py(px(12.))
            .border_t_1()
            .border_color(theme::border());
        if let Some(form) = &self.form {
            let row = &self.rows[self.selected];
            let info = self.registry.find(&form.id);
            let title = if form.id == NEW {
                row.name.clone()
            } else {
                info.map_or(row.name.clone(), |p| p.name.clone())
            };
            let mut head = div().flex().flex_row().items_center().gap(px(10.)).child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(16.))
                    .child(title),
            );
            if form.id != NEW {
                let (dot, color) = status_dot(row.connected);
                head = head.child(
                    div()
                        .text_size(theme::SMALL_SIZE)
                        .text_color(color)
                        .child(format!("{dot} {}", row.detail)),
                );
            }
            detail = detail.child(head);
            detail = detail.child(div().text_color(theme::text_soft()).child(form.entry.blurb));

            if !form.entry.links.is_empty() {
                let mut links = div().flex().flex_row().flex_wrap().gap(px(18.));
                for (i, (label, url)) in form.entry.links.iter().enumerate() {
                    links = links.child(link(("provider-link", i), label, url));
                }
                detail = detail.child(links);
            }

            // Region presets fill in the base URL; an empty field means the first one.
            let base_input = form
                .inputs
                .iter()
                .find(|(spec, _)| spec.field == Field::BaseUrl)
                .map(|(_, input)| input.clone());
            if let (false, Some(base_input)) = (form.entry.regions.is_empty(), base_input) {
                let current = base_input.read(cx).text().trim().to_owned();
                let mut toggles = div().flex().flex_row().items_center().gap(px(8.)).child(
                    div()
                        .text_size(theme::SMALL_SIZE)
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::text_soft())
                        .child("Region"),
                );
                for (i, (label, url)) in form.entry.regions.iter().enumerate() {
                    let active = current == *url || (current.is_empty() && i == 0);
                    let input = base_input.clone();
                    let url = url.to_string();
                    toggles = toggles.child(
                        div()
                            .id(("region", i))
                            .px(px(10.))
                            .py(px(3.))
                            .rounded(px(4.))
                            .border_1()
                            .cursor_pointer()
                            .when(active, |d| {
                                d.bg(theme::chip())
                                    .border_color(theme::chip())
                                    .text_color(theme::on_chip())
                            })
                            .when(!active, |d| {
                                d.border_color(theme::selected_bg())
                                    .text_color(theme::text_soft())
                                    .hover(|d| d.bg(theme::raised_high()))
                            })
                            .child(*label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                input.update(cx, |input, cx| input.set_text(url.clone(), cx));
                                this.status = None;
                                cx.notify();
                            })),
                    );
                }
                detail = detail.child(toggles);
            }

            for (index, (spec, input)) in form.inputs.iter().enumerate() {
                let focused = focused_field == Some(index);
                let mut field_row = div().flex().flex_row().gap(px(8.)).items_center().child(
                    div()
                        .id(("field", index))
                        .flex_1()
                        .min_w_0()
                        .bg(theme::prompt_bg())
                        .border_1()
                        .rounded(px(4.))
                        .border_color(if focused {
                            theme::primary()
                        } else {
                            theme::border()
                        })
                        .px(px(10.))
                        .py(px(6.))
                        .cursor_text()
                        .child(input.clone()),
                );
                if spec.field == Field::ApiKey {
                    let masked = input.read(cx).is_masked();
                    let input = input.clone();
                    field_row = field_row.child(
                        div()
                            .id(("reveal", index))
                            .text_size(theme::SMALL_SIZE)
                            .text_color(theme::muted())
                            .cursor_pointer()
                            .hover(|d| d.text_color(theme::text()))
                            .child(if masked { "show" } else { "hide" })
                            .on_click(cx.listener(move |_, _, _, cx| {
                                input.update(cx, |input, cx| {
                                    let masked = input.is_masked();
                                    input.set_masked(!masked, cx)
                                });
                            })),
                    );
                }
                detail = detail.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(theme::SMALL_SIZE)
                                .font_weight(FontWeight::BOLD)
                                .text_color(if focused {
                                    theme::primary()
                                } else {
                                    theme::text_soft()
                                })
                                .child(spec.label),
                        )
                        .child(field_row)
                        .child(
                            div()
                                .text_size(theme::SMALL_SIZE)
                                .text_color(theme::muted())
                                .child(spec.help),
                        ),
                );
            }

            let busy = self.busy;
            let mut buttons = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(10.))
                .pt(px(4.))
                .child(
                    button(
                        "save",
                        if form.id == NEW { "Add server" } else { "Save" },
                        Some("enter"),
                        true,
                        !busy,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                )
                .child(
                    button("test", "Test connection", Some("ctrl-t"), false, !busy)
                        .on_click(cx.listener(|this, _, _, cx| this.test(cx))),
                );
            if let Some(sign_in) = &form.entry.sign_in {
                buttons = buttons.child(
                    button("sign-in", sign_in.label, None, false, !busy)
                        .on_click(cx.listener(|this, _, _, cx| this.sign_in(cx))),
                );
            }
            let removable = form.id != NEW
                && (config::has_saved_key(&form.id) || self.config.provider.contains_key(&form.id));
            if removable {
                buttons = buttons.child(
                    button("forget", "Disconnect", None, false, !busy)
                        .on_click(cx.listener(|this, _, window, cx| this.disconnect(window, cx))),
                );
            }
            actions = actions.child(buttons);

            if let Some((text, tone)) = &self.status {
                let color = match tone {
                    Tone::Info => theme::muted(),
                    Tone::Busy => theme::warning(),
                    Tone::Good => theme::success(),
                    Tone::Bad => theme::error(),
                };
                let mut status = div().flex().flex_col().gap(px(4.)).text_color(color).child(
                    if *tone == Tone::Busy {
                        format!("⋯ {text}")
                    } else {
                        text.clone()
                    },
                );
                if let Some((label, url)) = &self.status_link {
                    status = status.child(link(("status-link", 0), label, url));
                }
                actions = actions.child(status);
            }
        }

        div()
            .key_context("Providers")
            .on_action(cx.listener(|this, _: &Close, window, cx| {
                // Esc in a field goes back to the list; in the list it closes.
                if this.list_focus.is_focused(window) {
                    cx.emit(ProvidersEvent::Dismiss);
                } else {
                    window.focus(&this.list_focus);
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &NextField, window, cx| {
                match this.focused_field(window, cx) {
                    Some(i) => {
                        let input = this.form.as_ref().unwrap().inputs[i].1.clone();
                        this.move_field(&input, 1, window, cx);
                    }
                    None => this.focus_first_field(window, cx),
                }
            }))
            .on_action(cx.listener(|this, _: &PrevField, window, cx| {
                if let Some(i) = this.focused_field(window, cx) {
                    let input = this.form.as_ref().unwrap().inputs[i].1.clone();
                    this.move_field(&input, -1, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Save, window, cx| this.save(window, cx)))
            .on_action(cx.listener(|this, _: &Test, _, cx| this.test(cx)))
            .w(px(960.))
            .max_w_full()
            .h(px(600.))
            .bg(theme::bg())
            .rounded(px(6.))
            .border_1()
            .border_color(theme::raised_max())
            .shadow_lg()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .px(px(20.))
                    .py(px(12.))
                    .bg(theme::raised())
                    .border_b_1()
                    .border_color(theme::border())
                    .child(div().font_weight(FontWeight::BOLD).child("Providers"))
                    .child(
                        div()
                            .id("providers-close")
                            .text_color(theme::muted())
                            .cursor_pointer()
                            .hover(|d| d.text_color(theme::text()))
                            .child("esc ✕")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|_, _, _, cx| cx.emit(ProvidersEvent::Dismiss)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(list)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(detail)
                            .when(self.form.is_some(), |d| d.child(actions)),
                    ),
            )
            .child(
                div()
                    .px(px(20.))
                    .py(px(8.))
                    .bg(theme::raised_high())
                    .flex()
                    .flex_row()
                    .gap(px(18.))
                    .text_size(theme::SMALL_SIZE)
                    .text_color(theme::text_soft())
                    .child("↑↓ choose")
                    .child("enter / tab edit")
                    .child("enter save")
                    .child("ctrl-t test")
                    .child("esc back"),
            )
    }
}
