//! crack draws its own title bar: the icon, the tabs, an empty drag area, and
//! the window buttons. On Linux it asks for client-side decorations and adds resize
//! edges; on Windows the system title bar is hidden and the buttons map to the system's
//! own areas (so snap layouts still work); macOS keeps its traffic lights.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, CursorStyle, Decorations, InteractiveElement, IntoElement, MouseButton,
    ParentElement, ResizeEdge, StatefulInteractiveElement, Styled, Window, WindowControlArea, div,
    px,
};

use super::theme;
use super::view::CrackView;

pub const HEIGHT: f32 = 38.;
/// How far from the edge a press starts a resize (client-side decorations only).
const GRIP: f32 = 6.;

fn icon() -> std::sync::Arc<gpui::Image> {
    static ICON: std::sync::LazyLock<std::sync::Arc<gpui::Image>> =
        std::sync::LazyLock::new(|| {
            std::sync::Arc::new(gpui::Image::from_bytes(
                gpui::ImageFormat::Png,
                include_bytes!("../../assets/icon-64.png").to_vec(),
            ))
        });
    ICON.clone()
}

/// Whether crack draws the window buttons itself.
fn draws_controls(window: &Window) -> bool {
    cfg!(target_os = "windows") || matches!(window.window_decorations(), Decorations::Client { .. })
}

impl CrackView {
    pub(super) fn render_title_bar(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // What the tabs may use: the width less the icon, the "+" button, a strip to
        // drag the window by, and the window buttons.
        let controls_width = if draws_controls(window) { 3. * 44. } else { 0. };
        let leading_width = if cfg!(target_os = "macos") { 78. } else { 38. };
        let tab_room = f32::from(window.viewport_size().width)
            - leading_width
            - controls_width
            - 40.
            - 36.
            - 8.;
        let leading = if cfg!(target_os = "macos") {
            // Room for the traffic lights.
            div().w(px(78.)).flex_none().into_any_element()
        } else {
            div()
                .flex_none()
                .pl(px(12.))
                .pr(px(6.))
                .child(gpui::img(icon()).size(px(20.)))
                .into_any_element()
        };
        let drag = div()
            .id("title-drag")
            .flex_1()
            .min_w(px(40.))
            .h_full()
            .window_control_area(WindowControlArea::Drag)
            .on_mouse_down(MouseButton::Left, |event, window, _| {
                if event.click_count >= 2 {
                    window.zoom_window();
                } else {
                    window.start_window_move();
                }
            })
            .on_mouse_down(MouseButton::Right, |event, window, _| {
                window.show_window_menu(event.position)
            });

        div()
            .id("title-bar")
            .flex()
            .flex_row()
            .items_center()
            .h(px(HEIGHT))
            .flex_none()
            .bg(theme::bg())
            .border_b_1()
            .border_color(theme::border())
            .child(leading)
            .child(
                div()
                    .min_w_0()
                    .flex_shrink()
                    .h_full()
                    .child(self.render_tab_bar(tab_room, cx)),
            )
            .child(drag)
            .when(draws_controls(window), |bar| bar.child(controls(window)))
            .into_any_element()
    }
}

fn controls(window: &Window) -> AnyElement {
    let button = |id: &'static str, glyph: &'static str, area: WindowControlArea, close: bool| {
        div()
            .id(id)
            .w(px(44.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme::text_soft())
            .cursor_pointer()
            .window_control_area(area)
            .map(|d| {
                if close {
                    d.hover(|d| d.bg(theme::error()).text_color(theme::text()))
                } else {
                    d.hover(|d| d.bg(theme::raised_high()).text_color(theme::text()))
                }
            })
            .child(glyph)
    };
    let maximized = window.is_maximized();
    div()
        .flex()
        .flex_row()
        .h_full()
        .flex_none()
        .child(
            button("win-min", "\u{2013}", WindowControlArea::Min, false).on_click(
                |_, window, cx| {
                    cx.stop_propagation();
                    window.minimize_window()
                },
            ),
        )
        .child(
            button(
                "win-max",
                if maximized { "\u{2750}" } else { "\u{25A1}" },
                WindowControlArea::Max,
                false,
            )
            .on_click(|_, window, cx| {
                cx.stop_propagation();
                window.zoom_window()
            }),
        )
        .child(
            button("win-close", "\u{2715}", WindowControlArea::Close, true).on_click(
                |_, window, cx| {
                    cx.stop_propagation();
                    window.remove_window()
                },
            ),
        )
        .into_any_element()
}

/// Invisible strips along the window's edges and corners that resize it, when crack
/// draws its own decorations and the window isn't maximized or tiled.
pub fn resize_handles(window: &Window) -> Vec<AnyElement> {
    let Decorations::Client { tiling } = window.window_decorations() else {
        return Vec::new();
    };
    if window.is_maximized() || tiling.top || tiling.bottom || tiling.left || tiling.right {
        return Vec::new();
    }
    let grip = px(GRIP);
    let corner = px(GRIP * 2.);
    let edge = |id: &'static str, edge: ResizeEdge, cursor: CursorStyle| {
        div().id(id).absolute().cursor(cursor).on_mouse_down(
            MouseButton::Left,
            move |_, window, cx| {
                cx.stop_propagation();
                window.start_window_resize(edge)
            },
        )
    };
    use CursorStyle::*;
    vec![
        edge("rz-t", ResizeEdge::Top, ResizeUpDown)
            .top_0()
            .left(corner)
            .right(corner)
            .h(grip),
        edge("rz-b", ResizeEdge::Bottom, ResizeUpDown)
            .bottom_0()
            .left(corner)
            .right(corner)
            .h(grip),
        edge("rz-l", ResizeEdge::Left, ResizeLeftRight)
            .left_0()
            .top(corner)
            .bottom(corner)
            .w(grip),
        edge("rz-r", ResizeEdge::Right, ResizeLeftRight)
            .right_0()
            .top(corner)
            .bottom(corner)
            .w(grip),
        edge("rz-tl", ResizeEdge::TopLeft, ResizeUpLeftDownRight)
            .top_0()
            .left_0()
            .size(corner),
        edge("rz-tr", ResizeEdge::TopRight, ResizeUpRightDownLeft)
            .top_0()
            .right_0()
            .size(corner),
        edge("rz-bl", ResizeEdge::BottomLeft, ResizeUpRightDownLeft)
            .bottom_0()
            .left_0()
            .size(corner),
        edge("rz-br", ResizeEdge::BottomRight, ResizeUpLeftDownRight)
            .bottom_0()
            .right_0()
            .size(corner),
    ]
    .into_iter()
    .map(|e| e.into_any_element())
    .collect()
}
