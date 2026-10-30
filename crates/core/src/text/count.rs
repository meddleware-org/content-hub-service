//! Counters: how each platform measures a rendered text against its limit.
//!
//! - `bytes` — UTF-8 bytes; `codepoints` — Unicode scalar values; `utf16` — UTF-16 code units;
//!   `graphemes` — extended grapheme clusters (UAX #29).
//! - `x_weighted` — X's published counting configuration (twitter-text v3): text NFC-normalised; code points in the
//!   ranges U+0000–U+10FF, U+2000–U+200D, U+2010–U+201F, U+2032–U+2037 weigh 1, every other code point 2; an emoji
//!   sequence (a grapheme cluster of several code points containing U+200D, U+FE0F, U+20E3, a skin-tone modifier or a
//!   code point at or above U+1F000) weighs 2 in total; every URL weighs 23. Domains written without a scheme are
//!   counted as text (X itself may count them as a URL): the hub always writes links with a scheme.
//! - URL weight (`url_weight` in a definition's `secondary_limits`): every URL counts as that many characters
//!   (Mastodon: its instance's `characters_reserved_per_url`).
//! - Remote mentions (`mention_local_part`): `@user@host` counts as `@user` (Mastodon).
//!
//! URLs are found with the same rule as the canonical format's bare URLs (`markup::scan_url`).

use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

use super::markup::scan_url;
use crate::model::CountingMethod;

/// How to count one text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountRules {
    pub method: CountingMethod,
    /// Each URL counts as this many characters (X: always 23).
    pub url_weight: Option<u32>,
    /// `@user@host` counts as `@user`.
    pub mention_local_part: bool,
}

impl CountRules {
    pub const fn plain(method: CountingMethod) -> Self {
        CountRules { method, url_weight: None, mention_local_part: false }
    }
}

/// X's transformed URL length.
pub const X_URL_WEIGHT: u32 = 23;

/// Replace every URL by `weight` placeholder characters and every remote mention by its local part.
fn substitute(text: &str, url_weight: Option<u32>, mention_local_part: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let c = text[i..].chars().next().unwrap_or('\0');
        if let (Some(w), 'h') = (url_weight, c)
            && let Some(end) = scan_url(text, i)
        {
            out.extend(std::iter::repeat_n('x', w as usize));
            i = end;
            continue;
        }
        if mention_local_part
            && c == '@'
            && !text[..i].chars().next_back().is_some_and(|p| p.is_alphanumeric() || p == '_')
        {
            let user_len = text[i + 1..]
                .chars()
                .take_while(|x| x.is_ascii_alphanumeric() || *x == '_')
                .map(char::len_utf8)
                .sum::<usize>();
            let after = i + 1 + user_len;
            if user_len > 0 && text[after..].starts_with('@') {
                let host_len = text[after + 1..]
                    .chars()
                    .take_while(|x| x.is_ascii_alphanumeric() || *x == '.' || *x == '-')
                    .map(char::len_utf8)
                    .sum::<usize>();
                let host = text[after + 1..after + 1 + host_len].trim_end_matches('.');
                if host.contains('.') {
                    out.push_str(&text[i..after]);
                    i = after + 1 + host.len();
                    continue;
                }
            }
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn x_char_weight(c: char) -> u64 {
    let u = c as u32;
    if u <= 0x10FF || (0x2000..=0x200D).contains(&u) || (0x2010..=0x201F).contains(&u) || (0x2032..=0x2037).contains(&u)
    {
        1
    } else {
        2
    }
}

fn is_emoji_sequence(g: &str) -> bool {
    let mut n = 0;
    let mut marker = false;
    for c in g.chars() {
        n += 1;
        let u = c as u32;
        if u == 0x200D || u == 0xFE0F || u == 0x20E3 || (0x1F3FB..=0x1F3FF).contains(&u) || u >= 0x1F000 {
            marker = true;
        }
    }
    n > 1 && marker
}

fn x_weighted(text: &str) -> u64 {
    let normal: String = text.nfc().collect();
    let substituted = substitute(&normal, Some(X_URL_WEIGHT), false);
    substituted.graphemes(true).map(|g| if is_emoji_sequence(g) { 2 } else { g.chars().map(x_char_weight).sum() }).sum()
}

/// Count `text` under `rules`.
pub fn count(rules: &CountRules, text: &str) -> u64 {
    if rules.method == CountingMethod::XWeighted {
        return x_weighted(text);
    }
    let t = if rules.url_weight.is_some() || rules.mention_local_part {
        substitute(text, rules.url_weight, rules.mention_local_part)
    } else {
        text.to_owned()
    };
    match rules.method {
        CountingMethod::Bytes => t.len() as u64,
        CountingMethod::Codepoints => t.chars().count() as u64,
        CountingMethod::Utf16 => t.encode_utf16().count() as u64,
        CountingMethod::Graphemes => t.graphemes(true).count() as u64,
        CountingMethod::XWeighted => x_weighted(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CountingMethod::*;

    fn c(m: CountingMethod, s: &str) -> u64 {
        count(&CountRules::plain(m), s)
    }

    #[test]
    fn the_basic_units() {
        let s = "aé😀👩‍💻";
        assert_eq!(c(Bytes, s), 1 + 2 + 4 + 11);
        assert_eq!(c(Codepoints, s), 1 + 1 + 1 + 3);
        assert_eq!(c(Utf16, s), 1 + 1 + 2 + 5);
        assert_eq!(c(Graphemes, s), 4);
    }

    #[test]
    fn x_weights_ranges_emoji_and_urls() {
        assert_eq!(c(XWeighted, "hello"), 5);
        assert_eq!(c(XWeighted, "日本"), 4);
        assert_eq!(c(XWeighted, "😀"), 2);
        assert_eq!(c(XWeighted, "👩‍💻"), 2, "a ZWJ sequence counts once");
        assert_eq!(c(XWeighted, "see https://example.org/a/very/long/path?x=1"), 4 + 23);
        // NFC: e + combining acute is one code point after normalisation.
        assert_eq!(c(XWeighted, "e\u{301}"), 1);
    }

    #[test]
    fn mastodon_rules() {
        let r = CountRules { method: Graphemes, url_weight: Some(23), mention_local_part: true };
        assert_eq!(count(&r, "hi @alice@example.social https://example.org/x"), "hi @alice ".len() as u64 + 23);
        assert_eq!(count(&r, "mail me@example.org"), "mail me@example.org".len() as u64, "an address is not a mention");
    }

    proptest::proptest! {
        #[test]
        fn counts_never_panic_and_are_ordered(s in "\\PC{0,200}") {
            let g = c(Graphemes, &s);
            let cp = c(Codepoints, &s);
            let u16 = c(Utf16, &s);
            let b = c(Bytes, &s);
            proptest::prop_assert!(g <= cp && cp <= u16 && u16 <= b.max(u16));
            proptest::prop_assert!(cp <= b);
            let _ = c(XWeighted, &s);
        }
    }
}
