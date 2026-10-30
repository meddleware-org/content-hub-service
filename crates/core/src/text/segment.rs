//! Segmentation: how much of a post fits each selected platform (ARCHITECTURE.md §9).
//!
//! For each target, `cut` is the largest **candidate position** such that the rendered prefix fits every one of
//! the target's limits. Candidates are the grapheme boundaries of the source that do not fall inside a link or a
//! bare URL (those are atomic). The largest fitting candidate is found by binary search, which assumes that a
//! longer prefix never counts less; the canonical format can break that slightly (an unmatched `**` that a later
//! `**` closes renders shorter in plain text), so the result is the binary search's answer, pinned by the shared
//! vectors and identical in the TypeScript implementation.
//!
//! Positions in the result are **code-point offsets** into the source (shared with TypeScript, whose strings
//! index by UTF-16 unit).

use unicode_segmentation::UnicodeSegmentation;

use super::count::count;
use super::limits::Limits;
use super::markup::{Token, parse};
use super::render::render;
use crate::model::Markup;

/// One selected target.
#[derive(Debug, Clone)]
pub struct SegmentTarget {
    pub id: String,
    pub markup: Markup,
    pub limits: Limits,
}

/// A band of the source and the targets it fits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Band {
    /// Code-point offsets.
    pub start: usize,
    pub end: usize,
    /// Targets whose cut is at or beyond `end`, in the order they were given.
    pub fits: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segmentation {
    /// Each target's cut (code-point offset), in the order given.
    pub cuts: Vec<(String, usize)>,
    pub bands: Vec<Band>,
}

/// Whether a rendered text fits all limits of a target.
pub fn fits(markup: Markup, limits: &Limits, source: &str) -> bool {
    let r = render(markup, &parse(source));
    let counted = if markup == Markup::TelegramHtml { &r.visible } else { &r.payload };
    limits.primary.is_none_or(|l| count(&limits.rules, counted) <= l)
        && limits.bytes.is_none_or(|l| r.payload.len() as u64 <= l)
}

/// Byte offsets of the candidate cut positions, ascending, always including 0 and the source length.
fn candidates(source: &str) -> Vec<usize> {
    let atomic: Vec<std::ops::Range<usize>> = parse(source)
        .into_iter()
        .filter(|t| matches!(t.token, Token::Link { .. } | Token::Url(_)))
        .map(|t| t.src)
        .collect();
    let mut out: Vec<usize> = std::iter::once(0)
        .chain(source.grapheme_indices(true).map(|(i, g)| i + g.len()))
        .filter(|&b| !atomic.iter().any(|r| r.start < b && b < r.end))
        .collect();
    out.dedup();
    out
}

fn to_codepoints(source: &str, byte: usize) -> usize {
    source[..byte].chars().count()
}

/// The largest fitting candidate for one target.
fn cut_for(source: &str, cands: &[usize], t: &SegmentTarget) -> usize {
    // cands[0] is 0, which always fits (an empty message counts 0).
    let (mut lo, mut hi) = (0usize, cands.len() - 1);
    if fits(t.markup, &t.limits, &source[..cands[hi]]) {
        return cands[hi];
    }
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if fits(t.markup, &t.limits, &source[..cands[mid]]) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    cands[lo]
}

/// Segment `source` for the targets (targets with an override are not passed here; they get their own count).
pub fn segment(source: &str, targets: &[SegmentTarget]) -> Segmentation {
    let cands = candidates(source);
    let total = to_codepoints(source, source.len());
    let cuts: Vec<(String, usize)> =
        targets.iter().map(|t| (t.id.clone(), to_codepoints(source, cut_for(source, &cands, t)))).collect();
    let mut points: Vec<usize> = cuts.iter().map(|(_, c)| *c).collect();
    points.push(0);
    points.push(total);
    points.sort_unstable();
    points.dedup();
    let bands = points
        .windows(2)
        .map(|w| Band {
            start: w[0],
            end: w[1],
            fits: cuts.iter().filter(|(_, c)| *c >= w[1]).map(|(id, _)| id.clone()).collect(),
        })
        .collect();
    Segmentation { cuts, bands }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CountingMethod;
    use crate::text::count::CountRules;

    fn target(id: &str, limit: u64) -> SegmentTarget {
        SegmentTarget {
            id: id.into(),
            markup: Markup::Plain,
            limits: Limits {
                rules: CountRules::plain(CountingMethod::Codepoints),
                primary: Some(limit),
                bytes: None,
                title: None,
            },
        }
    }

    #[test]
    fn bands_follow_the_cuts() {
        let s = "abcdefghij";
        let seg = segment(s, &[target("short", 3), target("long", 7), target("all", 100)]);
        assert_eq!(seg.cuts, vec![("short".into(), 3), ("long".into(), 7), ("all".into(), 10)]);
        assert_eq!(
            seg.bands,
            vec![
                Band { start: 0, end: 3, fits: vec!["short".into(), "long".into(), "all".into()] },
                Band { start: 3, end: 7, fits: vec!["long".into(), "all".into()] },
                Band { start: 7, end: 10, fits: vec!["all".into()] },
            ]
        );
    }

    #[test]
    fn urls_are_never_split() {
        let s = "go https://example.org/abc now";
        let seg = segment(s, &[target("t", 10)]);
        // The URL starts at 3 and would overflow: the cut stays before it.
        assert_eq!(seg.cuts[0].1, 3);
    }

    #[test]
    fn graphemes_are_never_split() {
        let s = "ab👩‍💻cd";
        let seg = segment(s, &[target("t", 4)]);
        // "ab" (2) + the ZWJ sequence (3 code points) = 5 > 4, so the cut is after "ab".
        assert_eq!(seg.cuts[0].1, 2);
    }

    #[test]
    fn nothing_fits_gives_a_grey_band() {
        let seg = segment("abc", &[target("t", 0)]);
        assert_eq!(seg.bands, vec![Band { start: 0, end: 3, fits: vec![] }]);
    }
}
