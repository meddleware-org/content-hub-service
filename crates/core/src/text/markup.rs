//! The canonical post format: a small CommonMark subset (ARCHITECTURE.md §8).
//!
//! Supported: paragraphs (blank line), line breaks, `**strong**`, `*emphasis*`, `` `code` ``, `[text](https://url)`,
//! bare `https://` / `http://` URLs, `- ` / `* ` bullets at the start of a line, and backslash escapes of
//! `` \ * _ ` [ ] ( ) ``. Nothing else is markup: raw HTML is text (renderers escape it), and an unmatched `*` or
//! `**` is a literal star.
//!
//! The same algorithm is implemented in `@meddleware/content-hub-client` (TypeScript); the golden vectors pin both.
//! Source ranges are byte offsets into the input; the vectors convert them to code-point offsets.

use std::ops::Range;

/// One token of the canonical format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Text(String),
    StrongOpen,
    StrongClose,
    EmphOpen,
    EmphClose,
    Code(String),
    Link { text: String, url: String },
    Url(String),
    LineBreak,
    ParagraphBreak,
    Bullet,
}

/// A token and the source bytes it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned {
    pub token: Token,
    pub src: Range<usize>,
}

const ESCAPABLE: &[char] = &['\\', '*', '_', '`', '[', ']', '(', ')'];

/// Characters that end a bare URL.
fn ends_url(c: char) -> bool {
    c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '`')
}

/// If a bare URL starts at byte `i` of `s`, its end (exclusive). The scheme must be `https://` or `http://`, the
/// previous character must not be alphanumeric, and at least one character must follow the scheme. Trailing
/// punctuation `. , ; : ! ? *` and an unbalanced `)` are not part of the URL.
pub fn scan_url(s: &str, i: usize) -> Option<usize> {
    let rest = &s[i..];
    let scheme = if rest.starts_with("https://") {
        8
    } else if rest.starts_with("http://") {
        7
    } else {
        return None;
    };
    if s[..i].chars().next_back().is_some_and(char::is_alphanumeric) {
        return None;
    }
    let mut end = i + scheme;
    for (k, c) in s[end..].char_indices() {
        if ends_url(c) {
            end += k;
            return trim_url(s, i, i + scheme, end);
        }
    }
    end = s.len();
    trim_url(s, i, i + scheme, end)
}

fn trim_url(s: &str, start: usize, after_scheme: usize, mut end: usize) -> Option<usize> {
    while let Some(c) = s[..end].chars().next_back() {
        if end <= after_scheme {
            break;
        }
        let strip = match c {
            '.' | ',' | ';' | ':' | '!' | '?' | '*' => true,
            ')' => {
                let body = &s[start..end];
                body.matches('(').count() < body.matches(')').count()
            }
            _ => false,
        };
        if !strip {
            break;
        }
        end -= c.len_utf8();
    }
    (end > after_scheme).then_some(end)
}

/// Try `[text](url)` at byte `i` (which holds `[`). Returns (text, url, end).
fn scan_link(s: &str, i: usize) -> Option<(String, String, usize)> {
    let close = s[i + 1..].find(']')? + i + 1;
    let text = &s[i + 1..close];
    if text.is_empty() || text.contains('\n') || text.contains('[') {
        return None;
    }
    if !s[close + 1..].starts_with('(') {
        return None;
    }
    let url_start = close + 2;
    let url_end = s[url_start..].find(')')? + url_start;
    let url = &s[url_start..url_end];
    let scheme = if url.starts_with("https://") {
        8
    } else if url.starts_with("http://") {
        7
    } else {
        return None;
    };
    if url.len() <= scheme || url.chars().any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '(')) {
        return None;
    }
    Some((text.to_owned(), url.to_owned(), url_end + 1))
}

#[derive(Debug)]
enum Raw {
    Tok(Token),
    /// A run of `*` that may open and/or close (length 1 or 2).
    Delim {
        len: usize,
        can_open: bool,
        can_close: bool,
    },
}

/// Parse the canonical format into tokens. Never fails: anything that is not markup is text.
pub fn parse(s: &str) -> Vec<Spanned> {
    let mut raw: Vec<(Raw, Range<usize>)> = Vec::new();
    let mut text = String::new();
    let mut text_start = 0;
    let flush = |raw: &mut Vec<(Raw, Range<usize>)>, text: &mut String, start: usize, end: usize| {
        if !text.is_empty() {
            raw.push((Raw::Tok(Token::Text(std::mem::take(text))), start..end));
        }
    };
    let mut i = 0;
    let mut line_start = true;
    while i < s.len() {
        let c = s[i..].chars().next().unwrap_or('\0');
        // Bullets at the start of a line.
        if line_start && (s[i..].starts_with("- ") || s[i..].starts_with("* ")) {
            flush(&mut raw, &mut text, text_start, i);
            raw.push((Raw::Tok(Token::Bullet), i..i + 2));
            i += 2;
            text_start = i;
            line_start = false;
            continue;
        }
        line_start = false;
        if c == '\n' {
            flush(&mut raw, &mut text, text_start, i);
            let n = s[i..].chars().take_while(|&x| x == '\n').count();
            let tok = if n >= 2 { Token::ParagraphBreak } else { Token::LineBreak };
            raw.push((Raw::Tok(tok), i..i + n));
            i += n;
            text_start = i;
            line_start = true;
            continue;
        }
        if c == '\\'
            && let Some(next) = s[i + 1..].chars().next().filter(|n| ESCAPABLE.contains(n))
        {
            if text.is_empty() {
                text_start = i;
            }
            text.push(next);
            i += 1 + next.len_utf8();
            continue;
        }
        if c == '`'
            && let Some(rel) = s[i + 1..].find('`')
        {
            let body = &s[i + 1..i + 1 + rel];
            if !body.is_empty() && !body.contains("\n\n") {
                flush(&mut raw, &mut text, text_start, i);
                let end = i + 2 + rel;
                raw.push((Raw::Tok(Token::Code(body.to_owned())), i..end));
                i = end;
                text_start = i;
                continue;
            }
        }
        if c == '['
            && let Some((t, u, end)) = scan_link(s, i)
        {
            flush(&mut raw, &mut text, text_start, i);
            raw.push((Raw::Tok(Token::Link { text: t, url: u }), i..end));
            i = end;
            text_start = i;
            continue;
        }
        if c == 'h'
            && let Some(end) = scan_url(s, i)
        {
            flush(&mut raw, &mut text, text_start, i);
            raw.push((Raw::Tok(Token::Url(s[i..end].to_owned())), i..end));
            i = end;
            text_start = i;
            continue;
        }
        if c == '*' {
            let run = s[i..].chars().take_while(|&x| x == '*').count();
            if run <= 2 {
                let before = s[..i].chars().next_back();
                let after = s[i + run..].chars().next();
                let can_open = after.is_some_and(|a| !a.is_whitespace());
                let can_close = before.is_some_and(|b| !b.is_whitespace());
                if can_open || can_close {
                    flush(&mut raw, &mut text, text_start, i);
                    raw.push((Raw::Delim { len: run, can_open, can_close }, i..i + run));
                    i += run;
                    text_start = i;
                    continue;
                }
            }
            if text.is_empty() {
                text_start = i;
            }
            text.push_str(&s[i..i + run]);
            i += run;
            continue;
        }
        if text.is_empty() {
            text_start = i;
        }
        text.push(c);
        i += c.len_utf8();
    }
    flush(&mut raw, &mut text, text_start, s.len());

    // Pair the delimiters: a closer matches the most recent unmatched opener of the same length.
    let mut kind: Vec<Option<Token>> = vec![None; raw.len()];
    let mut stack: Vec<(usize, usize)> = Vec::new(); // (raw index, len)
    for (idx, (r, _)) in raw.iter().enumerate() {
        if let Raw::Delim { len, can_open, can_close } = *r {
            let matched = if can_close { stack.iter().rposition(|&(_, l)| l == len) } else { None };
            if let Some(pos) = matched {
                let (open_idx, _) = stack.remove(pos);
                // Openers between the pair stay unmatched (dropped from the stack).
                stack.truncate(pos);
                kind[open_idx] = Some(if len == 2 { Token::StrongOpen } else { Token::EmphOpen });
                kind[idx] = Some(if len == 2 { Token::StrongClose } else { Token::EmphClose });
            } else if can_open {
                stack.push((idx, len));
            }
        }
    }

    // Emit, turning unmatched delimiters into text and merging adjacent text.
    let mut out: Vec<Spanned> = Vec::new();
    for (idx, (r, range)) in raw.into_iter().enumerate() {
        let tok = match r {
            Raw::Tok(t) => t,
            Raw::Delim { len, .. } => match kind[idx].take() {
                Some(t) => t,
                None => Token::Text("*".repeat(len)),
            },
        };
        if let (Token::Text(add), Some(Spanned { token: Token::Text(prev), src })) = (&tok, out.last_mut())
            && src.end == range.start
        {
            prev.push_str(add);
            src.end = range.end;
            continue;
        }
        out.push(Spanned { token: tok, src: range });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use Token::*;

    fn toks(s: &str) -> Vec<Token> {
        parse(s).into_iter().map(|t| t.token).collect()
    }

    #[test]
    fn plain_text_is_one_token() {
        assert_eq!(toks("hello world"), vec![Text("hello world".into())]);
    }

    #[test]
    fn strong_emphasis_and_code() {
        assert_eq!(
            toks("a **b** *c* `d`"),
            vec![
                Text("a ".into()),
                StrongOpen,
                Text("b".into()),
                StrongClose,
                Text(" ".into()),
                EmphOpen,
                Text("c".into()),
                EmphClose,
                Text(" ".into()),
                Code("d".into())
            ]
        );
    }

    #[test]
    fn unmatched_delimiters_are_literal() {
        assert_eq!(toks("2 * 3 = 6"), vec![Text("2 * 3 = 6".into())]);
        assert_eq!(toks("**open"), vec![Text("**open".into())]);
        assert_eq!(toks("`tick"), vec![Text("`tick".into())]);
    }

    #[test]
    fn links_and_bare_urls() {
        assert_eq!(
            toks("see [docs](https://example.org/a) or https://example.org/b."),
            vec![
                Text("see ".into()),
                Link { text: "docs".into(), url: "https://example.org/a".into() },
                Text(" or ".into()),
                Url("https://example.org/b".into()),
                Text(".".into())
            ]
        );
        assert_eq!(
            toks("(https://example.org/x)"),
            vec![Text("(".into()), Url("https://example.org/x".into()), Text(")".into())]
        );
        assert_eq!(toks("[x](javascript:alert(1))"), vec![Text("[x](javascript:alert(1))".into())]);
        assert_eq!(toks("nohttps://x.org"), vec![Text("nohttps://x.org".into())]);
    }

    #[test]
    fn breaks_and_bullets() {
        assert_eq!(
            toks("- one\n- two\n\nend"),
            vec![Bullet, Text("one".into()), LineBreak, Bullet, Text("two".into()), ParagraphBreak, Text("end".into())]
        );
    }

    #[test]
    fn escapes_produce_literal_text() {
        assert_eq!(toks(r"\*not em\*"), vec![Text("*not em*".into())]);
    }

    #[test]
    fn source_ranges_cover_the_input_in_order() {
        let s = "a **b** [c](https://x.org) d\n\n- e `f`";
        let t = parse(s);
        assert_eq!(t.first().map(|x| x.src.start), Some(0));
        assert_eq!(t.last().map(|x| x.src.end), Some(s.len()));
        for w in t.windows(2) {
            assert_eq!(w[0].src.end, w[1].src.start);
        }
    }
}
