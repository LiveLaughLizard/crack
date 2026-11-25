//! Drawing the window: home screen, transcript, prompt, sidebar and overlays.

use super::picker::PickerKind;
use super::theme;
use super::view::CrackView;
use super::*;
use crate::context;
use crate::providers::Effort;
use crate::session::{Entry, ToolState};
use crate::tools::{Agent, DisplayKind, ToolMeta};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, FontWeight, Hsla, InteractiveElement,
    IntoElement, MouseButton, ParentElement, Render, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use std::time::Duration;

/// Three pulsing dots, drawn rather than typed so they show in any font.
fn spinner(id: impl Into<gpui::ElementId>, color: Hsla) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(3.))
        .h(px(14.))
        .with_animation(
            id,
            Animation::new(Duration::from_millis(1200)).repeat(),
            move |mut el, delta| {
                for i in 0..3 {
                    // Each dot brightens in turn.
                    let phase = (delta * 3. - i as f32).rem_euclid(3.);
                    let mut dot = color;
                    dot.a = if phase < 1. { 1. - phase * 0.65 } else { 0.35 };
                    el = el.child(div().size(px(5.)).rounded_full().bg(dot));
                }
                el
            },
        )
        .into_any_element()
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn tool_icon(name: &str) -> &'static str {
    match name {
        "read" | "list" | "skill" => "→",
        "glob" | "grep" => "✱",
        "webfetch" => "%",
        "write" | "edit" => "←",
        "bash" => "$",
        "todowrite" => "⚙",
        _ => "⚙",
    }
}

impl CrackView {
    fn model_label(&self) -> (String, String) {
        match self.selection.model.as_deref() {
            Some(model_ref) => {
                let (provider, model) = model_ref.split_once('/').unwrap_or(("", model_ref));
                let provider_name = self
                    .ws
                    .registry
                    .find(provider)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| provider.to_owned());
                (model.to_owned(), provider_name)
            }
            None if self.discovering => ("looking for models…".into(), String::new()),
            None => ("no model configured".into(), String::new()),
        }
    }

    fn location(&self) -> String {
        match context::git_branch(&self.ws.cwd) {
            Some(branch) => format!("{}:{branch}", self.ws.short_cwd()),
            None => self.ws.short_cwd(),
        }
    }

    /// The logo: the flexed arm and CRACK lettering. Up to 260px tall, smaller on
    /// short windows so the prompt stays in view. The PNG is 2x for HiDPI screens.
    fn render_logo(&self, window: &Window) -> AnyElement {
        static LOGO: std::sync::LazyLock<std::sync::Arc<gpui::Image>> =
            std::sync::LazyLock::new(|| {
                std::sync::Arc::new(gpui::Image::from_bytes(
                    gpui::ImageFormat::Png,
                    include_bytes!("../../assets/logo.png").to_vec(),
                ))
            });
        // The asset is 435x520.
        let height = (window.viewport_size().height * 0.32).clamp(px(120.), px(260.));
        gpui::img(LOGO.clone())
            .h(height)
            .w(height * (435. / 520.))
            .into_any_element()
    }

    fn render_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let agent = self.selection.agent;
        let agent_color = theme::agent_color(agent);
        let (model, provider) = self.model_label();

        // Each part of the line is a button: agent cycles, model and effort open pickers.
        let has_model = self.selection.model.is_some();
        let mut meta = div()
            .flex()
            .flex_row()
            .gap(px(6.))
            .text_size(theme::SMALL_SIZE)
            .child(
                div()
                    .id("meta-agent")
                    .text_color(agent_color)
                    .cursor_pointer()
                    .hover(|d| d.underline())
                    .child(agent.name())
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_agent(cx))),
            )
            .child(div().text_color(theme::muted()).child("·"))
            .child(
                div()
                    .id("meta-model")
                    .flex()
                    .flex_row()
                    .gap(px(6.))
                    .cursor_pointer()
                    .hover(|d| d.underline())
                    .child(
                        div()
                            .text_color(if has_model {
                                theme::text()
                            } else {
                                theme::warning()
                            })
                            .child(if has_model {
                                model
                            } else {
                                "Connect a provider…".into()
                            }),
                    )
                    .child(div().text_color(theme::muted()).child(provider))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if has_model {
                            this.open_picker(PickerKind::Model, window, cx)
                        } else {
                            this.open_providers(None, window, cx)
                        }
                    })),
            );
        // "default" means medium where the model has it; show what will be sent.
        let is_default = self.selection.effort == Effort::Default;
        let resolves_to_medium = is_default
            && self
                .selection
                .model
                .as_deref()
                .and_then(|m| self.ws.registry.resolve(m).ok())
                .is_some_and(|(p, _)| p.endpoint.has_medium_effort());
        let effort_label = if resolves_to_medium {
            "medium".to_owned()
        } else if is_default {
            "effort".to_owned()
        } else {
            self.selection.effort.as_str().to_owned()
        };
        meta = meta
            .child(div().text_color(theme::muted()).child("·"))
            .child(
                div()
                    .id("meta-effort")
                    .cursor_pointer()
                    .hover(|d| d.underline())
                    .when(is_default && !resolves_to_medium, |d| {
                        d.text_color(theme::muted())
                    })
                    // An effort that will be sent looks the same whether chosen or defaulted.
                    .when(!is_default || resolves_to_medium, |d| {
                        d.text_color(theme::warning()).font_weight(FontWeight::BOLD)
                    })
                    .child(effort_label)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_picker(PickerKind::Effort, window, cx)
                    })),
            );

        // The permission mode, when it isn't the normal one.
        match self.selection.mode {
            crate::policy::Mode::Normal => {}
            crate::policy::Mode::AcceptEdits => {
                meta = meta
                    .child(div().text_color(theme::muted()).child("·"))
                    .child(div().text_color(theme::warning()).child("accept edits"));
            }
            #[cfg(feature = "yolo")]
            crate::policy::Mode::Yolo => {
                meta = meta
                    .child(div().text_color(theme::muted()).child("·"))
                    .child(
                        div()
                            .text_color(theme::error())
                            .font_weight(FontWeight::BOLD)
                            .child("YOLO"),
                    );
            }
        }

        let mut column = div().flex().flex_col().w_full();

        if let Some(card) = self.render_approval(cx) {
            column = column.child(card);
        } else if let Some(status) = self.render_status_line() {
            column = column.child(status);
        }

        // Slash-command completion, above the prompt.
        if !self.autocomplete.is_empty() {
            let mut list = div()
                .flex()
                .flex_col()
                .bg(theme::raised_high())
                .py(px(4.))
                .mb(px(4.));
            for (i, item) in self.autocomplete.iter().take(10).enumerate() {
                let selected = i == self.ac_selected;
                list = list.child(
                    div()
                        .id(("ac", i))
                        .flex()
                        .flex_row()
                        .gap(px(12.))
                        .pr(px(16.))
                        .when(selected, |d| d.bg(theme::selected_bg()))
                        .child(
                            div()
                                .w(px(3.))
                                .h(px(20.))
                                .when(selected, |d| d.bg(theme::primary())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .min_w(px(160.))
                                .text_color(if item.skill {
                                    theme::secondary()
                                } else {
                                    theme::text()
                                })
                                .child(item.label.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(theme::muted())
                                .child(item.description.clone()),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.ac_selected = i;
                            let item = this.autocomplete[i].clone();
                            this.autocomplete.clear();
                            this.input
                                .update(cx, |input, cx| input.set_text(item.insert.clone(), cx));
                            this.focus_prompt(window, cx);
                        })),
                );
            }
            column = column.child(list);
        }

        column = column.child(
            div()
                .key_context("Prompt")
                .on_action(cx.listener(|this, _: &Tab, window, cx| {
                    if this.autocomplete.is_empty() {
                        this.cycle_agent(cx);
                    } else {
                        let item = this.autocomplete[this.ac_selected].clone();
                        this.autocomplete.clear();
                        this.input
                            .update(cx, |input, cx| input.set_text(item.insert.clone(), cx));
                        this.focus_prompt(window, cx);
                    }
                }))
                .flex()
                .flex_row()
                .bg(theme::prompt_bg())
                .border_l_2()
                .border_color(agent_color)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .px(px(16.))
                        .pt(px(12.))
                        .pb(px(10.))
                        .gap(px(10.))
                        .children(self.render_attachments(cx))
                        .child(self.input.clone())
                        .child(meta),
                ),
        );

        // The line under the prompt.
        let left: AnyElement = if !self.session.is_empty() && self.session.context_tokens > 0 {
            let tokens = self.session.context_tokens;
            let percent = self
                .ws
                .registry
                .caps(self.selection.model.as_deref().unwrap_or(""))
                .context
                .map(|w| format!(" ({}%)", tokens * 100 / w))
                .unwrap_or_default();
            div()
                .text_color(theme::muted())
                .child(format!("{}{percent}", thousands(tokens)))
                .into_any_element()
        } else {
            // The directory is in the footer and sidebar; here it would crowd out the hints.
            div().flex_1().into_any_element()
        };
        let key = |k: &'static str, label: &'static str| {
            div()
                .id(k)
                .flex()
                .flex_row()
                .gap(px(6.))
                .cursor_pointer()
                .hover(|d| d.underline())
                .child(div().text_color(theme::text()).child(k))
                .child(div().text_color(theme::muted()).child(label))
        };
        column = column.child(
            div()
                .flex()
                .flex_row()
                .justify_between()
                .pt(px(8.))
                .gap(px(18.))
                .text_size(theme::SMALL_SIZE)
                .child(left)
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .gap(px(18.))
                        .child(
                            key("tab", "agents")
                                .on_click(cx.listener(|this, _, _, cx| this.cycle_agent(cx))),
                        )
                        .child(key("ctrl+x p", "providers").on_click(
                            cx.listener(|this, _, window, cx| {
                                this.open_providers(None, window, cx)
                            }),
                        ))
                        .child(key("ctrl+p", "commands").on_click(cx.listener(
                            |this, _, window, cx| this.open_picker(PickerKind::Command, window, cx),
                        ))),
                ),
        );
        let _ = window;
        column.into_any_element()
    }

    fn render_home(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let mut status = div()
            .flex_none()
            .flex()
            .flex_row()
            .gap(px(18.))
            .text_color(theme::muted());
        let agents_files = self.ws.instructions.len();
        status = status.child(
            div()
                .flex()
                .flex_row()
                .gap(px(6.))
                .child(
                    div()
                        .text_color(if agents_files > 0 {
                            theme::success()
                        } else {
                            theme::muted()
                        })
                        .child("⊙"),
                )
                .child(format!(
                    "{agents_files} instruction file{}",
                    if agents_files == 1 { "" } else { "s" }
                )),
        );
        status = status.child(format!(
            "{} skill{}",
            self.ws.skills.len(),
            if self.ws.skills.len() == 1 { "" } else { "s" }
        ));
        if let Some((label, color)) = self.mcp_summary() {
            status = status.child(
                div()
                    .id("mcp-status")
                    .flex()
                    .flex_row()
                    .gap(px(6.))
                    .cursor_pointer()
                    .hover(|d| d.underline())
                    .child(div().text_color(color).child("⊙"))
                    .child(label)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_picker(PickerKind::Mcp, window, cx)
                    })),
            );
        }

        // With nothing connected, the way forward is a button, not instructions.
        let mut hint = div()
            .min_h(px(40.))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.));
        if self.selection.model.is_none() && !self.discovering {
            hint = hint
                .child(
                    div()
                        .text_color(theme::text_soft())
                        .child("Connect a model provider to get started."),
                )
                .child(
                    div()
                        .id("connect-cta")
                        .px(px(18.))
                        .py(px(7.))
                        .rounded(px(4.))
                        .bg(theme::chip())
                        .text_color(theme::on_chip())
                        .font_weight(FontWeight::BOLD)
                        .cursor_pointer()
                        .hover(|d| d.bg(theme::selected_bg()))
                        .child("Connect a provider  →")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_providers(None, window, cx)
                        })),
                )
                .child(
                    div()
                        .text_size(theme::SMALL_SIZE)
                        .text_color(theme::muted())
                        .child(if cfg!(feature = "china") {
                            "Anthropic, OpenAI, Gemini, Bedrock, Vertex AI, Azure, Qwen, Kimi, Z.ai, Ollama, …"
                        } else if cfg!(feature = "cloud") {
                            "Anthropic, OpenAI, Gemini, Bedrock, Vertex AI, Azure, OpenRouter, Ollama, …"
                        } else {
                            "Air-gapped build: Ollama, LM Studio, or any self-hosted OpenAI-compatible server"
                        }),
                );
        }

        div()
            .flex()
            .flex_col()
            .size_full()
            .px(px(24.))
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_center()
                    .child(self.render_logo(window)),
            )
            .child(div().h(px(32.)))
            .child(
                div().flex().flex_row().justify_center().child(
                    div()
                        .w_full()
                        .min_w_0()
                        .max_w(px(780.))
                        .overflow_hidden()
                        .child(self.render_prompt(window, cx)),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_center()
                    .pt(px(16.))
                    .child(div().max_w(px(780.)).child(hint)),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .gap(px(24.))
                    .py(px(12.))
                    .text_size(theme::SMALL_SIZE)
                    .child(status)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(18.))
                            .min_w_0()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme::muted())
                                    .child(self.location()),
                            )
                            .child(div().text_color(theme::muted()).child(
                                if cfg!(feature = "china") {
                                    concat!("v", env!("CARGO_PKG_VERSION")).to_owned()
                                } else {
                                    format!(
                                        "v{} · {}",
                                        env!("CARGO_PKG_VERSION"),
                                        crate::providers::registry::build_flavor()
                                    )
                                },
                            )),
                    ),
            )
            .into_any_element()
    }

    fn render_tool(
        &self,
        index: usize,
        id: &str,
        name: &str,
        state: ToolState,
        meta: &ToolMeta,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let expanded = self.expanded.contains(id);
        let failed = state == ToolState::Error;
        let running = matches!(state, ToolState::Pending | ToolState::Running);

        let block = !failed
            && !running
            && matches!(
                meta.kind,
                DisplayKind::Shell | DisplayKind::Diff | DisplayKind::Write | DisplayKind::Todos
            );
        if !block {
            let color = if failed {
                theme::error()
            } else {
                theme::muted()
            };
            let icon: AnyElement = if state == ToolState::Waiting {
                // Waiting on the approval card above the prompt.
                div()
                    .text_color(theme::warning())
                    .font_weight(FontWeight::BOLD)
                    .child("?")
                    .into_any_element()
            } else if running {
                spinner(("tool-spin", index), theme::muted())
            } else if failed {
                div()
                    .text_color(theme::error())
                    .child("✗")
                    .into_any_element()
            } else {
                div().child(tool_icon(name)).into_any_element()
            };
            let mut row = div()
                .id(("tool", index))
                .flex()
                .flex_col()
                .pl(px(24.))
                .text_color(color)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.))
                        .child(div().w(px(22.)).flex_none().child(icon))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(meta.title.clone()),
                        ),
                );
            // A running subagent's live status; a finished one links to its transcript.
            if running
                && name == "task"
                && let Some(status) = &meta.detail
            {
                row = row.child(
                    div()
                        .pl(px(30.))
                        .text_size(theme::SMALL_SIZE)
                        .truncate()
                        .child(status.clone()),
                );
            }
            if !running && let Some(link) = meta.link.clone() {
                return row
                    .child(
                        div()
                            .pl(px(30.))
                            .text_size(theme::SMALL_SIZE)
                            .text_color(theme::secondary())
                            .child("open transcript \u{203a}"),
                    )
                    .cursor_pointer()
                    .hover(|d| d.text_color(theme::text()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_transcript(link.clone(), window, cx)
                    }))
                    .into_any_element();
            }
            if failed && let Some(detail) = &meta.detail {
                let shown: String = if expanded {
                    detail.clone()
                } else {
                    detail
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(200)
                        .collect()
                };
                row = row.child(div().pl(px(24.)).text_color(theme::error()).child(shown));
            }
            let id = id.to_owned();
            return row
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.expanded.remove(&id) {
                        this.expanded.insert(id.clone());
                    }
                    cx.notify();
                }))
                .into_any_element();
        }

        let detail = meta.detail.clone().unwrap_or_default();
        let is_diff = meta.kind == DisplayKind::Diff;
        let lines: Vec<&str> = detail
            .lines()
            .filter(|l| !(is_diff && (l.starts_with("---") || l.starts_with("+++"))))
            .collect();
        let limit = if expanded { usize::MAX } else { 10 };
        let header = match meta.kind {
            DisplayKind::Write => format!("# {}", meta.title),
            DisplayKind::Diff => format!("← {}", meta.title),
            DisplayKind::Todos => "# Todos".to_owned(),
            _ => meta.title.clone(),
        };
        let mut body = div().flex().flex_col().text_size(theme::SMALL_SIZE);
        let mut shown = 0;
        for line in lines.iter() {
            if shown >= limit {
                break;
            }
            shown += 1;
            let text = SharedString::from(if line.is_empty() {
                " ".to_owned()
            } else {
                line.to_string()
            });
            let el = match meta.kind {
                DisplayKind::Diff if line.starts_with('+') => div()
                    .bg(theme::diff_added_bg())
                    .text_color(theme::diff_added())
                    .child(text),
                DisplayKind::Diff if line.starts_with('-') => div()
                    .bg(theme::diff_removed_bg())
                    .text_color(theme::diff_removed())
                    .child(text),
                DisplayKind::Diff if line.starts_with("@@") => {
                    div().text_color(theme::diff_context()).child(text)
                }
                DisplayKind::Todos => {
                    let color = if line.starts_with("[✓]") {
                        theme::success()
                    } else if line.starts_with("[•]") {
                        theme::warning()
                    } else {
                        theme::text_soft()
                    };
                    div().text_color(color).child(text)
                }
                DisplayKind::Write => div().text_color(theme::text_soft()).child(text),
                _ => div().text_color(theme::muted()).child(text),
            };
            body = body.child(el);
        }
        let hidden = lines.len().saturating_sub(shown);
        if hidden > 0 && !expanded {
            body = body.child(
                div()
                    .text_color(theme::muted())
                    .child(format!("… {hidden} more lines (click to expand)")),
            );
        }
        let id = id.to_owned();
        div()
            .id(("tool", index))
            .flex()
            .flex_col()
            .gap(px(6.))
            .ml(px(24.))
            .bg(theme::raised())
            .px(px(16.))
            .py(px(10.))
            .cursor_pointer()
            .child(div().text_color(theme::muted()).child(header))
            .when(!detail.is_empty(), |d| d.child(body))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.expanded.remove(&id) {
                    this.expanded.insert(id.clone());
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_entry(&self, index: usize, entry: &Entry, cx: &mut Context<Self>) -> AnyElement {
        match entry {
            Entry::User {
                text,
                agent,
                skill,
                images,
            } => {
                let mut content = div().flex().flex_col().gap(px(8.));
                if let Some(skill) = skill {
                    content = content.child(
                        div()
                            .flex()
                            .flex_row()
                            .text_size(theme::SMALL_SIZE)
                            .child(
                                div()
                                    .px(px(6.))
                                    .bg(theme::chip())
                                    .text_color(theme::on_chip())
                                    .font_weight(FontWeight::BOLD)
                                    .child("skill"),
                            )
                            .child(
                                div()
                                    .px(px(6.))
                                    .bg(theme::raised_max())
                                    .text_color(theme::text_soft())
                                    .child(skill.clone()),
                            ),
                    );
                }
                if !images.is_empty() {
                    let mut chips = div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.))
                        .text_size(theme::SMALL_SIZE);
                    for name in images {
                        chips = chips.child(
                            div()
                                .px(px(6.))
                                .bg(theme::raised_max())
                                .text_color(theme::text_soft())
                                .child(format!("\u{25a3} {name}")),
                        );
                    }
                    content = content.child(chips);
                }
                let copy = text.clone();
                div()
                    .id(("user", index))
                    .bg(theme::raised())
                    .border_l_2()
                    .border_color(theme::agent_color(*agent))
                    .px(px(16.))
                    .py(px(10.))
                    .hover(|d| d.bg(theme::raised_high()))
                    .child(content.child(div().text_color(theme::text()).child(text.clone())))
                    .cursor_pointer()
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.rewind_to(index, window, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, _, cx| this.copy_text(copy.clone(), cx)),
                    )
                    .into_any_element()
            }
            Entry::Text { text } => div()
                .pl(px(24.))
                .child(super::markdown::render(
                    &self.markdown(text),
                    theme::md_text(),
                ))
                .into_any_element(),
            Entry::Thinking { text, done } => {
                let key = format!("thinking-{index}");
                let expanded = self.expanded.contains(&key);
                let color = theme::thinking();
                let summary: String = text
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("")
                    .trim_matches('*')
                    .to_owned();
                let mut column = div()
                    .id(("thinking", index))
                    .flex()
                    .flex_col()
                    .pl(px(24.))
                    .gap(px(6.));
                let head = if *done {
                    // The first line of the reasoning, wrapped; click shows all of it.
                    div()
                        .flex()
                        .flex_row()
                        .text_color(color)
                        .child(div().flex_1().min_w_0().child(if expanded {
                            "- Thought".to_owned()
                        } else {
                            format!("+ Thought: {summary}")
                        }))
                        .into_any_element()
                } else {
                    div()
                        .w_full()
                        .flex()
                        .flex_row()
                        .gap(px(8.))
                        .text_color(color)
                        .child(spinner(("think-spin", index), color))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(format!("Thinking: {summary}")),
                        )
                        .into_any_element()
                };
                column = column.child(head);
                if expanded && !text.trim().is_empty() {
                    column = column.child(
                        div()
                            .border_l_2()
                            .border_color(theme::raised_high())
                            .pl(px(12.))
                            .child(super::markdown::render(
                                &self.markdown(text),
                                theme::muted(),
                            )),
                    );
                }
                column
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded.remove(&key) {
                            this.expanded.insert(key.clone());
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Entry::Tool {
                id,
                name,
                state,
                meta,
                ..
            } => self.render_tool(index, id, name, *state, meta, cx),
            Entry::Footer {
                agent,
                model,
                seconds,
                interrupted,
            } => {
                let model_name = model
                    .split_once('/')
                    .map_or(model.as_str(), |(_, m)| m)
                    .to_owned();
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.))
                    .pl(px(24.))
                    .text_size(theme::SMALL_SIZE)
                    .child(
                        div()
                            .text_color(theme::agent_color(*agent))
                            .child(format!("▣ {}", agent.name())),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .child(format!("· {model_name} · {seconds:.1}s")),
                    )
                    .when(*interrupted, |d| {
                        d.child(div().text_color(theme::warning()).child("· interrupted"))
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id(("copy", index))
                            .text_color(theme::muted())
                            .cursor_pointer()
                            .hover(|d| d.text_color(theme::text()))
                            .child("copy")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                // The response this footer closes: the assistant text before it.
                                let text: Vec<String> = this.session.entries[..index]
                                    .iter()
                                    .rev()
                                    .take_while(|e| !matches!(e, Entry::User { .. }))
                                    .filter_map(|e| match e {
                                        Entry::Text { text } => Some(text.clone()),
                                        _ => None,
                                    })
                                    .collect();
                                let text: Vec<String> = text.into_iter().rev().collect();
                                this.copy_text(text.join("\n\n"), cx);
                            })),
                    )
                    .into_any_element()
            }
            Entry::Notice { text } => div()
                .pl(px(24.))
                .text_color(theme::muted())
                .child(text.clone())
                .into_any_element(),
            Entry::Error { text } => div()
                .pl(px(24.))
                .text_color(theme::error())
                .child(format!("Error: {text}"))
                .into_any_element(),
        }
    }

    fn render_session(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.follow {
            self.scroll.scroll_to_bottom();
        }
        let entries: Vec<AnyElement> = self
            .session
            .entries
            .iter()
            .enumerate()
            .map(|(i, entry)| self.render_entry(i, entry, cx))
            .collect();
        let transcript = div()
            .id("transcript")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                let delta = event.delta.pixel_delta(px(20.));
                if delta.y > px(0.) {
                    this.follow = false;
                } else {
                    let max = this.scroll.max_offset().height;
                    this.follow = -this.scroll.offset().y >= max - px(30.);
                }
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .px(px(24.))
                    .pt(px(20.))
                    .pb(px(12.))
                    .children(entries),
            );
        let jump = div()
            .h(px(20.))
            .px(px(24.))
            .flex()
            .flex_row()
            .justify_end()
            .when(!self.follow, |d| {
                d.child(
                    div()
                        .id("jump")
                        .text_color(theme::muted())
                        .text_size(theme::SMALL_SIZE)
                        .cursor_pointer()
                        .hover(|d| d.text_color(theme::text()))
                        .child("Jump to latest ↓")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.follow = true;
                            cx.notify();
                        })),
                )
            });
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(transcript)
            .child(jump)
            .child(
                div()
                    .px(px(24.))
                    .pb(px(12.))
                    .child(self.render_prompt(window, cx)),
            )
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let section = |title: &str| {
            div()
                .font_weight(FontWeight::BOLD)
                .text_color(theme::text())
                .child(title.to_owned())
        };
        let mut column = div()
            .w(px(340.))
            .flex_none()
            .h_full()
            .bg(theme::raised())
            .px(px(20.))
            .py(px(16.))
            .flex()
            .flex_col()
            .gap(px(18.))
            .text_size(theme::SMALL_SIZE)
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_size(theme::TEXT_SIZE)
                    .child(self.session.title.clone()),
            );

        let tokens = self.session.context_tokens;
        let mut context_section = div()
            .flex()
            .flex_col()
            .text_color(theme::muted())
            .child(section("Context"))
            .child(format!("{} tokens", thousands(tokens)));
        if let Some(window) = self
            .ws
            .registry
            .caps(self.selection.model.as_deref().unwrap_or(""))
            .context
        {
            context_section = context_section.child(format!("{}% used", tokens * 100 / window));
        }
        let usage = self.session.usage;
        context_section = context_section.child(format!(
            "in {} · out {} · cached {}",
            thousands(usage.input_tokens + usage.cache_write_tokens),
            thousands(usage.output_tokens),
            thousands(usage.cache_read_tokens)
        ));
        // From the usage log: every request at its own model's price, subagents included.
        if self.session.cost > 0.0 {
            context_section = context_section.child(format!("${:.2} spent", self.session.cost));
        }
        column = column.child(context_section);

        let todos = self.todos.lock().unwrap().clone();
        if !todos.is_empty() {
            let remaining = todos
                .iter()
                .filter(|t| t.status == "pending" || t.status == "in_progress")
                .count();
            let mut list = div()
                .flex()
                .flex_col()
                .child(section(&format!("▼ Todo ({remaining} remaining)")));
            for todo in todos {
                let (mark, color) = match todo.status.as_str() {
                    "completed" => ("[✓]", theme::success()),
                    "in_progress" => ("[•]", theme::warning()),
                    "cancelled" => ("[✗]", theme::muted()),
                    _ => ("[ ]", theme::text_soft()),
                };
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .text_color(color)
                        .child(div().flex_none().child(mark))
                        .child(div().flex_1().min_w_0().child(todo.content)),
                );
            }
            column = column.child(list);
        }

        let mut instructions = div()
            .flex()
            .flex_col()
            .text_color(theme::muted())
            .child(section("Instructions"));
        if self.ws.instructions.is_empty() {
            instructions = instructions.child("No AGENTS.md found");
        }
        for instruction in &self.ws.instructions {
            let shown = crate::tools::display_path(&self.ws.cwd, &instruction.path);
            let shown = match dirs::home_dir() {
                Some(home)
                    if instruction.path.starts_with(&home)
                        && !instruction.path.starts_with(&self.ws.cwd) =>
                {
                    format!(
                        "~/{}",
                        instruction.path.strip_prefix(&home).unwrap().display()
                    )
                }
                _ => shown,
            };
            instructions = instructions.child(shown);
        }
        column = column.child(instructions);

        // Safety: the permission mode and the sandbox.
        let sandbox = crate::policy::Sandbox::resolve(&self.ws.config.permissions);
        let (sandbox_text, sandbox_color) = match (sandbox.wanted, sandbox.available) {
            (true, true) => (
                format!(
                    "on ({}) · network {}",
                    sandbox.backend(),
                    if sandbox.network {
                        "allowed"
                    } else {
                        "blocked"
                    }
                ),
                theme::success(),
            ),
            (true, false) if sandbox.forced => {
                ("required, but bwrap is missing".to_owned(), theme::error())
            }
            (true, false) => ("bwrap missing: commands ask".to_owned(), theme::warning()),
            (false, _) => ("off".to_owned(), theme::warning()),
        };
        let mode_color = match self.selection.mode {
            crate::policy::Mode::Normal => theme::text_soft(),
            crate::policy::Mode::AcceptEdits => theme::warning(),
            #[cfg(feature = "yolo")]
            crate::policy::Mode::Yolo => theme::error(),
        };
        if let Some(git) = &self.git {
            let mut line = div().flex().flex_row().gap(px(6.)).child(
                div()
                    .text_color(theme::text_soft())
                    .child(git.branch.clone()),
            );
            if git.ahead > 0 {
                line = line.child(
                    div()
                        .text_color(theme::success())
                        .child(format!("\u{2191}{}", git.ahead)),
                );
            }
            if git.behind > 0 {
                line = line.child(
                    div()
                        .text_color(theme::warning())
                        .child(format!("\u{2193}{}", git.behind)),
                );
            }
            let changed = git.changed();
            line = line.child(if changed == 0 {
                "clean".to_owned()
            } else {
                format!("{changed} changed")
            });
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .text_color(theme::muted())
                    .child(section("Git"))
                    .child(line),
            );
        }
        // Language servers this project uses: running, idle, failed, or one click to install.
        let servers = crate::lsp::relevant(&crate::context::project_root(&self.ws.cwd));
        if !servers.is_empty() && self.ws.config.lsp.unwrap_or(true) {
            let hub = crate::lsp::Hub::for_project(&self.ws.cwd, None);
            let mut list = div()
                .flex()
                .flex_col()
                .text_color(theme::muted())
                .child(section("Language servers"));
            for (i, (name, status)) in hub.statuses().into_iter().enumerate() {
                let (text, color) = match &status {
                    crate::lsp::Status::Running => ("running".to_owned(), theme::success()),
                    crate::lsp::Status::Idle => ("ready".to_owned(), theme::text_soft()),
                    crate::lsp::Status::Failed(why) => (format!("failed: {why}"), theme::error()),
                    crate::lsp::Status::NotInstalled => {
                        ("not installed".to_owned(), theme::warning())
                    }
                };
                let mut row = div()
                    .flex()
                    .flex_row()
                    .gap(px(6.))
                    .child(div().text_color(theme::text_soft()).child(name))
                    .child(div().text_color(color).truncate().child(text));
                let install = crate::lsp::SERVERS
                    .iter()
                    .find(|s| s.name == name)
                    .and_then(|s| s.install);
                if status == crate::lsp::Status::NotInstalled
                    && let Some(command) = install
                {
                    row = row.child(
                        div()
                            .id(("lsp-install", i))
                            .text_color(theme::secondary())
                            .cursor_pointer()
                            .hover(|d| d.text_color(theme::text()))
                            .child("install")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.offer_install(name.to_owned(), command.to_owned(), window, cx)
                            })),
                    );
                }
                list = list.child(row);
            }
            column = column.child(list);
        }
        column = column.child(
            div()
                .flex()
                .flex_col()
                .text_color(theme::muted())
                .child(section("Safety"))
                .child(
                    div().flex().flex_row().gap(px(6.)).child("mode").child(
                        div()
                            .text_color(mode_color)
                            .child(self.selection.mode.label()),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .child("sandbox")
                        .child(div().text_color(sandbox_color).child(sandbox_text)),
                ),
        );

        let servers = self.ws.mcp.snapshot();
        if !servers.is_empty() {
            let mut list = div()
                .flex()
                .flex_col()
                .text_color(theme::muted())
                .child(section("MCP"));
            for server in servers {
                let (dot, color, detail) = match &server.status {
                    crate::mcp::Status::Connected => {
                        ("●", theme::success(), format!("{} tools", server.tools))
                    }
                    crate::mcp::Status::Connecting => ("○", theme::warning(), "connecting".into()),
                    crate::mcp::Status::NeedsAuth => {
                        ("●", theme::warning(), "needs sign-in".into())
                    }
                    crate::mcp::Status::Failed(_) => ("●", theme::error(), "failed".into()),
                    crate::mcp::Status::Disabled => ("○", theme::muted(), "disabled".into()),
                };
                list = list.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .child(div().text_color(color).child(dot))
                        .child(div().text_color(theme::text_soft()).child(server.name))
                        .child(detail),
                );
            }
            column = column.child(list);
        }

        if !self.ws.skills.is_empty() {
            let mut skills = div()
                .flex()
                .flex_col()
                .text_color(theme::muted())
                .child(section(&format!("Skills ({})", self.ws.skills.len())));
            for skill in self.ws.skills.iter().take(10) {
                skills = skills.child(div().truncate().child(skill.name.clone()));
            }
            column = column.child(skills);
        }

        column
            .child(div().flex_1())
            .child(div().text_color(theme::muted()).child(self.location()))
            .into_any_element()
    }
}

impl Render for CrackView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_window_title(window);
        self.window_active = window.is_window_active();
        if self.focus_approval && self.approvals.contains_key(&self.tab_id) {
            window.focus(&self.approval_focus);
            self.focus_approval = false;
        }
        let sidebar = self.sidebar_visible(window);
        let main = if self.session.is_empty() {
            self.render_home(window, cx)
        } else {
            self.render_session(window, cx)
        };

        let mut root = div()
            .key_context("Workspace")
            .on_drop(
                cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                    this.on_drop_paths(paths, window, cx)
                }),
            )
            .relative()
            .size_full()
            .flex()
            .flex_row()
            .bg(theme::bg())
            .text_color(theme::text())
            .font_family(theme::font())
            .text_size(theme::TEXT_SIZE)
            .line_height(theme::LINE_HEIGHT)
            .on_action(
                cx.listener(|this, _: &Providers, window, cx| {
                    this.open_providers(None, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.new_tab(window, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                let id = this.tab_id;
                this.close_tab(id, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.cycle_tab(1, window, cx)))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| this.cycle_tab(-1, window, cx)))
            .on_action(cx.listener(|this, _: &SelectTab1, w, cx| this.select_tab(0, w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab2, w, cx| this.select_tab(1, w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab3, w, cx| this.select_tab(2, w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab4, w, cx| this.select_tab(3, w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab5, w, cx| this.select_tab(4, w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab6, w, cx| this.select_tab(5, w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab7, w, cx| this.select_tab(6, w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab8, w, cx| this.select_tab(7, w, cx)))
            .on_action(cx.listener(|this, _: &SelectTab9, w, cx| this.select_tab(8, w, cx)))
            .on_action(cx.listener(|this, _: &CommandPalette, window, cx| {
                this.open_picker(PickerKind::Command, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ModelPicker, window, cx| {
                this.open_picker(PickerKind::Model, window, cx)
            }))
            .on_action(cx.listener(|this, _: &EffortPicker, window, cx| {
                this.open_picker(PickerKind::Effort, window, cx)
            }))
            .on_action(cx.listener(|this, _: &AgentPicker, window, cx| {
                this.open_picker(PickerKind::Agent, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SessionPicker, window, cx| {
                this.open_picker(PickerKind::Session, window, cx)
            }))
            .on_action(cx.listener(|this, _: &SkillPicker, window, cx| {
                this.open_picker(PickerKind::Skill, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Help, window, cx| {
                this.open_picker(PickerKind::Help, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NewSession, _, cx| this.new_session(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleSidebar, window, cx| this.toggle_sidebar(window, cx)),
            )
            .on_action(cx.listener(|this, _: &CycleAgent, _, cx| this.cycle_agent(cx)))
            .on_action(cx.listener(|this, _: &CycleEffort, _, cx| this.cycle_effort(cx)))
            .on_action(cx.listener(|this, _: &CycleRecentModel, _, cx| this.cycle_recent_model(cx)))
            .on_action(cx.listener(|this, _: &Interrupt, _, cx| this.interrupt(cx)))
            .on_action(cx.listener(|this, _: &Undo, window, cx| this.undo(window, cx)))
            .on_action(cx.listener(|this, _: &Redo, _, cx| this.redo(cx)))
            .on_action(cx.listener(|this, _: &ShowDiff, _, cx| this.show_session_diff(cx)))
            .on_action(cx.listener(|this, _: &CopyLast, _, cx| this.copy_last(cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.scroll_by(1., cx)))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.scroll_by(-1., cx)))
            .on_action(cx.listener(|this, _: &ScrollTop, _, cx| this.scroll_by(1_000., cx)))
            .on_action(cx.listener(|this, _: &ScrollBottom, _, cx| {
                this.follow = true;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ClearOrQuit, _, cx| {
                if this.input.read(cx).text().is_empty() {
                    cx.quit();
                } else {
                    this.input.update(cx, |input, cx| input.set_text("", cx));
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.render_tab_bar(cx))
                    .child(div().flex_1().min_h_0().child(main)),
            );
        if sidebar {
            root = root.child(self.render_sidebar(cx));
        }

        if let Some((picker, _)) = &self.picker {
            root = root.child(
                div()
                    .id("backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .bg(theme::backdrop())
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(window.viewport_size().height / 6.)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.close_picker(window, cx)),
                    )
                    .child(
                        div()
                            .id("picker-frame")
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(picker.clone()),
                    ),
            );
        }

        if let Some(view) = &self.diff_view {
            root = root.child(self.render_diff_view(view, window, cx));
        }

        if let Some((dialog, _)) = &self.providers {
            root = root.child(
                div()
                    .id("providers-backdrop")
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .bg(theme::backdrop())
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .px(px(16.))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.close_providers(window, cx)),
                    )
                    .child(
                        div()
                            .id("providers-frame")
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(dialog.clone()),
                    ),
            );
        }

        if let Some((text, _, error)) = &self.toast {
            root = root.child(
                div()
                    .absolute()
                    .top(px(16.))
                    .right(px(16.))
                    .max_w(px(480.))
                    .bg(theme::raised_high())
                    .border_l_2()
                    .border_color(if *error {
                        theme::error()
                    } else {
                        theme::primary()
                    })
                    .px(px(14.))
                    .py(px(8.))
                    .shadow_lg()
                    .child(text.clone()),
            );
        }
        let _ = Agent::Build;
        root
    }
}

impl CrackView {
    /// The line above the prompt while a turn runs: what the model is doing, for how
    /// long, and how to stop it.
    fn render_status_line(&self) -> Option<AnyElement> {
        let running = self.running.as_ref()?;
        let model = self
            .selection
            .model
            .as_deref()
            .map(|m| m.split_once('/').map_or(m, |(_, name)| name))
            .unwrap_or("the model")
            .to_owned();
        let label = match self.session.entries.last() {
            Some(Entry::Thinking { done: false, .. }) => "Thinking".to_owned(),
            Some(Entry::Tool {
                state: ToolState::Pending | ToolState::Running,
                meta,
                ..
            }) => {
                let title: String = meta.title.chars().take(60).collect();
                format!("Running {title}")
            }
            Some(Entry::Text { .. }) if running.last_event.elapsed() < Duration::from_secs(2) => {
                "Writing".to_owned()
            }
            Some(Entry::Text { .. }) => format!("{model} is working"),
            _ => format!("Waiting for {model}"),
        };
        let started = running.started;
        let color = theme::agent_color(self.selection.agent);
        let armed = self.esc_armed.is_some();
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .pb(px(8.))
                .text_size(theme::SMALL_SIZE)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.))
                        .min_w_0()
                        .child(spinner("status-spinner", color))
                        .child(
                            div()
                                .id("status-label")
                                .min_w_0()
                                .truncate()
                                .text_color(theme::text_soft())
                                // Redrawn every frame by the animation, so the seconds count up.
                                .with_animation(
                                    "status-clock",
                                    Animation::new(Duration::from_secs(1)).repeat(),
                                    move |el, _| {
                                        el.child(format!(
                                            "{label}… {}s",
                                            started.elapsed().as_secs()
                                        ))
                                    },
                                ),
                        ),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(if armed {
                            theme::warning()
                        } else {
                            theme::muted()
                        })
                        .child(if armed {
                            "esc again to interrupt"
                        } else {
                            "esc interrupt"
                        }),
                )
                .into_any_element(),
        )
    }
}

impl CrackView {
    /// "2 MCP" with a color: green when all are up, amber while connecting, red if any failed.
    fn mcp_summary(&self) -> Option<(String, Hsla)> {
        let servers = self.ws.mcp.snapshot();
        if servers.is_empty() {
            return None;
        }
        let failed = servers
            .iter()
            .filter(|s| matches!(s.status, crate::mcp::Status::Failed(_)))
            .count();
        let connecting = servers
            .iter()
            .any(|s| s.status == crate::mcp::Status::Connecting);
        Some(if failed > 0 {
            (
                format!("{} MCP ({failed} failed)", servers.len()),
                theme::error(),
            )
        } else if connecting {
            (
                format!("{} MCP connecting", servers.len()),
                theme::warning(),
            )
        } else {
            (format!("{} MCP", servers.len()), theme::success())
        })
    }
}

impl CrackView {
    /// One tab per open session; click to switch, × or middle-click to close.
    fn render_tab_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let accent = theme::agent_color(self.selection.agent);
        let mut bar = div()
            .id("tab-bar")
            .flex()
            .flex_row()
            .items_end()
            .h(px(34.))
            .flex_none()
            .px(px(12.))
            .gap(px(2.))
            .bg(theme::bg())
            .border_b_1()
            .border_color(theme::border())
            .overflow_x_scroll()
            .text_size(theme::SMALL_SIZE);
        for (index, (id, title, running, active)) in self.tab_summaries().into_iter().enumerate() {
            let mut tab = div()
                .id(("tab", id as usize))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .h(px(30.))
                .max_w(px(240.))
                .pl(px(12.))
                .pr(px(6.))
                .rounded_t(px(4.))
                .cursor_pointer()
                .border_t_2();
            tab = if active {
                tab.bg(theme::raised())
                    .border_color(accent)
                    .text_color(theme::text())
            } else {
                tab.border_color(gpui::transparent_black())
                    .text_color(theme::muted())
                    .hover(|d| d.bg(theme::raised_high()).text_color(theme::text_soft()))
            };
            if self.approvals.contains_key(&id) {
                // This tab needs you.
                tab = tab.child(
                    div()
                        .text_color(theme::warning())
                        .font_weight(FontWeight::BOLD)
                        .child("!"),
                );
            } else if running {
                tab = tab.child(spinner(("tab-spin", id as usize), accent));
            } else if index < 9 {
                tab = tab.child(
                    div()
                        .text_color(theme::muted())
                        .child(format!("{}", index + 1)),
                );
            }
            tab = tab
                .child(div().flex_1().min_w_0().truncate().child(title))
                .child(
                    div()
                        .id(("tab-close", id as usize))
                        .px(px(4.))
                        .rounded(px(3.))
                        .text_color(theme::muted())
                        .hover(|d| d.bg(theme::raised_max()).text_color(theme::text()))
                        .child("×")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_tab(id, window, cx);
                        })),
                )
                .on_click(cx.listener(move |this, _, window, cx| this.switch_tab(id, window, cx)))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, _, window, cx| this.close_tab(id, window, cx)),
                );
            bar = bar.child(tab);
        }
        bar.child(
            div()
                .id("tab-new")
                .h(px(30.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded_t(px(4.))
                .text_color(theme::muted())
                .cursor_pointer()
                .hover(|d| d.bg(theme::raised_high()).text_color(theme::text()))
                .child("+")
                .on_click(cx.listener(|this, _, window, cx| this.new_tab(window, cx))),
        )
        .into_any_element()
    }
}

impl CrackView {
    /// The approval card above the prompt: what the model wants to do, why it asks,
    /// and the four answers.
    fn render_approval(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let request = self.approvals.get(&self.tab_id)?;
        let input = &request.input;
        let (heading, preview): (&str, Vec<(String, Hsla)>) = match request.tool.as_str() {
            "bash" => (
                "Run this command?",
                vec![(
                    format!("$ {}", input["command"].as_str().unwrap_or_default()),
                    theme::text(),
                )],
            ),
            "edit" => {
                let mut lines = vec![(
                    input["path"].as_str().unwrap_or_default().to_owned(),
                    theme::text_soft(),
                )];
                for line in input["old_string"]
                    .as_str()
                    .unwrap_or_default()
                    .lines()
                    .take(6)
                {
                    lines.push((format!("- {line}"), theme::diff_removed()));
                }
                for line in input["new_string"]
                    .as_str()
                    .unwrap_or_default()
                    .lines()
                    .take(6)
                {
                    lines.push((format!("+ {line}"), theme::diff_added()));
                }
                ("Make this edit?", lines)
            }
            "write" => {
                let mut lines = vec![(
                    input["path"].as_str().unwrap_or_default().to_owned(),
                    theme::text_soft(),
                )];
                for line in input["content"]
                    .as_str()
                    .unwrap_or_default()
                    .lines()
                    .take(8)
                {
                    lines.push((format!("+ {line}"), theme::diff_added()));
                }
                ("Write this file?", lines)
            }
            tool if tool.starts_with("mcp__") => (
                "Use this MCP tool?",
                vec![
                    (crate::tools::title(tool, input), theme::text()),
                    (
                        input.to_string().chars().take(300).collect(),
                        theme::muted(),
                    ),
                ],
            ),
            tool => (
                "Allow this?",
                vec![(crate::tools::title(tool, input), theme::text())],
            ),
        };
        let button = |id: &'static str, key: &'static str, label: String, primary: bool| {
            div()
                .id(id)
                .flex()
                .flex_row()
                .gap(px(8.))
                .px(px(12.))
                .py(px(4.))
                .rounded(px(4.))
                .border_1()
                .cursor_pointer()
                .when(primary, |d| {
                    d.bg(theme::chip())
                        .border_color(theme::chip())
                        .text_color(theme::on_chip())
                })
                .when(!primary, |d| {
                    d.border_color(theme::selected_bg())
                        .text_color(theme::text())
                })
                .hover(|d| d.bg(theme::selected_bg()))
                .child(div().font_weight(FontWeight::BOLD).child(key))
                .child(label)
        };
        let mut code = div()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .rounded(px(4.))
            .px(px(12.))
            .py(px(8.))
            .text_size(theme::SMALL_SIZE);
        for (line, color) in preview {
            code = code.child(div().text_color(color).child(line));
        }
        let rule = request.suggestion.clone();
        Some(
            div()
                .id("approval")
                .key_context("Approval")
                .track_focus(&self.approval_focus)
                .on_action(cx.listener(|this, _: &ApproveOnce, window, cx| {
                    this.answer_approval(super::view::ApprovalChoice::Once, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ApproveSession, window, cx| {
                    this.answer_approval(super::view::ApprovalChoice::Session, window, cx)
                }))
                .on_action(cx.listener(|this, _: &ApproveAlways, window, cx| {
                    this.answer_approval(super::view::ApprovalChoice::Always, window, cx)
                }))
                .on_action(cx.listener(|this, _: &DenyApproval, window, cx| {
                    this.answer_approval(super::view::ApprovalChoice::Deny, window, cx)
                }))
                .mb(px(8.))
                .p(px(14.))
                .flex()
                .flex_col()
                .gap(px(10.))
                .bg(theme::raised())
                .border_1()
                .rounded(px(6.))
                .border_color(theme::warning())
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(10.))
                        .child(
                            div()
                                .text_color(theme::warning())
                                .font_weight(FontWeight::BOLD)
                                .child(heading),
                        )
                        .child(
                            div()
                                .text_color(theme::muted())
                                .child(format!("({})", request.reason)),
                        ),
                )
                .child(code)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(8.))
                        .child(
                            button("approve-once", "y", "Allow once".into(), true).on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.answer_approval(
                                        super::view::ApprovalChoice::Once,
                                        window,
                                        cx,
                                    )
                                }),
                            ),
                        )
                        .when(!rule.is_empty(), |row| {
                            row.child(
                                button(
                                    "approve-session",
                                    "a",
                                    format!("Allow `{rule}` this session"),
                                    false,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.answer_approval(
                                            super::view::ApprovalChoice::Session,
                                            window,
                                            cx,
                                        )
                                    },
                                )),
                            )
                            .child(
                                button("approve-always", "A", "Always allow".into(), false)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.answer_approval(
                                            super::view::ApprovalChoice::Always,
                                            window,
                                            cx,
                                        )
                                    })),
                            )
                        })
                        .child(
                            button("deny", "n", "Deny".into(), false).on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.answer_approval(
                                        super::view::ApprovalChoice::Deny,
                                        window,
                                        cx,
                                    )
                                },
                            )),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl CrackView {
    /// The `/diff` viewer: a scrollable, colored unified diff over the transcript.
    fn render_diff_view(
        &self,
        view: &super::undo::DiffView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut body = div().flex().flex_col().font_family(theme::font());
        for (i, line) in view.lines.iter().enumerate() {
            let text = if line.is_empty() {
                SharedString::from(" ")
            } else {
                line.clone()
            };
            let el = div().id(("diff-line", i)).px(px(12.)).child(text);
            let el = if line.starts_with("+++")
                || line.starts_with("---")
                || line.starts_with("diff --git")
            {
                el.text_color(theme::text()).font_weight(FontWeight::BOLD)
            } else if line.starts_with('+') {
                el.bg(theme::diff_added_bg())
                    .text_color(theme::diff_added())
            } else if line.starts_with('-') {
                el.bg(theme::diff_removed_bg())
                    .text_color(theme::diff_removed())
            } else if line.starts_with("@@") {
                el.text_color(theme::diff_context())
            } else {
                el.text_color(theme::text_soft())
            };
            body = body.child(el);
        }
        let size = window.viewport_size();
        div()
            .id("diff-backdrop")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(theme::backdrop())
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.diff_view = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("diff-frame")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .w(size.width * 0.85)
                    .h(size.height * 0.8)
                    .bg(theme::raised())
                    .border_1()
                    .border_color(theme::border())
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .px(px(16.))
                            .py(px(10.))
                            .child(
                                div()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme::text())
                                    .child(view.title.clone()),
                            )
                            .child(div().text_color(theme::muted()).child("esc")),
                    )
                    .child(
                        div()
                            .id("diff-scroll")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&view.scroll)
                            .text_size(theme::SMALL_SIZE)
                            .pb(px(12.))
                            .child(body),
                    ),
            )
    }
}
