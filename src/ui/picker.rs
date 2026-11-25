//! A searchable, grouped list in a centered dialog: models, sessions, commands...

use super::input::{InputEvent, TextInput};
use super::theme;
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyBinding, MouseButton, ParentElement, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Window, actions, div, px,
};

actions!(picker, [Dismiss]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("escape", Dismiss, Some("Picker"))]);
}

#[derive(Clone, Debug, Default)]
pub struct PickerItem {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub category: Option<String>,
    /// Right-aligned, e.g. a keybinding.
    pub footer: Option<String>,
    pub current: bool,
    pub disabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Effort,
    Agent,
    Session,
    Skill,
    Command,
    Help,
    Mcp,
}

pub enum PickerEvent {
    Confirm(PickerKind, String),
    Dismiss,
}

pub struct Picker {
    pub kind: PickerKind,
    title: SharedString,
    items: Vec<PickerItem>,
    filtered: Vec<usize>,
    selected: usize,
    search: Entity<TextInput>,
    scroll: ScrollHandle,
    wide: bool,
    _subscription: gpui::Subscription,
}

impl EventEmitter<PickerEvent> for Picker {}

/// Subsequence match; higher is better. `None` when the query doesn't match.
fn fuzzy(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let text = text.to_lowercase();
    if let Some(pos) = text.find(query) {
        return Some(1000 - pos as i32);
    }
    let mut score = 0;
    let mut last: Option<usize> = None;
    let mut chars = text.char_indices();
    for q in query.chars() {
        let (i, _) = chars.find(|(_, c)| *c == q)?;
        score += match last {
            Some(l) if i == l + 1 => 10,
            _ => 1,
        };
        last = Some(i);
    }
    Some(score)
}

impl Picker {
    pub fn new(
        kind: PickerKind,
        title: impl Into<SharedString>,
        items: Vec<PickerItem>,
        wide: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| TextInput::new(false, "Search", cx));
        let subscription =
            cx.subscribe(
                &search,
                |this: &mut Self, _, event: &InputEvent, cx| match event {
                    InputEvent::Changed => this.refilter(cx),
                    InputEvent::Up => this.move_selection(-1, cx),
                    InputEvent::Down => this.move_selection(1, cx),
                    InputEvent::Submit => this.confirm(cx),
                },
            );
        window.focus(&search.focus_handle(cx));
        let selected = items.iter().position(|i| i.current).unwrap_or(0);
        let mut picker = Self {
            kind,
            title: title.into(),
            filtered: (0..items.len()).collect(),
            items,
            selected: 0,
            search,
            scroll: ScrollHandle::new(),
            wide,
            _subscription: subscription,
        };
        picker.selected = picker
            .filtered
            .iter()
            .position(|i| *i == selected)
            .unwrap_or(0);
        picker.skip_disabled(1);
        picker.scroll.scroll_to_item(picker.row_of_selected());
        picker
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).text().trim().to_lowercase();
        let mut scored: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let haystack = format!(
                    "{} {} {}",
                    item.title,
                    item.category.as_deref().unwrap_or(""),
                    item.description.as_deref().unwrap_or("")
                );
                fuzzy(&query, &haystack).map(|s| (s, i))
            })
            .collect();
        if !query.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        self.filtered = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
        self.skip_disabled(1);
        self.scroll.scroll_to_item(0);
        cx.notify();
    }

    fn skip_disabled(&mut self, direction: isize) {
        let len = self.filtered.len();
        for _ in 0..len {
            if !self.items[self.filtered[self.selected]].disabled {
                return;
            }
            self.selected = (self.selected as isize + direction).rem_euclid(len as isize) as usize;
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
        self.skip_disabled(delta.signum());
        self.scroll.scroll_to_item(self.row_of_selected());
        cx.notify();
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.filtered.get(self.selected) {
            let item = &self.items[*index];
            if !item.disabled {
                cx.emit(PickerEvent::Confirm(self.kind, item.id.clone()));
            }
        }
    }

    fn grouped(&self, cx: &App) -> bool {
        self.search.read(cx).text().trim().is_empty()
    }

    /// The child index of the selected row, counting category headers.
    fn row_of_selected(&self) -> usize {
        let mut row = 0;
        let mut last_category: Option<&str> = None;
        for (n, index) in self.filtered.iter().enumerate() {
            let category = self.items[*index].category.as_deref();
            if category.is_some() && category != last_category {
                row += 1;
                last_category = category;
            }
            if n == self.selected {
                return row;
            }
            row += 1;
        }
        row
    }
}

impl Focusable for Picker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.focus_handle(cx)
    }
}

impl Render for Picker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let grouped = self.grouped(cx);
        let mut list = div()
            .id("picker-list")
            .flex()
            .flex_col()
            .max_h(px(440.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        let mut last_category: Option<String> = None;
        for (n, index) in self.filtered.iter().enumerate() {
            let item = &self.items[*index];
            if grouped && item.category.is_some() && item.category != last_category {
                last_category = item.category.clone();
                list = list.child(
                    div()
                        .px(px(20.))
                        .pt(px(10.))
                        .pb(px(2.))
                        .text_color(theme::muted())
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(item.category.clone().unwrap_or_default()),
                );
            }
            let selected = n == self.selected;
            let mut title = item.title.clone();
            if !grouped && let Some(category) = &item.category {
                title = format!("{title}  {category}");
            }
            let row = div()
                .id(("picker-row", n))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .pr(px(20.))
                .py(px(3.))
                .when_selected(selected)
                .child(
                    div()
                        .w(px(3.))
                        .h(px(20.))
                        .when(selected, |d| d.bg(theme::primary())),
                )
                .child(
                    div()
                        .w(px(10.))
                        .text_color(theme::primary())
                        .child(if item.current { "▪" } else { "" }),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(if item.disabled {
                            theme::muted()
                        } else {
                            theme::text()
                        })
                        .child(title),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme::muted())
                        .child(item.description.clone().unwrap_or_default()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(theme::muted())
                        .child(item.footer.clone().unwrap_or_default()),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.selected = n;
                        this.confirm(cx);
                    }),
                );
            list = list.child(row);
        }
        if self.filtered.is_empty() {
            list = list.child(
                div()
                    .px(px(24.))
                    .py(px(8.))
                    .text_color(theme::muted())
                    .child("No results"),
            );
        }

        div()
            .key_context("Picker")
            .on_action(cx.listener(|_, _: &Dismiss, _, cx| cx.emit(PickerEvent::Dismiss)))
            .w(px(if self.wide { 860. } else { 620. }))
            .max_w_full()
            .bg(theme::raised())
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
                    .px(px(24.))
                    .pt(px(16.))
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(self.title.clone()),
                    )
                    .child(div().text_color(theme::muted()).child("esc")),
            )
            .child(div().px(px(24.)).py(px(12.)).child(self.search.clone()))
            .child(list)
            .child(
                div()
                    .mt(px(8.))
                    .px(px(24.))
                    .py(px(8.))
                    .bg(theme::raised_high())
                    .flex()
                    .flex_row()
                    .gap(px(16.))
                    .text_color(theme::text_soft())
                    .child("↑↓ select")
                    .child("enter confirm")
                    .child("esc close"),
            )
    }
}

trait WhenSelected {
    fn when_selected(self, selected: bool) -> Self;
}

impl<T: Styled> WhenSelected for T {
    fn when_selected(self, selected: bool) -> Self {
        if selected {
            self.bg(theme::selected_bg())
        } else {
            self
        }
    }
}

use gpui::prelude::FluentBuilder as _;
