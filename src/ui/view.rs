//! The window's state: the session, the prompt, the running turn and dialogs.

use super::input::{InputEvent, TextInput};
use super::markdown::{self, Block};
use super::picker::{Picker, PickerEvent, PickerItem, PickerKind};
use super::providers::{ProvidersDialog, ProvidersEvent};
use super::*;
use crate::agent::{self, AgentEvent};
use crate::context;
use crate::providers::{Effort, Role};
use crate::session::{Entry, Session};
use crate::tools::{Agent, Todo};
use crate::workspace::{Selection, Workspace};
use gpui::{
    AppContext as _, ClipboardItem, Context, Entity, Focusable, ScrollHandle, Subscription, Window,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

pub(super) struct Running {
    pub cancel: watch::Sender<bool>,
    pub started: Instant,
    /// When the model last sent anything; a long gap shows a "working" row.
    pub last_event: Instant,
}

#[derive(Clone)]
pub(super) struct SlashItem {
    pub name: String,
    pub description: String,
    pub skill: bool,
}

pub(super) const COMMANDS: &[(&str, &str, &[&str])] = &[
    ("new", "Start a new session", &["clear"]),
    (
        "connect",
        "Connect a provider: API keys, cloud sign-in, local servers",
        &["login", "auth", "providers"],
    ),
    (
        "sessions",
        "Switch to another session",
        &["resume", "continue"],
    ),
    ("models", "Switch model", &[]),
    (
        "effort",
        "Change reasoning effort",
        &["thinking", "variants"],
    ),
    ("agents", "Switch agent (build / plan)", &[]),
    ("skills", "Browse and run skills", &[]),
    ("init", "Create or update AGENTS.md for this project", &[]),
    ("reload", "Reload AGENTS.md, skills and config", &[]),
    ("copy", "Copy the last response", &[]),
    ("rename", "Rename the session: /rename <title>", &[]),
    ("cd", "Change the working directory: /cd <dir>", &[]),
    ("sidebar", "Toggle the sidebar", &[]),
    ("mcp", "MCP servers: status and reconnect", &["mcps"]),
    ("tab", "Open a new tab", &["newtab"]),
    ("close", "Close this tab", &["closetab"]),
    ("help", "Keybindings", &[]),
    ("exit", "Quit", &["quit", "q"]),
];

const INIT_PROMPT: &str = "Analyze this codebase and create an AGENTS.md file in the project root \
(or improve the existing one) for future coding agents working here. Include: the build, lint and \
test commands (including how to run a single test), and code style guidelines (imports, formatting, \
types, naming, error handling). Keep it around 20-30 lines. If there are Cursor rules \
(.cursor/rules/ or .cursorrules) or Copilot rules (.github/copilot-instructions.md), include them.";

const PLACEHOLDERS: [&str; 3] = [
    "Fix a TODO in the codebase",
    "What is the tech stack of this project?",
    "Fix broken tests",
];

pub struct CrackView {
    pub(super) ws: Workspace,
    pub(super) selection: Selection,
    pub(super) session: Session,
    pub(super) input: Entity<TextInput>,
    pub(super) scroll: ScrollHandle,
    pub(super) follow: bool,
    pub(super) running: Option<Running>,
    pub(super) esc_armed: Option<Instant>,
    pub(super) todos: Arc<Mutex<Vec<Todo>>>,
    pub(super) picker: Option<(Entity<Picker>, Subscription)>,
    pub(super) providers: Option<(Entity<ProvidersDialog>, Subscription)>,
    pub(super) sidebar: Option<bool>,
    history: Vec<String>,
    history_pos: Option<usize>,
    pub(super) autocomplete: Vec<SlashItem>,
    pub(super) ac_selected: usize,
    pub(super) expanded: HashSet<String>,
    md_cache: RefCell<HashMap<u64, Rc<Vec<Block>>>>,
    pub(super) toast: Option<(String, Instant, bool)>,
    recent_models: Vec<String>,
    pub(super) discovering: bool,
    /// The window title last set; updated when the session or directory changes.
    pub(super) window_title: String,
    // ---- Tabs: the active tab lives in the fields above; the others wait here. ----
    pub(super) tab_id: u64,
    pub(super) tab_order: Vec<u64>,
    pub(super) background: HashMap<u64, TabState>,
    next_tab_id: u64,
    /// Saves the window size shortly after resizing stops (dropping it cancels).
    resize_save: Option<gpui::Task<()>>,
    _bounds_subscription: Subscription,
    _subscription: Subscription,
}

impl CrackView {
    pub fn new(
        ws: Workspace,
        selection: Selection,
        prompt: Option<String>,
        session_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let pick = PLACEHOLDERS[(std::process::id() as usize) % PLACEHOLDERS.len()];
        let input = cx.new(|cx| TextInput::new(true, format!("Ask anything… \"{pick}\""), cx));
        let subscription = cx.subscribe_in(&input, window, Self::on_input_event);

        let mut session = Session::new(&ws.cwd);
        let mut toast = None;
        if let Some(id) = session_id {
            let id = if id == "last" {
                Session::list(&context::project_root(&ws.cwd))
                    .first()
                    .map(|s| s.id.clone())
                    .unwrap_or_default()
            } else {
                id
            };
            match Session::load(&id) {
                Ok(loaded) => session = loaded,
                Err(error) => {
                    toast = Some((
                        format!("Could not open session {id}: {error}"),
                        Instant::now(),
                        true,
                    ))
                }
            }
        }
        let todos = Arc::new(Mutex::new(session.todos.clone()));
        let history = load_history();
        let recent_models = selection.model.iter().cloned().collect();

        let bounds_subscription = cx.observe_window_bounds(window, |this, window, cx| {
            this.on_window_resized(window, cx)
        });
        let mut view = Self {
            ws,
            selection,
            session,
            input,
            scroll: ScrollHandle::new(),
            follow: true,
            running: None,
            esc_armed: None,
            todos,
            picker: None,
            providers: None,
            sidebar: None,
            history,
            history_pos: None,
            autocomplete: Vec::new(),
            ac_selected: 0,
            expanded: HashSet::new(),
            md_cache: RefCell::new(HashMap::new()),
            toast,
            recent_models,
            discovering: false,
            window_title: String::new(),
            tab_id: 1,
            tab_order: vec![1],
            background: HashMap::new(),
            next_tab_id: 2,
            resize_save: None,
            _bounds_subscription: bounds_subscription,
            _subscription: subscription,
        };
        view.discover_models(cx);
        view.connect_mcp(None, cx);
        if let Some(prompt) = prompt {
            view.submit_text(prompt, window, cx);
        }
        view
    }

    pub fn focus_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.input.focus_handle(cx));
    }

    /// Asks OpenAI-compatible and local servers for their models in the background.
    fn discover_models(&mut self, cx: &mut Context<Self>) {
        self.discovering = true;
        let mut registry = self.ws.registry.clone();
        let client = self.ws.http.clone();
        let task = crate::runtime::spawn(async move {
            registry.discover(&client).await;
            registry
        });
        cx.spawn(async move |this, cx| {
            let Ok(registry) = task.await else { return };
            let _ = this.update(cx, |view, cx| {
                view.ws.registry = registry;
                view.discovering = false;
                if view.selection.model.is_none() {
                    view.selection.model = view.ws.registry.default_model();
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn show_toast(
        &mut self,
        text: impl Into<String>,
        error: bool,
        cx: &mut Context<Self>,
    ) {
        self.toast = Some((text.into(), Instant::now(), error));
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(4)).await;
            let _ = this.update(cx, |view, cx| {
                if view
                    .toast
                    .as_ref()
                    .is_some_and(|t| t.1.elapsed() >= Duration::from_secs(4))
                {
                    view.toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn markdown(&self, text: &str) -> Rc<Vec<Block>> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let key = hasher.finish();
        let mut cache = self.md_cache.borrow_mut();
        if cache.len() > 400 {
            cache.clear();
        }
        cache
            .entry(key)
            .or_insert_with(|| Rc::new(markdown::parse(text)))
            .clone()
    }

    // ---- Prompt ----

    fn on_input_event(
        &mut self,
        _input: &Entity<TextInput>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Changed => self.update_autocomplete(cx),
            InputEvent::Submit => {
                if !self.autocomplete.is_empty() {
                    self.accept_autocomplete(true, window, cx);
                } else {
                    let text = self.input.read(cx).text().to_owned();
                    self.input.update(cx, |input, cx| input.set_text("", cx));
                    self.submit_text(text, window, cx);
                }
            }
            InputEvent::Up => {
                if !self.autocomplete.is_empty() {
                    let len = self.autocomplete.len();
                    self.ac_selected = (self.ac_selected + len - 1) % len;
                    cx.notify();
                } else {
                    self.history_step(-1, cx);
                }
            }
            InputEvent::Down => {
                if !self.autocomplete.is_empty() {
                    self.ac_selected = (self.ac_selected + 1) % self.autocomplete.len();
                    cx.notify();
                } else {
                    self.history_step(1, cx);
                }
            }
        }
    }

    fn history_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.history.is_empty() {
            return;
        }
        let len = self.history.len() as isize;
        let next = match self.history_pos {
            None if delta < 0 => len - 1,
            None => return,
            Some(pos) => pos as isize + delta,
        };
        if next >= len {
            self.history_pos = None;
            self.input.update(cx, |input, cx| input.set_text("", cx));
            return;
        }
        let next = next.clamp(0, len - 1) as usize;
        self.history_pos = Some(next);
        let text = self.history[next].clone();
        self.input.update(cx, |input, cx| input.set_text(text, cx));
    }

    fn update_autocomplete(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).text();
        self.autocomplete.clear();
        if let Some(query) = text.strip_prefix('/')
            && !query.contains(char::is_whitespace)
        {
            let query = query.to_lowercase();
            for (name, description, aliases) in COMMANDS {
                if name.starts_with(&query) || aliases.iter().any(|a| a.starts_with(&query)) {
                    self.autocomplete.push(SlashItem {
                        name: name.to_string(),
                        description: description.to_string(),
                        skill: false,
                    });
                }
            }
            for skill in self.ws.skills.iter() {
                if skill.name.to_lowercase().contains(&query) {
                    self.autocomplete.push(SlashItem {
                        name: skill.name.clone(),
                        description: skill.description.clone(),
                        skill: true,
                    });
                }
            }
            // An exact command with nothing more to complete needs no popup.
            if self.autocomplete.len() == 1
                && self.autocomplete[0].name == query
                && !self.autocomplete[0].skill
            {
                self.autocomplete.clear();
            }
        }
        self.ac_selected = self
            .ac_selected
            .min(self.autocomplete.len().saturating_sub(1));
        cx.notify();
    }

    fn accept_autocomplete(&mut self, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.autocomplete.get(self.ac_selected).cloned() else {
            return;
        };
        self.autocomplete.clear();
        let takes_args = item.skill || matches!(item.name.as_str(), "rename" | "cd");
        if run && !takes_args {
            self.input.update(cx, |input, cx| input.set_text("", cx));
            self.submit_text(format!("/{}", item.name), window, cx);
        } else {
            self.input.update(cx, |input, cx| {
                input.set_text(format!("/{} ", item.name), cx)
            });
            self.autocomplete.clear();
        }
        cx.notify();
    }

    pub(super) fn submit_text(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.history_pos = None;
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
            save_history(&self.history);
        }
        if let Some(rest) = text.strip_prefix('/') {
            let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if self.run_command(name, args.trim(), window, cx) {
                return;
            }
        }
        if self.running.is_some() {
            self.show_toast(
                "A turn is running. Press esc twice to interrupt it.",
                true,
                cx,
            );
            self.input.update(cx, |input, cx| input.set_text(text, cx));
            return;
        }
        self.start_turn(text, cx);
    }

    /// Runs a slash command. Returns false when `name` isn't one (it may be a skill).
    fn run_command(
        &mut self,
        name: &str,
        args: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let resolved = COMMANDS
            .iter()
            .find(|(n, _, aliases)| *n == name || aliases.contains(&name))
            .map(|(n, _, _)| *n);
        match resolved {
            Some("new") => self.new_session(cx),
            Some("sessions") => self.open_picker(PickerKind::Session, window, cx),
            Some("models") => self.open_picker(PickerKind::Model, window, cx),
            Some("connect") => self.open_providers(None, window, cx),
            Some("effort") => {
                if let Some(effort) = Effort::parse(args) {
                    self.set_effort(effort, cx);
                } else {
                    self.open_picker(PickerKind::Effort, window, cx);
                }
            }
            Some("agents") => self.open_picker(PickerKind::Agent, window, cx),
            Some("skills") => self.open_picker(PickerKind::Skill, window, cx),
            Some("help") => self.open_picker(PickerKind::Help, window, cx),
            Some("init") => {
                if self.running.is_none() {
                    self.start_turn(INIT_PROMPT.to_owned(), cx);
                }
            }
            Some("reload") => {
                match crate::config::Config::load(&self.ws.cwd) {
                    Ok(config) => {
                        self.ws.config = config;
                        self.ws.registry = crate::providers::Registry::load(&self.ws.config);
                        self.discover_models(cx);
                    }
                    Err(error) => self.show_toast(format!("Config: {error}"), true, cx),
                }
                self.ws.reload_context();
                self.connect_mcp(None, cx);
                let text = format!(
                    "Reloaded: {} instruction file(s), {} skill(s).",
                    self.ws.instructions.len(),
                    self.ws.skills.len()
                );
                self.show_toast(text, false, cx);
            }
            Some("copy") => self.copy_last(cx),
            Some("rename") => {
                if !args.is_empty() {
                    self.session.title = args.to_owned();
                    let _ = self.session.save();
                    cx.notify();
                }
            }
            Some("cd") => self.change_dir(args, cx),
            Some("sidebar") => self.toggle_sidebar(window, cx),
            Some("mcp") => self.open_picker(PickerKind::Mcp, window, cx),
            Some("tab") => self.new_tab(window, cx),
            Some("close") => {
                let id = self.tab_id;
                self.close_tab(id, window, cx)
            }
            Some("exit") => cx.quit(),
            _ => {
                if self.ws.skill(name).is_some() {
                    if self.running.is_some() {
                        self.show_toast("A turn is running.", true, cx);
                    } else {
                        let text = if args.is_empty() {
                            format!("/{name}")
                        } else {
                            format!("/{name} {args}")
                        };
                        self.start_turn(text, cx);
                    }
                    return true;
                }
                return false;
            }
        }
        true
    }

    fn change_dir(&mut self, dir: &str, cx: &mut Context<Self>) {
        if self.running.is_some() {
            self.show_toast("Wait for the turn to finish first.", true, cx);
            return;
        }
        let target = crate::config::expand_home(if dir.is_empty() { "~/" } else { dir });
        let target = if target.is_absolute() {
            target
        } else {
            self.ws.cwd.join(target)
        };
        match Workspace::load(&target) {
            Ok(ws) => {
                self.ws = ws;
                self.new_session(cx);
                self.discover_models(cx);
                self.connect_mcp(None, cx);
                let shown = self.ws.short_cwd();
                self.show_toast(format!("Now in {shown}"), false, cx);
            }
            Err(error) => self.show_toast(format!("{}: {error}", target.display()), true, cx),
        }
    }

    pub(super) fn new_session(&mut self, cx: &mut Context<Self>) {
        if self.running.is_some() {
            self.show_toast("Wait for the turn to finish first.", true, cx);
            return;
        }
        self.session = Session::new(&self.ws.cwd);
        *self.todos.lock().unwrap() = Vec::new();
        self.expanded.clear();
        self.follow = true;
        cx.notify();
    }

    // ---- Turns ----

    fn start_turn(&mut self, text: String, cx: &mut Context<Self>) {
        // The model may have been picked before discovery finished.
        let config = match self.ws.turn_config(&self.selection) {
            Ok(config) => config,
            Err(error) => {
                self.session.entries.push(Entry::User {
                    text: text.clone(),
                    agent: self.selection.agent,
                    skill: None,
                });
                self.session.entries.push(Entry::Error {
                    text: format!("{error:#}"),
                });
                cx.notify();
                return;
            }
        };
        let (model_text, skill) = match self.ws.expand_skill(&text) {
            Some((expanded, name)) => (expanded, Some(name)),
            None => (text.clone(), None),
        };
        self.session
            .push_user(&text, model_text, self.selection.agent, skill);
        self.follow = true;

        let (events_tx, mut events) = mpsc::unbounded_channel();
        let (cancel_tx, cancel) = watch::channel(false);
        let tools = self.ws.tool_context(self.todos.clone(), None);
        let history = self.session.messages.clone();
        crate::runtime::spawn(agent::run_turn(config, history, tools, events_tx, cancel));
        self.running = Some(Running {
            cancel: cancel_tx,
            started: Instant::now(),
            last_event: Instant::now(),
        });

        // Events belong to the tab that started the turn, even after switching away.
        let tab = self.tab_id;
        let agent = self.selection.agent;
        let model = self.selection.model.clone().unwrap_or_default();
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                // Coalesce bursts of streamed text into one redraw.
                let mut batch = vec![event];
                while let Ok(more) = events.try_recv() {
                    batch.push(more);
                }
                let alive = this.update(cx, |view, cx| {
                    for event in batch {
                        view.on_agent_event(tab, event, agent, &model, cx);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn on_agent_event(
        &mut self,
        tab: u64,
        event: AgentEvent,
        agent: Agent,
        model: &str,
        cx: &mut Context<Self>,
    ) {
        if tab != self.tab_id {
            self.on_background_event(tab, event, agent, model, cx);
            return;
        }
        self.session.apply(&event, agent, model);
        if let Some(running) = self.running.as_mut() {
            running.last_event = Instant::now();
        }
        if let AgentEvent::Done { .. } = event {
            self.running = None;
            self.esc_armed = None;
            self.session.todos = self.todos.lock().unwrap().clone();
            if let Err(error) = self.session.save() {
                self.show_toast(format!("Could not save the session: {error}"), true, cx);
            }
        }
    }

    pub(super) fn interrupt(&mut self, cx: &mut Context<Self>) {
        if !self.autocomplete.is_empty() {
            self.autocomplete.clear();
            cx.notify();
            return;
        }
        let Some(running) = &self.running else {
            return;
        };
        // The first press arms, the second (within 2s) interrupts.
        if self
            .esc_armed
            .is_some_and(|at| at.elapsed() < Duration::from_secs(2))
        {
            let _ = running.cancel.send(true);
            self.esc_armed = None;
        } else {
            self.esc_armed = Some(Instant::now());
        }
        cx.notify();
    }

    pub(super) fn copy_last(&mut self, cx: &mut Context<Self>) {
        let last = self
            .session
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant && !m.text().is_empty())
            .map(|m| m.text());
        match last {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.show_toast("Copied the last response.", false, cx);
            }
            None => self.show_toast("Nothing to copy yet.", true, cx),
        }
    }

    pub(super) fn copy_text(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.show_toast("Copied.", false, cx);
    }

    // ---- Selection ----

    pub(super) fn cycle_agent(&mut self, cx: &mut Context<Self>) {
        self.selection.agent = self.selection.agent.next();
        cx.notify();
    }

    pub(super) fn cycle_effort(&mut self, cx: &mut Context<Self>) {
        let next = self.selection.effort.next();
        self.set_effort(next, cx);
    }

    pub(super) fn cycle_recent_model(&mut self, cx: &mut Context<Self>) {
        if self.recent_models.len() < 2 {
            self.show_toast("No other recent model. Pick one with ctrl-x m.", false, cx);
            return;
        }
        let current = self.selection.model.clone();
        let index = self
            .recent_models
            .iter()
            .position(|m| Some(m) == current.as_ref())
            .map_or(0, |i| (i + 1) % self.recent_models.len());
        self.selection.model = Some(self.recent_models[index].clone());
        cx.notify();
    }

    fn set_model(&mut self, model: String, cx: &mut Context<Self>) {
        self.recent_models.retain(|m| m != &model);
        self.recent_models.insert(0, model.clone());
        self.recent_models.truncate(8);
        self.selection.model = Some(model);
        cx.notify();
    }

    pub(super) fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = self.sidebar_visible(window);
        self.sidebar = Some(!shown);
        cx.notify();
    }

    pub(super) fn sidebar_visible(&self, window: &Window) -> bool {
        !self.session.is_empty()
            && self
                .sidebar
                .unwrap_or_else(|| window.viewport_size().width > px(1100.))
    }

    pub(super) fn scroll_by(&mut self, pages: f32, cx: &mut Context<Self>) {
        let height = self.scroll.bounds().size.height;
        let mut offset = self.scroll.offset();
        let max = self.scroll.max_offset().height;
        offset.y = (offset.y - height * pages * 0.9).clamp(-max, px(0.));
        self.scroll.set_offset(offset);
        self.follow = -offset.y >= max - px(4.);
        cx.notify();
    }

    // ---- Dialogs ----

    pub(super) fn open_picker(
        &mut self,
        kind: PickerKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (title, items, wide) = match kind {
            PickerKind::Model => ("Select model", self.model_items(), true),
            PickerKind::Effort => ("Reasoning effort", self.effort_items(), false),
            PickerKind::Agent => ("Select agent", self.agent_items(), false),
            PickerKind::Session => ("Sessions", self.session_items(), true),
            PickerKind::Skill => ("Skills", self.skill_items(), true),
            PickerKind::Command => ("Commands", self.command_items(), false),
            PickerKind::Help => ("Keybindings", help_items(), false),
            PickerKind::Mcp => ("MCP servers", self.mcp_items(), true),
        };
        let picker = cx.new(|cx| Picker::new(kind, title, items, wide, window, cx));
        let subscription = cx.subscribe_in(&picker, window, Self::on_picker_event);
        self.picker = Some((picker, subscription));
        cx.notify();
    }

    pub(super) fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker = None;
        self.focus_prompt(window, cx);
        cx.notify();
    }

    fn on_picker_event(
        &mut self,
        _picker: &Entity<Picker>,
        event: &PickerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (kind, id) = match event {
            PickerEvent::Dismiss => {
                self.close_picker(window, cx);
                return;
            }
            PickerEvent::Confirm(kind, id) => (*kind, id.clone()),
        };
        self.close_picker(window, cx);
        match kind {
            PickerKind::Model => match id.strip_prefix("connect:") {
                Some("") => self.open_providers(None, window, cx),
                Some(provider) => self.open_providers(Some(provider.to_owned()), window, cx),
                None => self.set_model(id, cx),
            },
            PickerKind::Effort => {
                if let Some(effort) = Effort::parse(&id) {
                    self.set_effort(effort, cx);
                }
            }
            PickerKind::Agent => {
                if let Some(agent) = Agent::parse(&id) {
                    self.selection.agent = agent;
                }
            }
            PickerKind::Session => {
                if let Some(tab) = self.tab_with_session(&id) {
                    // Already open: go to it.
                    self.switch_tab(tab, window, cx);
                } else if self.running.is_some() {
                    self.show_toast("Wait for the turn to finish first.", true, cx);
                } else {
                    match Session::load(&id) {
                        Ok(session) => {
                            *self.todos.lock().unwrap() = session.todos.clone();
                            self.session = session;
                            self.follow = true;
                            self.expanded.clear();
                        }
                        Err(error) => self.show_toast(format!("{error}"), true, cx),
                    }
                }
            }
            PickerKind::Skill => {
                self.input
                    .update(cx, |input, cx| input.set_text(format!("/{id} "), cx));
                self.autocomplete.clear();
            }
            PickerKind::Command => {
                self.submit_text(format!("/{id}"), window, cx);
            }
            PickerKind::Help => {}
            PickerKind::Mcp => {
                if !id.is_empty() {
                    self.show_toast(format!("Reconnecting {id}…"), false, cx);
                    self.connect_mcp(Some(id), cx);
                }
            }
        }
        cx.notify();
    }

    fn model_items(&self) -> Vec<PickerItem> {
        let current = self.selection.model.clone().unwrap_or_default();
        let mut items = Vec::new();
        for recent in self.recent_models.iter().take(5) {
            items.push(PickerItem {
                id: recent.clone(),
                title: recent
                    .split_once('/')
                    .map_or(recent.as_str(), |(_, m)| m)
                    .to_owned(),
                description: recent.split_once('/').map(|(p, _)| p.to_owned()),
                category: Some("Recent".into()),
                current: *recent == current,
                ..Default::default()
            });
        }
        for provider in self.ws.registry.available() {
            if provider.models.is_empty() {
                items.push(PickerItem {
                    id: format!("connect:{}", provider.id),
                    title: "No models found: add them in Providers…".into(),
                    category: Some(provider.name.clone()),
                    ..Default::default()
                });
            }
            for model in &provider.models {
                let id = format!("{}/{}", provider.id, model.id);
                items.push(PickerItem {
                    current: id == current,
                    id,
                    title: model.id.clone(),
                    category: Some(provider.name.clone()),
                    ..Default::default()
                });
            }
        }
        for provider in self
            .ws
            .registry
            .providers
            .iter()
            .filter(|p| !p.available && !p.local)
        {
            items.push(PickerItem {
                id: format!("connect:{}", provider.id),
                title: format!("Connect {}…", provider.name),
                description: provider.note.clone(),
                category: Some("Connect a provider".into()),
                ..Default::default()
            });
        }
        items.push(PickerItem {
            id: "connect:".into(),
            title: "Manage providers…".into(),
            description: Some("API keys, cloud sign-in, local servers".into()),
            category: Some("Connect a provider".into()),
            footer: Some("ctrl-x p".into()),
            ..Default::default()
        });
        items
    }

    pub(super) fn open_providers(
        &mut self,
        initial: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker = None;
        let ws = &self.ws;
        let dialog = cx.new(|cx| {
            ProvidersDialog::new(
                ws.cwd.clone(),
                ws.config.clone(),
                ws.registry.clone(),
                ws.http.clone(),
                initial,
                window,
                cx,
            )
        });
        let subscription = cx.subscribe_in(&dialog, window, Self::on_providers_event);
        self.providers = Some((dialog, subscription));
        cx.notify();
    }

    pub(super) fn close_providers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.providers = None;
        self.focus_prompt(window, cx);
        cx.notify();
    }

    fn on_providers_event(
        &mut self,
        _dialog: &Entity<ProvidersDialog>,
        event: &ProvidersEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ProvidersEvent::Dismiss => self.close_providers(window, cx),
            ProvidersEvent::Changed {
                config,
                registry,
                connected,
            } => {
                self.ws.config = (**config).clone();
                self.ws.registry = (**registry).clone();
                // Switch to the new provider when the current model can't be used.
                let current_ok =
                    self.selection.model.as_deref().is_some_and(|m| {
                        self.ws.registry.resolve(m).is_ok_and(|(p, _)| p.available)
                    });
                if !current_ok {
                    let pick = connected
                        .as_deref()
                        .and_then(|id| self.ws.registry.find(id))
                        .filter(|p| p.available)
                        .and_then(|p| p.models.first().map(|m| format!("{}/{}", p.id, m.id)))
                        .or_else(|| self.ws.registry.default_model());
                    if let Some(model) = pick {
                        self.set_model(model, cx);
                    }
                }
                cx.notify();
            }
        }
    }

    fn effort_items(&self) -> Vec<PickerItem> {
        let descriptions = [
            "Medium if the model has it, otherwise the provider's default",
            "Least thinking (OpenAI-style models)",
            "Fast, light reasoning",
            "Balanced",
            "Thorough",
            "Very thorough (Claude)",
            "Maximum (Claude)",
        ];
        Effort::ALL
            .iter()
            .zip(descriptions)
            .map(|(effort, description)| PickerItem {
                id: effort.as_str().into(),
                title: effort.as_str().into(),
                description: Some(description.into()),
                current: *effort == self.selection.effort,
                ..Default::default()
            })
            .collect()
    }

    fn agent_items(&self) -> Vec<PickerItem> {
        [
            (Agent::Build, "Full tools: edit files, run commands"),
            (Agent::Plan, "Read-only: investigate and plan"),
        ]
        .into_iter()
        .map(|(agent, description)| PickerItem {
            id: agent.name().to_lowercase(),
            title: agent.name().into(),
            description: Some(description.into()),
            current: agent == self.selection.agent,
            ..Default::default()
        })
        .collect()
    }

    fn session_items(&self) -> Vec<PickerItem> {
        let today = chrono::Local::now().date_naive();
        Session::list(&context::project_root(&self.ws.cwd))
            .into_iter()
            .map(|s| {
                let when = chrono::DateTime::from_timestamp(s.updated, 0)
                    .map(|d| d.with_timezone(&chrono::Local))
                    .unwrap_or_else(chrono::Local::now);
                let category = if when.date_naive() == today {
                    "Today".to_owned()
                } else {
                    when.format("%a %b %d %Y").to_string()
                };
                PickerItem {
                    current: s.id == self.session.id,
                    id: s.id,
                    title: s.title,
                    footer: Some(when.format("%H:%M").to_string()),
                    category: Some(category),
                    ..Default::default()
                }
            })
            .collect()
    }

    fn skill_items(&self) -> Vec<PickerItem> {
        self.ws
            .skills
            .iter()
            .map(|skill| PickerItem {
                id: skill.name.clone(),
                title: skill.name.clone(),
                description: Some(skill.description.clone()),
                footer: skill.user_only.then(|| "manual".into()),
                ..Default::default()
            })
            .collect()
    }

    fn command_items(&self) -> Vec<PickerItem> {
        let keys: HashMap<&str, &str> = [
            ("new", "ctrl-x n"),
            ("sessions", "ctrl-x l"),
            ("models", "ctrl-x m"),
            ("connect", "ctrl-x p"),
            ("tab", "ctrl-x t"),
            ("close", "ctrl-x w"),
            ("effort", "ctrl-t"),
            ("agents", "tab"),
            ("skills", "ctrl-x k"),
            ("copy", "ctrl-x y"),
            ("sidebar", "ctrl-x b"),
            ("help", "ctrl-x h"),
            ("exit", "ctrl-x q"),
        ]
        .into_iter()
        .collect();
        let group = |name: &str| match name {
            "new" | "sessions" | "rename" | "copy" | "tab" | "close" => "Session",
            "models" | "effort" | "agents" | "connect" => "Agent",
            "skills" | "init" | "reload" | "cd" => "Project",
            _ => "System",
        };
        let mut items: Vec<PickerItem> = COMMANDS
            .iter()
            .filter(|(name, _, _)| !matches!(*name, "rename" | "cd"))
            .map(|(name, description, _)| PickerItem {
                id: name.to_string(),
                title: description.to_string(),
                description: Some(format!("/{name}")),
                category: Some(group(name).into()),
                footer: keys.get(name).map(|k| k.to_string()),
                ..Default::default()
            })
            .collect();
        items.sort_by_key(|i| {
            ["Session", "Agent", "Project", "System"]
                .iter()
                .position(|g| Some(*g) == i.category.as_deref())
        });
        items
    }
}

fn help_items() -> Vec<PickerItem> {
    KEYMAP
        .iter()
        .map(|(key, description)| PickerItem {
            id: String::new(),
            title: description.to_string(),
            footer: Some(key.to_string()),
            ..Default::default()
        })
        .collect()
}

fn history_path() -> std::path::PathBuf {
    crate::config::data_dir().join("prompt-history.json")
}

fn load_history() -> Vec<String> {
    std::fs::read(history_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_history(history: &[String]) {
    let start = history.len().saturating_sub(200);
    if let Some(parent) = history_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        history_path(),
        serde_json::to_vec(&history[start..]).unwrap_or_default(),
    );
}

impl CrackView {
    /// Keeps the window title in step with the session and directory. GPUI's Wayland
    /// backend ignores the title given when the window opens, so it is set here.
    pub(super) fn sync_window_title(&mut self, window: &mut Window) {
        let title = if self.session.is_empty() {
            format!("crack — {}", self.ws.short_cwd())
        } else {
            format!("{} — crack — {}", self.session.title, self.ws.short_cwd())
        };
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
    }
}

impl CrackView {
    /// Remembers the window's size for the next launch, once resizing settles.
    fn on_window_resized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (bounds, maximized) = match window.window_bounds() {
            gpui::WindowBounds::Windowed(b) => (b, false),
            gpui::WindowBounds::Maximized(b) | gpui::WindowBounds::Fullscreen(b) => (b, true),
        };
        let width = f32::from(bounds.size.width);
        let height = f32::from(bounds.size.height);
        self.resize_save = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            crate::state::State::update(|state| {
                state.window_maximized = maximized;
                // Keep the windowed size when maximized, to restore to it later.
                if !maximized {
                    state.window_width = Some(width);
                    state.window_height = Some(height);
                }
            });
        }));
    }

    /// Sets the effort and remembers it for the next launch.
    pub(super) fn set_effort(&mut self, effort: Effort, cx: &mut Context<Self>) {
        self.selection.effort = effort;
        crate::state::State::update(|state| state.effort = Some(effort));
        cx.notify();
    }
}

impl CrackView {
    /// Connects the MCP servers (or one of them) in the background.
    pub(super) fn connect_mcp(&mut self, server: Option<String>, cx: &mut Context<Self>) {
        let hub = self.ws.mcp.clone();
        if hub.is_empty() {
            return;
        }
        let task = crate::runtime::spawn(async move {
            match server {
                Some(name) => hub.connect(&name).await,
                None => hub.connect_all().await,
            }
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            let _ = task.await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    fn mcp_items(&self) -> Vec<PickerItem> {
        let mut items: Vec<PickerItem> = self
            .ws
            .mcp
            .snapshot()
            .into_iter()
            .map(|server| {
                let description = match &server.status {
                    crate::mcp::Status::Connected => {
                        format!("● connected · {} tools", server.tools)
                    }
                    crate::mcp::Status::Connecting => "⋯ connecting".into(),
                    crate::mcp::Status::Failed(why) => format!("✗ {why}"),
                    crate::mcp::Status::Disabled => "disabled".into(),
                };
                PickerItem {
                    id: server.name.clone(),
                    title: server.name,
                    description: Some(description),
                    footer: Some(format!("{} · enter reconnects", server.transport)),
                    ..Default::default()
                }
            })
            .collect();
        if items.is_empty() {
            items.push(PickerItem {
                title: "No MCP servers configured".into(),
                description: Some(
                    "Add [mcp.<name>] to ~/.config/crack/config.toml or a .mcp.json to the project"
                        .into(),
                ),
                disabled: true,
                ..Default::default()
            });
        }
        items
    }
}

/// A tab that isn't showing: its session, any running turn, and view state.
pub(super) struct TabState {
    pub session: Session,
    pub running: Option<Running>,
    pub todos: Arc<Mutex<Vec<Todo>>>,
    pub expanded: HashSet<String>,
    pub follow: bool,
    pub scroll: ScrollHandle,
    pub esc_armed: Option<Instant>,
    /// What was typed in the prompt but not sent.
    pub draft: String,
}

impl CrackView {
    /// Takes the active tab's state out of the view, leaving a blank one in place.
    fn stash_active(&mut self, cx: &mut Context<Self>) -> TabState {
        let draft = self.input.read(cx).text().to_owned();
        TabState {
            session: std::mem::replace(&mut self.session, Session::new(&self.ws.cwd)),
            running: self.running.take(),
            todos: std::mem::take(&mut self.todos),
            expanded: std::mem::take(&mut self.expanded),
            follow: std::mem::replace(&mut self.follow, true),
            scroll: std::mem::replace(&mut self.scroll, ScrollHandle::new()),
            esc_armed: self.esc_armed.take(),
            draft,
        }
    }

    fn restore_active(&mut self, id: u64, tab: TabState, cx: &mut Context<Self>) {
        self.tab_id = id;
        self.session = tab.session;
        self.running = tab.running;
        self.todos = tab.todos;
        self.expanded = tab.expanded;
        self.follow = tab.follow;
        self.scroll = tab.scroll;
        self.esc_armed = tab.esc_armed;
        self.autocomplete.clear();
        let draft = tab.draft;
        self.input.update(cx, |input, cx| input.set_text(draft, cx));
        cx.notify();
    }

    fn fresh_tab(&self) -> TabState {
        TabState {
            session: Session::new(&self.ws.cwd),
            running: None,
            todos: Arc::default(),
            expanded: HashSet::new(),
            follow: true,
            scroll: ScrollHandle::new(),
            esc_armed: None,
            draft: String::new(),
        }
    }

    /// Opens an empty tab after the current one and switches to it.
    pub(super) fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let position = self
            .tab_order
            .iter()
            .position(|t| *t == self.tab_id)
            .map_or(self.tab_order.len(), |p| p + 1);
        self.tab_order.insert(position, id);
        let current = self.stash_active(cx);
        self.background.insert(self.tab_id, current);
        let fresh = self.fresh_tab();
        self.restore_active(id, fresh, cx);
        self.focus_prompt(window, cx);
    }

    pub(super) fn switch_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if id == self.tab_id {
            return;
        }
        let Some(target) = self.background.remove(&id) else {
            return;
        };
        let current = self.stash_active(cx);
        self.background.insert(self.tab_id, current);
        self.restore_active(id, target, cx);
        self.focus_prompt(window, cx);
    }

    /// Moves to the next (`1`) or previous (`-1`) tab, wrapping around.
    pub(super) fn cycle_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let len = self.tab_order.len() as isize;
        if len < 2 {
            return;
        }
        let index = self
            .tab_order
            .iter()
            .position(|t| *t == self.tab_id)
            .unwrap_or(0) as isize;
        let next = self.tab_order[(index + delta).rem_euclid(len) as usize];
        self.switch_tab(next, window, cx);
    }

    /// Selects the tab at `index` (0-based), as ctrl-x 1..9 do.
    pub(super) fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.tab_order.get(index).copied() {
            self.switch_tab(id, window, cx);
        }
    }

    /// Closes a tab, stopping its turn if one is running. The last tab is replaced
    /// by an empty one rather than closed.
    pub(super) fn close_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if id != self.tab_id {
            if let Some(tab) = self.background.remove(&id)
                && let Some(running) = &tab.running
            {
                let _ = running.cancel.send(true);
            }
            self.tab_order.retain(|t| *t != id);
            cx.notify();
            return;
        }
        if let Some(running) = &self.running {
            let _ = running.cancel.send(true);
        }
        let index = self.tab_order.iter().position(|t| *t == id).unwrap_or(0);
        self.tab_order.retain(|t| *t != id);
        if self.tab_order.is_empty() {
            let new_id = self.next_tab_id;
            self.next_tab_id += 1;
            self.tab_order.push(new_id);
            let fresh = self.fresh_tab();
            self.restore_active(new_id, fresh, cx);
        } else {
            let next = self.tab_order[index.min(self.tab_order.len() - 1)];
            let target = self
                .background
                .remove(&next)
                .unwrap_or_else(|| self.fresh_tab());
            // The closed tab's state is dropped here.
            let _ = self.stash_active(cx);
            self.restore_active(next, target, cx);
        }
        self.focus_prompt(window, cx);
    }

    /// The tab already showing `session_id`, if any.
    pub(super) fn tab_with_session(&self, session_id: &str) -> Option<u64> {
        if self.session.id == session_id {
            return Some(self.tab_id);
        }
        self.background
            .iter()
            .find(|(_, t)| t.session.id == session_id)
            .map(|(id, _)| *id)
    }

    /// A turn's event for a tab that isn't showing.
    fn on_background_event(
        &mut self,
        tab: u64,
        event: AgentEvent,
        agent: Agent,
        model: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.background.get_mut(&tab) else {
            // The tab was closed; its turn has been told to stop.
            return;
        };
        state.session.apply(&event, agent, model);
        if let Some(running) = state.running.as_mut() {
            running.last_event = Instant::now();
        }
        if let AgentEvent::Done { .. } = event {
            state.running = None;
            state.esc_armed = None;
            state.session.todos = state.todos.lock().unwrap().clone();
            let title = state.session.title.clone();
            if let Err(error) = state.session.save() {
                self.show_toast(format!("Could not save the session: {error}"), true, cx);
            } else {
                self.show_toast(format!("Finished in another tab: {title}"), false, cx);
            }
        }
        cx.notify();
    }

    /// Title, running flag and active flag for each tab, in order.
    pub(super) fn tab_summaries(&self) -> Vec<(u64, String, bool, bool)> {
        self.tab_order
            .iter()
            .map(|id| {
                let (session, running) = if *id == self.tab_id {
                    (&self.session, self.running.is_some())
                } else {
                    match self.background.get(id) {
                        Some(t) => (&t.session, t.running.is_some()),
                        None => return (*id, "New session".to_owned(), false, false),
                    }
                };
                let title = if session.is_empty() {
                    "New session".to_owned()
                } else {
                    session.title.clone()
                };
                (*id, title, running, *id == self.tab_id)
            })
            .collect()
    }
}
