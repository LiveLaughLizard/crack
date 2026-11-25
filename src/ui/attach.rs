//! Images attached to the next message: pasted screenshots and dropped image files.
//! Dropping any other file adds an `@path` mention instead.

use base64::Engine;
use gpui::*;

use super::theme;
use super::view::CrackView;

/// Providers reject images over about 5 MB once encoded.
const MAX_BYTES: usize = 3_750_000;

#[derive(Clone)]
pub struct Attachment {
    pub name: String,
    pub media_type: String,
    pub data: String,
}

fn media_type(format: ImageFormat) -> Option<&'static str> {
    Some(match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Webp => "image/webp",
        ImageFormat::Gif => "image/gif",
        _ => return None,
    })
}

fn media_type_for_path(path: &std::path::Path) -> Option<&'static str> {
    Some(
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => return None,
        },
    )
}

impl CrackView {
    fn add_attachment(
        &mut self,
        name: String,
        media_type: &str,
        bytes: &[u8],
        cx: &mut Context<Self>,
    ) {
        if bytes.len() > MAX_BYTES {
            self.show_toast(
                format!(
                    "{name} is too big to send ({} MB; the limit is about 3.7 MB).",
                    bytes.len() / 1_000_000
                ),
                true,
                cx,
            );
            return;
        }
        self.attachments.push(Attachment {
            name,
            media_type: media_type.to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
        cx.notify();
    }

    /// A screenshot pasted into the prompt.
    pub(super) fn attach_image(&mut self, image: Image, cx: &mut Context<Self>) {
        let Some(media_type) = media_type(image.format) else {
            self.show_toast(
                "That image format can't be sent; paste a PNG, JPEG, WebP or GIF.",
                true,
                cx,
            );
            return;
        };
        let name = format!(
            "pasted-{}.{}",
            self.attachments.len() + 1,
            media_type.trim_start_matches("image/")
        );
        self.add_attachment(name, media_type, &image.bytes, cx);
    }

    /// Files dropped on the window: images attach, anything else becomes `@path`.
    pub(super) fn on_drop_paths(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = crate::context::project_root(&self.ws.cwd);
        let mut mentions = Vec::new();
        for path in paths.paths() {
            if let Some(media_type) = media_type_for_path(path) {
                match std::fs::read(path) {
                    Ok(bytes) => {
                        let name = path
                            .file_name()
                            .map_or("image".into(), |n| n.to_string_lossy().into_owned());
                        self.add_attachment(name, media_type, &bytes, cx);
                    }
                    Err(error) => self.show_toast(
                        format!("Couldn't read {}: {error}", path.display()),
                        true,
                        cx,
                    ),
                }
            } else {
                let shown = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .display()
                    .to_string();
                mentions.push(format!("@{shown}"));
            }
        }
        if !mentions.is_empty() {
            let mut text = self.input.read(cx).text().to_owned();
            if !text.is_empty() && !text.ends_with(' ') {
                text.push(' ');
            }
            text.push_str(&mentions.join(" "));
            text.push(' ');
            self.input.update(cx, |input, cx| input.set_text(text, cx));
        }
        self.focus_prompt(window, cx);
    }

    /// The pending images, as chips above the prompt (click × to remove).
    pub(super) fn render_attachments(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.attachments.is_empty() {
            return None;
        }
        let mut row = div().flex().flex_row().flex_wrap().gap(px(6.)).pb(px(6.));
        for (i, attachment) in self.attachments.iter().enumerate() {
            row = row.child(
                div()
                    .id(("attachment", i))
                    .flex()
                    .flex_row()
                    .gap(px(6.))
                    .px(px(8.))
                    .bg(theme::raised_high())
                    .text_size(theme::SMALL_SIZE)
                    .text_color(theme::text_soft())
                    .child(format!("\u{25a3} {}", attachment.name))
                    .child(
                        div()
                            .id(("attachment-remove", i))
                            .text_color(theme::muted())
                            .cursor_pointer()
                            .hover(|d| d.text_color(theme::error()))
                            .child("\u{d7}")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if i < this.attachments.len() {
                                    this.attachments.remove(i);
                                }
                                cx.notify();
                            })),
                    ),
            );
        }
        Some(row.into_any_element())
    }
}
