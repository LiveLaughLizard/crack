//! Exporting a session to share: Markdown (tool calls folded into `<details>`) or a
//! self-contained HTML page (theme colors, highlighted code, images inlined). Text that
//! looks like a secret is redacted, and output from reading secret files is dropped.

use crate::providers::Part;
use crate::session::{Entry, Session};
use crate::tools::DisplayKind;

/// What a secret looks like: key prefixes, private keys, `password=`-style values and
/// bearer tokens.
fn secret_patterns() -> Vec<(regex::Regex, &'static str)> {
    [
        (
            r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----",
            "[redacted private key]",
        ),
        (
            r"\b(?:sk-|sk_live_|AKIA|ASIA|ghp_|gho_|github_pat_|xoxb-|xoxp-|AIza|glpat-)[A-Za-z0-9_\-]{12,}",
            "[redacted]",
        ),
        (
            r"(?i)\b(password|passwd|secret|token|api[_-]?key|access[_-]?key)(\s*[:=]\s*[\x22']?)[^\s\x22',;]{6,}",
            "$1$2[redacted]",
        ),
        (r"(?i)\bbearer\s+[A-Za-z0-9._~+/=\-]{16,}", "Bearer [redacted]"),
    ]
    .into_iter()
    .map(|(p, r)| (regex::Regex::new(p).expect("valid pattern"), r))
    .collect()
}

/// Redacts secret-looking text; returns the text and how many were found.
pub fn redact(text: &str) -> (String, usize) {
    let mut out = text.to_owned();
    let mut count = 0;
    for (pattern, replacement) in secret_patterns() {
        count += pattern.find_iter(&out).count();
        out = pattern.replace_all(&out, replacement).into_owned();
    }
    (out, count)
}

struct Writer {
    out: String,
    redacted: usize,
}

impl Writer {
    fn text(&mut self, text: &str) {
        let (clean, n) = redact(text);
        self.redacted += n;
        self.out.push_str(&clean);
    }

    fn raw(&mut self, text: &str) {
        self.out.push_str(text);
    }
}

/// The longest run of backticks in `text`, so a fence can be longer.
fn fence(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

fn markdown_into(session: &Session, w: &mut Writer, depth: usize, images: &mut Vec<String>) {
    let mut image_parts = session
        .messages
        .iter()
        .flat_map(|m| &m.parts)
        .filter_map(|p| match p {
            Part::Image { media_type, data } => Some(format!("data:{media_type};base64,{data}")),
            _ => None,
        });
    let cwd = session.cwd.clone();
    for entry in &session.entries {
        match entry {
            Entry::User {
                text,
                images: names,
                ..
            } => {
                w.raw("\n## You\n\n");
                w.text(text.trim());
                w.raw("\n");
                for name in names {
                    match image_parts.next() {
                        Some(uri) => {
                            images.push(uri);
                            w.raw(&format!("\n![{name}](crack-image:{})\n", images.len() - 1));
                        }
                        None => w.raw(&format!("\n_[image: {name}]_\n")),
                    }
                }
            }
            Entry::Text { text } => {
                w.raw("\n");
                w.text(text.trim());
                w.raw("\n");
            }
            Entry::Thinking { text, .. } if !text.trim().is_empty() => {
                w.raw("\n<details><summary>Thinking</summary>\n\n");
                w.text(text.trim());
                w.raw("\n\n</details>\n");
            }
            Entry::Thinking { .. } => {}
            Entry::Tool {
                name, input, meta, ..
            } => {
                let title = if meta.title.is_empty() {
                    crate::tools::title(name, input)
                } else {
                    meta.title.clone()
                };
                // Output from reading a secret file never leaves the machine.
                let secret = input
                    .get("path")
                    .and_then(|p| p.as_str())
                    .is_some_and(|p| crate::policy::is_secret_path(&cwd.join(p)));
                w.raw("\n<details><summary>");
                w.text(&html_escape(&title));
                w.raw("</summary>\n\n");
                if secret {
                    w.raw("_[output from a secret file left out]_\n");
                    w.redacted += 1;
                } else if let Some(detail) = meta.detail.as_deref().filter(|d| !d.trim().is_empty())
                {
                    let lang = match meta.kind {
                        DisplayKind::Diff => "diff",
                        _ => "",
                    };
                    let detail = crate::tools::truncate_middle(detail, 20_000);
                    let f = fence(&detail);
                    w.raw(&format!("{f}{lang}\n"));
                    w.text(detail.trim_end());
                    w.raw(&format!("\n{f}\n"));
                } else if !input.is_null() {
                    let args = serde_json::to_string_pretty(input).unwrap_or_default();
                    let args = crate::tools::truncate_middle(&args, 4_000);
                    let f = fence(&args);
                    w.raw(&format!("{f}json\n"));
                    w.text(&args);
                    w.raw(&format!("\n{f}\n"));
                }
                // A subagent's own transcript, nested.
                if let Some(link) = &meta.link
                    && depth < 2
                    && let Ok(sub) = Session::load(link)
                {
                    w.raw("\n<blockquote>\n\n");
                    markdown_into(&sub, w, depth + 1, images);
                    w.raw("\n</blockquote>\n");
                }
                w.raw("\n</details>\n");
            }
            Entry::Notice { text } => {
                w.raw("\n> ");
                w.text(text.trim());
                w.raw("\n");
            }
            Entry::Error { text } => {
                w.raw("\n> **Error:** ");
                w.text(text.trim());
                w.raw("\n");
            }
            Entry::Footer { .. } => {}
        }
    }
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Markdown and the images it refers to (`crack-image:N`), plus the redaction count.
fn render(session: &Session) -> (String, Vec<String>, usize) {
    let mut w = Writer {
        out: String::new(),
        redacted: 0,
    };
    let mut images = Vec::new();
    w.raw(&format!("# {}\n\n", session.title.trim()));
    let date = chrono::DateTime::from_timestamp(session.created, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default();
    w.raw(&format!(
        "_A crack session \u{b7} {date} \u{b7} {}_\n",
        session
            .cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    markdown_into(session, &mut w, 0, &mut images);
    (w.out, images, w.redacted)
}

/// The session as Markdown. Images are left as notes.
pub fn markdown(session: &Session) -> (String, usize) {
    let (text, _, redacted) = render(session);
    let image = regex::Regex::new(r"!\[([^\]]*)\]\(crack-image:\d+\)").expect("valid");
    (
        image.replace_all(&text, "_[image: $1]_").into_owned(),
        redacted,
    )
}

/// The session as one self-contained HTML page.
pub fn html(session: &Session) -> (String, usize) {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
    let (text, images, redacted) = render(session);
    let mut events = Vec::new();
    let mut code: Option<(String, String)> = None;
    for event in Parser::new_ext(
        &text,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    ) {
        match (&mut code, event) {
            (None, Event::Start(Tag::CodeBlock(kind))) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((lang, String::new()));
            }
            (Some((_, body)), Event::Text(t)) => body.push_str(&t),
            (Some(_), Event::End(TagEnd::CodeBlock)) => {
                let (lang, body) = code.take().unwrap();
                events.push(Event::Html(highlighted(&lang, &body).into()));
            }
            (Some(_), _) => {}
            (
                None,
                Event::Start(Tag::Image {
                    dest_url,
                    title,
                    id,
                    link_type,
                }),
            ) => {
                let dest = dest_url
                    .strip_prefix("crack-image:")
                    .and_then(|n| n.parse::<usize>().ok())
                    .and_then(|n| images.get(n).cloned())
                    .map(Into::into)
                    .unwrap_or(dest_url);
                events.push(Event::Start(Tag::Image {
                    dest_url: dest,
                    title,
                    id,
                    link_type,
                }));
            }
            (None, other) => events.push(other),
        }
    }
    let mut body = String::new();
    pulldown_cmark::html::push_html(&mut body, events.into_iter());
    let palette = |light: bool| {
        if light {
            (
                "#CFC8E4", "#D8D2EA", "#2D2242", "#6D58AF", "#4F3795", "#24753A", "#B51A17",
                "#996100", "#1F5F99",
            )
        } else {
            (
                "#15121E", "#1F182C", "#E8E6F2", "#A29EC0", "#C4A9FF", "#3DDC84", "#FF6B6B",
                "#FFCF8E", "#9ED3FF",
            )
        }
    };
    let vars = |p: (&str, &str, &str, &str, &str, &str, &str, &str, &str)| {
        format!(
            "--bg:{};--raised:{};--text:{};--muted:{};--primary:{};--added:{};--removed:{};--type:{};--fn:{};",
            p.0, p.1, p.2, p.3, p.4, p.5, p.6, p.7, p.8
        )
    };
    let page = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
:root{{{dark}color-scheme:dark}}
@media (prefers-color-scheme: light){{:root:not([data-theme="dark"]){{{light}color-scheme:light}}}}
:root[data-theme="light"]{{{light}color-scheme:light}}
body{{margin:0;background:var(--bg);color:var(--text);font:15px/1.55 ui-sans-serif,system-ui,sans-serif}}
main{{max-width:860px;margin:0 auto;padding:24px 16px 64px}}
h1{{color:var(--primary);font-size:1.5em}} h2{{color:var(--primary);font-size:1.05em;margin-top:2em;border-top:1px solid var(--raised);padding-top:1em}}
em{{color:var(--muted)}} a{{color:var(--fn)}}
pre{{background:var(--raised);padding:10px 12px;border-radius:6px;overflow-x:auto;font:13px/1.45 ui-monospace,"JetBrains Mono",monospace}}
code{{font-family:ui-monospace,"JetBrains Mono",monospace}}
details{{background:var(--raised);border-radius:6px;padding:6px 10px;margin:8px 0}} details pre{{background:var(--bg)}}
summary{{cursor:pointer;color:var(--muted);font-family:ui-monospace,monospace;font-size:13px}}
blockquote{{border-left:2px solid var(--muted);margin:8px 0;padding-left:12px;color:var(--muted)}}
img{{max-width:100%;border-radius:6px}}
.k{{color:var(--primary)}}.s{{color:var(--added)}}.c{{color:var(--muted);font-style:italic}}.f{{color:var(--fn)}}.t{{color:var(--type)}}.n{{color:var(--type)}}
.add{{color:var(--added)}}.del{{color:var(--removed)}}
button{{position:fixed;top:12px;right:12px;background:var(--raised);color:var(--text);border:1px solid var(--muted);border-radius:6px;padding:4px 10px;cursor:pointer}}
</style></head>
<body><button onclick="var r=document.documentElement;r.dataset.theme=(r.dataset.theme==='light'||(!r.dataset.theme&&matchMedia('(prefers-color-scheme: light)').matches))?'dark':'light'">theme</button>
<main>
{body}
</main></body></html>
"#,
        title = html_escape(&session.title),
        dark = vars(palette(false)),
        light = vars(palette(true)),
    );
    (page, redacted)
}

/// A code block as HTML, highlighted with the bundled grammars (diffs colored by line).
fn highlighted(lang: &str, code: &str) -> String {
    use crate::highlight::Kind;
    if lang == "diff" {
        let lines: Vec<String> = code
            .lines()
            .map(|l| {
                let class = match l.chars().next() {
                    Some('+') => " class=\"add\"",
                    Some('-') => " class=\"del\"",
                    _ => "",
                };
                format!("<span{class}>{}</span>", html_escape(l))
            })
            .collect();
        return format!("<pre><code>{}</code></pre>", lines.join("\n"));
    }
    let spans = crate::highlight::highlight(lang, code);
    let mut out = String::from("<pre><code>");
    let mut at = 0;
    for (range, kind) in spans {
        out.push_str(&html_escape(&code[at..range.start]));
        let class = match kind {
            Kind::Keyword => "k",
            Kind::String => "s",
            Kind::Comment => "c",
            Kind::Function => "f",
            Kind::Type => "t",
            Kind::Constant => "n",
        };
        out.push_str(&format!(
            "<span class=\"{class}\">{}</span>",
            html_escape(&code[range.clone()])
        ));
        at = range.end;
    }
    out.push_str(&html_escape(&code[at..]));
    out.push_str("</code></pre>");
    out
}

/// Where exports are saved.
pub fn save(session: &Session, html_format: bool) -> anyhow::Result<(std::path::PathBuf, usize)> {
    let (text, redacted) = if html_format {
        html(session)
    } else {
        markdown(session)
    };
    let dir = crate::config::data_dir().join("exports");
    std::fs::create_dir_all(&dir)?;
    let slug: String = session
        .title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("-");
    let path = dir.join(format!(
        "{}-{}.{}",
        if slug.is_empty() { "session" } else { &slug },
        &session.id[..session.id.len().min(15)],
        if html_format { "html" } else { "md" }
    ));
    std::fs::write(&path, text)?;
    Ok((path, redacted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secret_looking_text() {
        let text = "key sk-abcdefghijklmnopqrstuvwx and AKIAABCDEFGHIJKLMNOP\npassword=hunter2hunter2\nAuthorization: Bearer abcdefghijklmnopqrstuvwxyz123456\n-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----\nplain words stay";
        let (clean, n) = redact(text);
        assert_eq!(n, 5, "{clean}");
        for secret in [
            "sk-abcdefghij",
            "AKIAABCDEF",
            "hunter2",
            "abcdefghijklmnopqrstuvwxyz123456",
            "MIIE",
        ] {
            assert!(!clean.contains(secret), "{secret} leaked: {clean}");
        }
        assert!(clean.contains("password=[redacted]"));
        assert!(clean.contains("plain words stay"));
    }

    #[test]
    fn exports_markdown_and_html_without_secrets() {
        let mut session = Session::new(&std::env::temp_dir());
        session.title = "Fix the parser".into();
        session.entries = vec![
            Entry::User {
                text: "Use my key sk-live1234567890abcdefgh please".into(),
                agent: Default::default(),
                skill: None,
                images: vec![],
            },
            Entry::Text {
                text: "Done:\n\n```rust\nfn main() { let x = 1; }\n```".into(),
            },
            Entry::Tool {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": ".env"}),
                state: crate::session::ToolState::Done,
                meta: crate::tools::ToolMeta {
                    title: "Read .env".into(),
                    detail: Some("API_TOKEN=abc".into()),
                    ..Default::default()
                },
            },
        ];
        let (md, redacted) = markdown(&session);
        assert!(md.starts_with("# Fix the parser"));
        assert!(!md.contains("sk-live1234567890"));
        assert!(!md.contains("API_TOKEN=abc"));
        assert!(redacted >= 2);
        let (page, _) = html(&session);
        if cfg!(feature = "lang-rust") {
            assert!(page.contains("<span class=\"k\">fn</span>"));
        }
        assert!(!page.contains("sk-live1234567890"));
        assert!(page.starts_with("<!doctype html>"));
    }
}
