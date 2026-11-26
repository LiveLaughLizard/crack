//! The GPUI interface. It looks and behaves like opencode's terminal interface: a
//! home screen with the logo and prompt, a session screen with the transcript,
//! prompt and sidebar, and keyboard-driven dialogs.

mod attach;
mod extensions;
mod input;
mod markdown;
mod memory;
mod picker;
mod providers;
pub mod render;
mod settings;
mod theme;
mod titlebar;
mod undo;
mod view;
mod welcome;

use crate::workspace::{Selection, Workspace};
use gpui::{
    App, AppContext as _, Application, Bounds, KeyBinding, Pixels, TitlebarOptions, WindowBounds,
    WindowOptions, actions, px, size,
};

actions!(
    crack,
    [
        CommandPalette,
        Providers,
        ModelPicker,
        EffortPicker,
        AgentPicker,
        SessionPicker,
        SkillPicker,
        Help,
        NewSession,
        Undo,
        Redo,
        ShowDiff,
        OpenSettings,
        ToggleSidebar,
        CycleAgent,
        CycleEffort,
        CycleRecentModel,
        Interrupt,
        ClearPrompt,
        CloseTabArmed,
        Quit,
        CopyLast,
        PageUp,
        PageDown,
        ScrollTop,
        ScrollBottom,
        Tab,
        NewTab,
        CloseTab,
        NextTab,
        PrevTab,
        SelectTab1,
        SelectTab2,
        SelectTab3,
        SelectTab4,
        SelectTab5,
        SelectTab6,
        SelectTab7,
        SelectTab8,
        SelectTab9,
        OpenRecent1,
        OpenRecent2,
        OpenRecent3,
        OpenRecent4,
        OpenRecent5,
        ApproveOnce,
        ApproveSession,
        ApprovePartial,
        ApproveAlways,
        DenyApproval,
    ]
);

pub(crate) const KEYMAP: &[(&str, &str)] = &[
    ("ctrl-p", "Command palette"),
    ("ctrl-x p", "Providers: connect API keys and cloud accounts"),
    ("ctrl-x m", "Switch model"),
    ("f2", "Cycle recent models"),
    ("ctrl-t", "Cycle reasoning effort"),
    ("tab / shift-tab", "Cycle agent (Build / Plan)"),
    ("ctrl-x a", "Switch agent"),
    ("ctrl-x n", "New session in this tab"),
    ("ctrl-x t", "New tab"),
    ("ctrl-x w", "Close tab"),
    ("ctrl-tab / ctrl-shift-tab", "Next / previous tab"),
    ("ctrl-x 1-9", "Go to tab 1-9"),
    ("ctrl-x l", "Sessions"),
    ("alt-1-5", "Open a recent session (start screen)"),
    ("ctrl-x k", "Skills"),
    ("ctrl-x b", "Toggle sidebar"),
    ("ctrl-x y", "Copy last response"),
    ("ctrl-x h", "Help"),
    ("esc esc", "Interrupt the running turn"),
    ("enter", "Send"),
    ("shift-enter / ctrl-j", "New line"),
    ("up / down", "Prompt history"),
    ("pageup / pagedown", "Scroll"),
    ("ctrl-c", "Clear the prompt"),
    ("ctrl-d ctrl-d", "Close this tab"),
    ("ctrl-x q", "Quit"),
];

fn bind_keys(cx: &mut App) {
    input::bind_keys(cx);
    picker::bind_keys(cx);
    providers::bind_keys(cx);
    let ws = Some("Workspace");
    let prompt = Some("Prompt");
    cx.bind_keys([
        KeyBinding::new("ctrl-p", CommandPalette, ws),
        KeyBinding::new("ctrl-,", OpenSettings, ws),
        KeyBinding::new("ctrl-x u", Undo, ws),
        KeyBinding::new("ctrl-x r", Redo, ws),
        KeyBinding::new("ctrl-x d", ShowDiff, ws),
        KeyBinding::new("ctrl-x p", Providers, ws),
        KeyBinding::new("ctrl-x m", ModelPicker, ws),
        KeyBinding::new("ctrl-x e", EffortPicker, ws),
        KeyBinding::new("ctrl-x a", AgentPicker, ws),
        KeyBinding::new("ctrl-x l", SessionPicker, ws),
        KeyBinding::new("ctrl-x k", SkillPicker, ws),
        KeyBinding::new("ctrl-x h", Help, ws),
        KeyBinding::new("ctrl-x n", NewSession, ws),
        KeyBinding::new("ctrl-x t", NewTab, ws),
        KeyBinding::new("ctrl-x w", CloseTab, ws),
        KeyBinding::new("ctrl-tab", NextTab, ws),
        KeyBinding::new("ctrl-shift-tab", PrevTab, ws),
        KeyBinding::new("ctrl-x 1", SelectTab1, ws),
        KeyBinding::new("ctrl-x 2", SelectTab2, ws),
        KeyBinding::new("ctrl-x 3", SelectTab3, ws),
        KeyBinding::new("ctrl-x 4", SelectTab4, ws),
        KeyBinding::new("ctrl-x 5", SelectTab5, ws),
        KeyBinding::new("ctrl-x 6", SelectTab6, ws),
        KeyBinding::new("ctrl-x 7", SelectTab7, ws),
        KeyBinding::new("ctrl-x 8", SelectTab8, ws),
        KeyBinding::new("ctrl-x 9", SelectTab9, ws),
        KeyBinding::new("alt-1", OpenRecent1, ws),
        KeyBinding::new("alt-2", OpenRecent2, ws),
        KeyBinding::new("alt-3", OpenRecent3, ws),
        KeyBinding::new("alt-4", OpenRecent4, ws),
        KeyBinding::new("alt-5", OpenRecent5, ws),
        KeyBinding::new("ctrl-x b", ToggleSidebar, ws),
        KeyBinding::new("ctrl-x y", CopyLast, ws),
        KeyBinding::new("ctrl-x q", Quit, ws),
        KeyBinding::new("ctrl-t", CycleEffort, ws),
        KeyBinding::new("f2", CycleRecentModel, ws),
        KeyBinding::new("shift-tab", CycleAgent, ws),
        KeyBinding::new("tab", Tab, prompt),
        KeyBinding::new("escape", Interrupt, ws),
        KeyBinding::new("ctrl-c", ClearPrompt, ws),
        KeyBinding::new("ctrl-d", CloseTabArmed, ws),
        KeyBinding::new("pageup", PageUp, ws),
        KeyBinding::new("pagedown", PageDown, ws),
        KeyBinding::new("ctrl-g", ScrollTop, ws),
        KeyBinding::new("ctrl-alt-g", ScrollBottom, ws),
        // The approval card, when it has focus.
        KeyBinding::new("y", ApproveOnce, Some("Approval")),
        KeyBinding::new("enter", ApproveOnce, Some("Approval")),
        KeyBinding::new("a", ApproveSession, Some("Approval")),
        KeyBinding::new("p", ApprovePartial, Some("Approval")),
        KeyBinding::new("shift-a", ApproveAlways, Some("Approval")),
        KeyBinding::new("n", DenyApproval, Some("Approval")),
        KeyBinding::new("escape", DenyApproval, Some("Approval")),
    ]);
}

/// The smallest the window can be resized to.
const MIN_WIDTH: Pixels = px(900.);
const MIN_HEIGHT: Pixels = px(750.);

/// Opens the window and runs until it closes.
pub fn run(
    workspace: Workspace,
    selection: Selection,
    prompt: Option<String>,
    session: Option<String>,
) {
    // Start the async runtime before the window, on its own threads.
    crate::runtime::get();
    theme::init_palette(workspace.config.theme.as_deref());
    Application::new().run(move |cx: &mut App| {
        theme::init_font(cx);
        bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let title = format!("crack — {}", workspace.short_cwd());
        // Wide and not too tall, like a terminal: a share of the screen, within limits.
        let screen = cx
            .primary_display()
            .map(|d| d.bounds().size)
            .unwrap_or(size(px(1920.), px(1080.)));
        let default_size = size(
            (screen.width * 0.6).clamp(MIN_WIDTH, px(1280.)),
            (screen.height * 0.6).clamp(MIN_HEIGHT, px(900.)),
        );
        // The size the window had last time, kept within the screen.
        let remembered = crate::state::State::load();
        let window_size = match (remembered.window_width, remembered.window_height) {
            (Some(w), Some(h)) => size(
                px(w).clamp(MIN_WIDTH, screen.width.max(MIN_WIDTH)),
                px(h).clamp(MIN_HEIGHT, screen.height.max(MIN_HEIGHT)),
            ),
            _ => default_size,
        };
        let bounds = Bounds::centered(None, window_size, cx);
        let window_bounds = if remembered.window_maximized {
            WindowBounds::Maximized(bounds)
        } else {
            WindowBounds::Windowed(bounds)
        };
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(window_bounds),
                    window_min_size: Some(size(MIN_WIDTH, MIN_HEIGHT)),
                    // crack draws its own title bar (ui/titlebar.rs).
                    titlebar: Some(TitlebarOptions {
                        title: Some(title.into()),
                        appears_transparent: true,
                        traffic_light_position: Some(gpui::point(px(12.), px(12.))),
                    }),
                    window_decorations: Some(gpui::WindowDecorations::Client),
                    app_id: Some("crack".into()),
                    ..Default::default()
                },
                move |window, cx| {
                    cx.new(|cx| {
                        view::CrackView::new(workspace, selection, prompt, session, window, cx)
                    })
                },
            )
            .expect("could not open the window");
        let _ = window.update(cx, |view, window, cx| view.focus_prompt(window, cx));
        cx.on_window_closed(|cx| cx.quit()).detach();
        // However crack quits: save every tab, remember which were open, and wait
        // for the database writes to land.
        cx.on_app_quit(move |cx| {
            let _ = window.update(cx, |view, _, cx| view.before_quit(cx));
            for error in crate::store::get().flush() {
                eprintln!("warning: could not save: {error}");
            }
            async {}
        })
        .detach();
        cx.activate(true);
    });
}
