//! Colors and type: a purple dark palette by default, with a light variant on
//! `CRACK_THEME=light` or `theme = "light"` in the config.

use gpui::{App, Hsla, Pixels, SharedString, px, rgb};

pub struct Palette {
    bg: u32,
    raised: u32,
    raised_high: u32,
    raised_max: u32,
    prompt_bg: u32,
    text: u32,
    text_soft: u32,
    muted: u32,
    border: u32,
    primary: u32,
    secondary: u32,
    selected_bg: u32,
    error: u32,
    warning: u32,
    success: u32,
    md_heading: u32,
    md_code: u32,
    md_strong: u32,
    md_emphasis: u32,
    md_link: u32,
    md_list: u32,
    diff_added: u32,
    diff_removed: u32,
    diff_context: u32,
    diff_added_bg: u32,
    diff_removed_bg: u32,
    chip: u32,
    on_chip: u32,
    backdrop: u32,
}

/// Background #15121E, panel #1F182C, hover #2B243D, border #312A43,
/// button #4C3F73, header #C4A9FF, sub-header #957FD1, text #E8E6F2.
const DARK: Palette = Palette {
    bg: 0x15121E,
    raised: 0x1F182C,
    raised_high: 0x2B243D,
    raised_max: 0x312A43,
    prompt_bg: 0x2B243D,
    text: 0xE8E6F2,
    text_soft: 0xD7CCF4,
    muted: 0xA29EC0,
    border: 0x312A43,
    primary: 0xC4A9FF,
    secondary: 0x9ED3FF,
    selected_bg: 0x4C3F73,
    error: 0xFF6B6B,
    warning: 0xFFB74D,
    success: 0x3DDC84,
    md_heading: 0xC4A9FF,
    md_code: 0xA4F0C1,
    md_strong: 0xFFCF8E,
    md_emphasis: 0xBCA8FF,
    md_link: 0x9ED3FF,
    md_list: 0x957FD1,
    diff_added: 0x3DDC84,
    diff_removed: 0xFF6B6B,
    diff_context: 0x957FD1,
    diff_added_bg: 0x1A2A2A,
    diff_removed_bg: 0x2E1A2A,
    chip: 0x623DA6,
    on_chip: 0xE8E6F2,
    backdrop: 0x080B12,
};

/// The light variant.
const LIGHT: Palette = Palette {
    bg: 0xCFC8E4,
    raised: 0xD8D2EA,
    raised_high: 0xE8E4F4,
    raised_max: 0xD0C9E6,
    prompt_bg: 0xE8E4F4,
    text: 0x2D2242,
    text_soft: 0x59448E,
    muted: 0x6D58AF,
    border: 0xD0C9E6,
    primary: 0x4F3795,
    secondary: 0x1F5F99,
    selected_bg: 0xD7D2EF,
    error: 0xB51A17,
    warning: 0x996100,
    success: 0x24753A,
    md_heading: 0x4F3795,
    md_code: 0x24753A,
    md_strong: 0x996100,
    md_emphasis: 0x6D58AF,
    md_link: 0x1F5F99,
    md_list: 0x6D58AF,
    diff_added: 0x24753A,
    diff_removed: 0xB51A17,
    diff_context: 0x6D58AF,
    diff_added_bg: 0xD3EBDB,
    diff_removed_bg: 0xF1D6DA,
    chip: 0x422B7A,
    on_chip: 0xE8E6F2,
    backdrop: 0x2D2242,
};

static LIGHT_THEME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn p() -> &'static Palette {
    if LIGHT_THEME.load(std::sync::atomic::Ordering::Relaxed) {
        &LIGHT
    } else {
        &DARK
    }
}

/// Picks the palette: `CRACK_THEME`, then the Settings choice, then the config.
pub fn init_palette(name: Option<&str>) {
    let chosen = crate::state::State::load().theme;
    let theme = std::env::var("CRACK_THEME")
        .ok()
        .or(chosen)
        .or(name.map(str::to_owned));
    set_light(theme.as_deref() == Some("light"));
}

pub fn set_light(light: bool) {
    LIGHT_THEME.store(light, std::sync::atomic::Ordering::Relaxed);
}

pub fn is_light() -> bool {
    LIGHT_THEME.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

pub fn ca(hex: u32, alpha: f32) -> Hsla {
    let mut color: Hsla = rgb(hex).into();
    color.a = alpha;
    color
}

macro_rules! colors {
    ($($name:ident),* $(,)?) => { $( pub fn $name() -> Hsla { c(p().$name) } )* };
}

colors!(
    bg,
    raised,
    raised_high,
    raised_max,
    prompt_bg,
    text,
    text_soft,
    muted,
    border,
    primary,
    secondary,
    selected_bg,
    error,
    warning,
    success,
    md_heading,
    md_code,
    md_strong,
    md_emphasis,
    md_link,
    md_list,
    diff_added,
    diff_removed,
    diff_context,
    diff_added_bg,
    diff_removed_bg,
    chip,
    on_chip,
);

pub fn md_text() -> Hsla {
    text()
}

pub fn backdrop() -> Hsla {
    ca(p().backdrop, 0.6)
}

pub fn selection() -> Hsla {
    ca(p().primary, 0.3)
}

pub fn thinking() -> Hsla {
    ca(p().sub_header(), 0.85)
}

impl Palette {
    fn sub_header(&self) -> u32 {
        if std::ptr::eq(self, &LIGHT) {
            0x6D58AF
        } else {
            0x957FD1
        }
    }
}

pub fn agent_color(agent: crate::tools::Agent) -> Hsla {
    match agent {
        crate::tools::Agent::Build => primary(),
        crate::tools::Agent::Plan => secondary(),
    }
}

pub const TEXT_SIZE: Pixels = px(14.);
pub const SMALL_SIZE: Pixels = px(13.);
pub const LINE_HEIGHT: Pixels = px(21.);

static FONT: std::sync::RwLock<Option<SharedString>> = std::sync::RwLock::new(None);

/// The interface font: `CRACK_FONT`, or the first installed of a list of monospace
/// fonts.
pub fn init_font(cx: &App) {
    let installed = cx.text_system().all_font_names();
    let preferred = std::env::var("CRACK_FONT")
        .ok()
        .or(crate::state::State::load().font);
    let candidates = preferred
        .iter()
        .map(String::as_str)
        .chain(MONOSPACE_FONTS.iter().copied());
    let font = candidates
        .into_iter()
        .find(|name| installed.iter().any(|f| f == name))
        .unwrap_or("monospace");
    set_font(font);
}

pub fn set_font(name: &str) {
    if let Ok(mut font) = FONT.write() {
        *font = Some(name.to_owned().into());
    }
}

/// Monospace fonts crack looks for, in order.
pub const MONOSPACE_FONTS: &[&str] = &[
    "JetBrains Mono",
    "Berkeley Mono",
    "Iosevka",
    "Fira Code",
    "Fira Mono",
    "Cascadia Code",
    "Source Code Pro",
    "Hack",
    "DejaVu Sans Mono",
    "Menlo",
    "Consolas",
    "Liberation Mono",
];

pub fn font() -> SharedString {
    FONT.read()
        .ok()
        .and_then(|f| f.clone())
        .unwrap_or_else(|| "monospace".into())
}
