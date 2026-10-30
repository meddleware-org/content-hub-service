//! Renderers: the canonical format (markup.rs) to what each platform receives.
//!
//! Each renderer returns the **payload** (what is sent), the **visible** text (what the platform shows and counts:
//! the same as the payload except for Telegram HTML, where Telegram counts the text after entity parsing) and an
//! **offset map** from source byte ranges to payload byte ranges, one entry per token.
//!
//! | Markup | strong | emphasis | code | link | bullet |
//! | --- | --- | --- | --- | --- | --- |
//! | plain | text | text | text | `text (url)`, or `url` when the text is the URL | `• ` |
//! | markdown, discord_md | `**text**` | `*text*` | `` `text` `` | `[text](url)` | `- ` |
//! | telegram_html | `<b>` | `<i>` | `<code>` | `<a href="url">text</a>` | `• ` |
//! | bbcode | `[b]` | `[i]` | `[code]` | `[url=url]text[/url]` | `• ` |
//!
//! Text is escaped for the target: Markdown escapes `\ * _ ` [ ]`, Telegram HTML escapes `& < > "` (the only
//! entities Telegram accepts), BBCode and plain need none. Line breaks are `\n`, paragraph breaks `\n\n`.

use std::ops::Range;

use super::markup::{Spanned, Token};
use crate::model::Markup;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapEntry {
    pub src: Range<usize>,
    pub out: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub payload: String,
    pub visible: String,
    pub map: Vec<MapEntry>,
}

fn escape_markdown(s: &str, out: &mut String) {
    for c in s.chars() {
        if matches!(c, '\\' | '*' | '_' | '`' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
}

fn escape_html(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

/// Render tokens for a markup.
pub fn render(markup: Markup, tokens: &[Spanned]) -> Rendered {
    let mut payload = String::new();
    let mut visible = String::new();
    let mut map = Vec::with_capacity(tokens.len());
    let md = matches!(markup, Markup::Markdown | Markup::DiscordMd);
    for t in tokens {
        let start = payload.len();
        match &t.token {
            Token::Text(s) => {
                match markup {
                    Markup::Markdown | Markup::DiscordMd => escape_markdown(s, &mut payload),
                    Markup::TelegramHtml => escape_html(s, &mut payload),
                    Markup::Plain | Markup::Bbcode => payload.push_str(s),
                }
                visible.push_str(s);
            }
            Token::StrongOpen | Token::StrongClose | Token::EmphOpen | Token::EmphClose => {
                let open = matches!(t.token, Token::StrongOpen | Token::EmphOpen);
                let strong = matches!(t.token, Token::StrongOpen | Token::StrongClose);
                let mark = match (markup, strong, open) {
                    (Markup::Markdown | Markup::DiscordMd, true, _) => "**",
                    (Markup::Markdown | Markup::DiscordMd, false, _) => "*",
                    (Markup::TelegramHtml, true, true) => "<b>",
                    (Markup::TelegramHtml, true, false) => "</b>",
                    (Markup::TelegramHtml, false, true) => "<i>",
                    (Markup::TelegramHtml, false, false) => "</i>",
                    (Markup::Bbcode, true, true) => "[b]",
                    (Markup::Bbcode, true, false) => "[/b]",
                    (Markup::Bbcode, false, true) => "[i]",
                    (Markup::Bbcode, false, false) => "[/i]",
                    (Markup::Plain, _, _) => "",
                };
                payload.push_str(mark);
                if markup != Markup::TelegramHtml {
                    visible.push_str(mark);
                }
            }
            Token::Code(s) => match markup {
                Markup::Markdown | Markup::DiscordMd => {
                    payload.push('`');
                    payload.push_str(s);
                    payload.push('`');
                    visible.push('`');
                    visible.push_str(s);
                    visible.push('`');
                }
                Markup::TelegramHtml => {
                    payload.push_str("<code>");
                    escape_html(s, &mut payload);
                    payload.push_str("</code>");
                    visible.push_str(s);
                }
                Markup::Bbcode => {
                    let x = format!("[code]{s}[/code]");
                    payload.push_str(&x);
                    visible.push_str(&x);
                }
                Markup::Plain => {
                    payload.push_str(s);
                    visible.push_str(s);
                }
            },
            Token::Link { text, url } => match markup {
                Markup::Markdown | Markup::DiscordMd => {
                    let mut x = String::from("[");
                    escape_markdown(text, &mut x);
                    x.push_str("](");
                    x.push_str(url);
                    x.push(')');
                    payload.push_str(&x);
                    visible.push_str(&x);
                }
                Markup::TelegramHtml => {
                    payload.push_str("<a href=\"");
                    escape_html(url, &mut payload);
                    payload.push_str("\">");
                    escape_html(text, &mut payload);
                    payload.push_str("</a>");
                    visible.push_str(text);
                }
                Markup::Bbcode => {
                    let x = format!("[url={url}]{text}[/url]");
                    payload.push_str(&x);
                    visible.push_str(&x);
                }
                Markup::Plain => {
                    let x = if text == url { url.clone() } else { format!("{text} ({url})") };
                    payload.push_str(&x);
                    visible.push_str(&x);
                }
            },
            Token::Url(u) => {
                if markup == Markup::TelegramHtml {
                    escape_html(u, &mut payload);
                } else {
                    payload.push_str(u);
                }
                visible.push_str(u);
            }
            Token::LineBreak => {
                payload.push('\n');
                visible.push('\n');
            }
            Token::ParagraphBreak => {
                payload.push_str("\n\n");
                visible.push_str("\n\n");
            }
            Token::Bullet => {
                let b = if md { "- " } else { "• " };
                payload.push_str(b);
                visible.push_str(b);
            }
        }
        map.push(MapEntry { src: t.src.clone(), out: start..payload.len() });
    }
    Rendered { payload, visible, map }
}

/// Parse and render in one step.
pub fn render_source(markup: Markup, source: &str) -> Rendered {
    render(markup, &super::markup::parse(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "**Hi** & <b> [docs](https://x.org/a?b=1&c=2) `x<y`\n\n- item";

    #[test]
    fn every_markup_renders_the_same_source() {
        assert_eq!(render_source(Markup::Plain, SRC).payload, "Hi & <b> docs (https://x.org/a?b=1&c=2) x<y\n\n• item");
        assert_eq!(
            render_source(Markup::Markdown, SRC).payload,
            "**Hi** & <b> [docs](https://x.org/a?b=1&c=2) `x<y`\n\n- item"
        );
        assert_eq!(
            render_source(Markup::TelegramHtml, SRC).payload,
            "<b>Hi</b> &amp; &lt;b&gt; <a href=\"https://x.org/a?b=1&amp;c=2\">docs</a> <code>x&lt;y</code>\n\n• item"
        );
        assert_eq!(render_source(Markup::TelegramHtml, SRC).visible, "Hi & <b> docs x<y\n\n• item");
        assert_eq!(
            render_source(Markup::Bbcode, SRC).payload,
            "[b]Hi[/b] & <b> [url=https://x.org/a?b=1&c=2]docs[/url] [code]x<y[/code]\n\n• item"
        );
    }

    #[test]
    fn markdown_escapes_literal_specials() {
        assert_eq!(render_source(Markup::Markdown, r"2 \* 3 \[x\]").payload, r"2 \* 3 \[x\]");
        assert_eq!(render_source(Markup::Plain, r"2 \* 3").payload, "2 * 3");
    }

    #[test]
    fn the_map_covers_the_payload_in_order() {
        let r = render_source(Markup::TelegramHtml, SRC);
        assert_eq!(r.map.first().map(|m| m.out.start), Some(0));
        assert_eq!(r.map.last().map(|m| m.out.end), Some(r.payload.len()));
        for w in r.map.windows(2) {
            assert_eq!(w[0].out.end, w[1].out.start);
        }
    }
}
