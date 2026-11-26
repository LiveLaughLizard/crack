//! The first-run welcome on the start screen: connect a provider, try a prompt, and the
//! few keys worth knowing. It goes once dismissed or after the first prompt; `/welcome`
//! brings it back.

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, div, px,
};

use super::theme;
use super::view::CrackView;

const SAMPLES: [&str; 3] = [
    "Explain how this project is structured",
    "Find one thing worth improving here and explain why",
    "Write tests for a function that has none",
];

const KEYS: [(&str, &str); 5] = [
    ("ctrl-p", "every command"),
    ("tab", "switch Build / Plan (read-only)"),
    ("esc esc", "stop a running turn"),
    ("ctrl-x l", "your sessions"),
    ("ctrl-x b", "the sidebar"),
];

impl CrackView {
    /// Shows the welcome again (`/welcome`).
    pub(super) fn show_welcome(&mut self, cx: &mut Context<Self>) {
        self.welcome = true;
        cx.notify();
    }

    /// Hides the welcome for good.
    pub(super) fn dismiss_welcome(&mut self, cx: &mut Context<Self>) {
        if self.welcome {
            self.welcome = false;
            let mut state = crate::state::State::load();
            state.welcomed = true;
            state.save();
            cx.notify();
        }
    }

    pub(super) fn render_welcome(&self, cx: &mut Context<Self>) -> AnyElement {
        let connected = self.selection.model.is_some();
        let step = |n: usize, title: &str, done: bool| {
            div()
                .flex()
                .flex_row()
                .gap(px(10.))
                .items_center()
                .child(
                    div()
                        .w(px(20.))
                        .text_color(if done {
                            theme::success()
                        } else {
                            theme::primary()
                        })
                        .font_weight(FontWeight::BOLD)
                        .child(if done {
                            "\u{2713}".to_owned()
                        } else {
                            format!("{n}.")
                        }),
                )
                .child(
                    div()
                        .font_weight(FontWeight::BOLD)
                        .text_color(if done { theme::muted() } else { theme::text() })
                        .child(title.to_owned()),
                )
        };
        let button = |id: &'static str, label: String| {
            div()
                .id(id)
                .px(px(12.))
                .py(px(4.))
                .rounded(px(4.))
                .bg(theme::raised_high())
                .text_color(theme::text_soft())
                .cursor_pointer()
                .hover(|d| d.bg(theme::selected_bg()).text_color(theme::text()))
                .child(label)
        };

        let mut samples = div().flex().flex_row().flex_wrap().gap(px(8.)).pl(px(30.));
        for (i, sample) in SAMPLES.iter().enumerate() {
            let text = sample.to_string();
            samples = samples.child(
                button(["sample-0", "sample-1", "sample-2"][i], sample.to_string()).on_click(
                    cx.listener(move |this, _, window, cx| {
                        let text = text.clone();
                        this.input.update(cx, |input, cx| input.set_text(text, cx));
                        this.focus_prompt(window, cx);
                    }),
                ),
            );
        }
        let mut keys = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_x(px(18.))
            .gap_y(px(4.))
            .pl(px(30.));
        for (key, what) in KEYS {
            keys = keys.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.))
                    .child(div().text_color(theme::text()).child(key))
                    .child(div().text_color(theme::muted()).child(what)),
            );
        }

        let panel = div()
            .w_full()
            .max_w(px(780.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(18.))
            .mb(px(16.))
            .rounded(px(6.))
            .bg(theme::raised())
            .border_l_2()
            .border_color(theme::primary())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_size(theme::TEXT_SIZE)
                            .child("Welcome to crack"),
                    )
                    .child(
                        div()
                            .id("welcome-dismiss")
                            .text_size(theme::SMALL_SIZE)
                            .text_color(theme::muted())
                            .cursor_pointer()
                            .hover(|d| d.text_color(theme::text()))
                            .child("Got it")
                            .on_click(cx.listener(|this, _, _, cx| this.dismiss_welcome(cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(step(1, "Connect a model provider", connected))
                    .when(!connected, |d| {
                        d.child(
                            div().pl(px(30.)).child(
                                button("welcome-connect", "Connect a provider  \u{2192}".into())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_providers(None, window, cx)
                                    })),
                            ),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(step(
                        2,
                        "Ask about this project (click one to try it)",
                        false,
                    ))
                    .child(samples),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .text_size(theme::SMALL_SIZE)
                    .child(step(3, "Keys worth knowing", false))
                    .child(keys),
            );
        div()
            .flex()
            .flex_row()
            .justify_center()
            .child(panel)
            .into_any_element()
    }
}
