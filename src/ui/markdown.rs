//! Markdown for assistant messages, drawn with the theme's markdown colors.

use super::theme;
use gpui::{
    AnyElement, FontStyle, FontWeight, HighlightStyle, Hsla, IntoElement, ParentElement,
    SharedString, StrikethroughStyle, Styled, StyledText, div, px,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::ops::Range;

#[derive(Clone, Debug)]
pub enum Block {
    Text {
        text: String,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
        heading: Option<HeadingLevel>,
        quote: bool,
        indent: usize,
        prefix: Option<String>,
    },
    Code {
        text: String,
    },
    Rule,
}

#[derive(Default)]
struct Inline {
    text: String,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
}

impl Inline {
    fn push(&mut self, text: &str, style: Option<HighlightStyle>) {
        let start = self.text.len();
        self.text.push_str(text);
        if let Some(style) = style
            && !text.is_empty()
        {
            self.highlights.push((start..self.text.len(), style));
        }
    }
}

pub fn parse(source: &str) -> Vec<Block> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut blocks = Vec::new();
    let mut inline = Inline::default();
    let mut bold = 0;
    let mut italic = 0;
    let mut strike = 0;
    let mut link = 0;
    let mut heading: Option<HeadingLevel> = None;
    let mut quote = 0;
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut pending_prefix: Option<String> = None;
    let mut code: Option<String> = None;

    let style = |bold: i32, italic: i32, strike: i32, link: i32| -> Option<HighlightStyle> {
        if bold == 0 && italic == 0 && strike == 0 && link == 0 {
            return None;
        }
        let mut s = HighlightStyle::default();
        if bold > 0 {
            s.font_weight = Some(FontWeight::BOLD);
            s.color = Some(theme::md_strong());
        }
        if italic > 0 {
            s.font_style = Some(FontStyle::Italic);
            s.color = Some(theme::md_emphasis());
        }
        if strike > 0 {
            s.strikethrough = Some(StrikethroughStyle {
                thickness: px(1.),
                color: None,
            });
        }
        if link > 0 {
            s.color = Some(theme::md_link());
        }
        Some(s)
    };

    macro_rules! flush {
        () => {{
            let taken = std::mem::take(&mut inline);
            if !taken.text.trim().is_empty() || pending_prefix.is_some() {
                blocks.push(Block::Text {
                    text: taken.text.trim_end().to_owned(),
                    highlights: taken.highlights,
                    heading,
                    quote: quote > 0,
                    indent: lists.len().saturating_sub(1),
                    prefix: pending_prefix.take(),
                });
            }
        }};
    }

    for event in Parser::new_ext(source, options) {
        if let Some(buffer) = code.as_mut() {
            match event {
                Event::Text(text) => buffer.push_str(&text),
                Event::End(TagEnd::CodeBlock) => {
                    let text = code.take().unwrap();
                    blocks.push(Block::Code {
                        text: text.trim_end_matches('\n').to_owned(),
                    });
                }
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    flush!();
                    heading = Some(level);
                }
                Tag::BlockQuote(_) => {
                    flush!();
                    quote += 1;
                }
                Tag::CodeBlock(kind) => {
                    flush!();
                    let _ = matches!(kind, CodeBlockKind::Fenced(_));
                    code = Some(String::new());
                }
                Tag::List(start) => {
                    flush!();
                    lists.push(start);
                }
                Tag::Item => {
                    flush!();
                    let marker = match lists.last_mut() {
                        Some(Some(n)) => {
                            let marker = format!("{n}.");
                            *n += 1;
                            marker
                        }
                        _ => "•".to_owned(),
                    };
                    pending_prefix = Some(marker);
                }
                Tag::Emphasis => italic += 1,
                Tag::Strong => bold += 1,
                Tag::Strikethrough => strike += 1,
                Tag::Link { .. } => link += 1,
                Tag::TableCell => {}
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => flush!(),
                TagEnd::Heading(_) => {
                    flush!();
                    heading = None;
                }
                TagEnd::BlockQuote(_) => {
                    flush!();
                    quote -= 1;
                }
                TagEnd::List(_) => {
                    flush!();
                    lists.pop();
                }
                TagEnd::Item => flush!(),
                TagEnd::Emphasis => italic -= 1,
                TagEnd::Strong => bold -= 1,
                TagEnd::Strikethrough => strike -= 1,
                TagEnd::Link => link -= 1,
                TagEnd::TableCell => inline.push(
                    "  │  ",
                    Some(HighlightStyle {
                        color: Some(theme::muted()),
                        ..Default::default()
                    }),
                ),
                TagEnd::TableHead | TagEnd::TableRow => {
                    let trimmed = inline.text.trim_end_matches("  │  ").len();
                    inline.text.truncate(trimmed);
                    inline.highlights.retain(|(r, _)| r.end <= trimmed);
                    flush!();
                }
                _ => {}
            },
            Event::Text(text) => inline.push(&text, style(bold, italic, strike, link)),
            Event::Code(text) => inline.push(
                &text,
                Some(HighlightStyle {
                    color: Some(theme::md_code()),
                    ..Default::default()
                }),
            ),
            Event::Html(text) | Event::InlineHtml(text) => inline.push(&text, None),
            Event::SoftBreak => inline.push(" ", None),
            Event::HardBreak => inline.push("\n", None),
            Event::Rule => {
                flush!();
                blocks.push(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                pending_prefix = Some(if done { "[✓]".into() } else { "[ ]".into() });
            }
            _ => {}
        }
    }
    if let Some(text) = code.take() {
        // An unterminated fence while streaming.
        blocks.push(Block::Code { text });
    }
    flush!();
    blocks
}

pub fn render(blocks: &[Block], color: Hsla) -> AnyElement {
    let mut column = div().flex().flex_col().gap(px(8.)).min_w_0();
    for block in blocks {
        column = column.child(match block {
            Block::Text {
                text,
                highlights,
                heading,
                quote,
                indent,
                prefix,
            } => {
                let styled = StyledText::new(SharedString::from(text.clone()))
                    .with_highlights(highlights.clone());
                let mut row = div()
                    .flex()
                    .flex_row()
                    .min_w_0()
                    .pl(px(*indent as f32 * 20.));
                if let Some(prefix) = prefix {
                    row = row.child(
                        div()
                            .flex_none()
                            .min_w(px(22.))
                            .pr(px(6.))
                            .text_color(theme::md_list())
                            .child(prefix.clone()),
                    );
                }
                let mut body = div().flex_1().min_w_0().text_color(color);
                if heading.is_some() {
                    body = body
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme::md_heading());
                }
                if *quote {
                    body = body
                        .border_l_2()
                        .border_color(theme::border())
                        .pl(px(10.))
                        .text_color(theme::md_emphasis());
                }
                row.child(body.child(styled)).into_any_element()
            }
            Block::Code { text } => div()
                .bg(theme::raised())
                .rounded(px(4.))
                .px(px(12.))
                .py(px(8.))
                .text_color(theme::md_text())
                .overflow_hidden()
                .child(SharedString::from(text.clone()))
                .into_any_element(),
            Block::Rule => div().h(px(1.)).bg(theme::border()).into_any_element(),
        });
    }
    column.into_any_element()
}
