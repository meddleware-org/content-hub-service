//! Manual-assist links: fill a definition's URL template (e.g. `https://x.com/intent/tweet?text={text}&url={url}`).
//!
//! Every value is percent-encoded (RFC 3986: only unreserved characters stay literal), except `{target_base}`,
//! which is a target's own https base URL and is inserted as is after validation. Placeholders without a value
//! become empty. The result must be an https URL.

use std::collections::BTreeMap;

fn encode(v: &str, out: &mut String) {
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntentError {
    #[error("the template is malformed")]
    Malformed,
    #[error("target_base must be an https URL without spaces")]
    BadBase,
    #[error("the result is not an https URL")]
    NotHttps,
}

/// Fill `template` with `values` (placeholder name -> raw value).
pub fn fill(template: &str, values: &BTreeMap<&str, &str>) -> Result<String, IntentError> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or(IntentError::Malformed)?;
        let name = &after[..close];
        let v = values.get(name).copied().unwrap_or("");
        if name == "target_base" {
            if !v.starts_with("https://") || v.contains(char::is_whitespace) {
                return Err(IntentError::BadBase);
            }
            out.push_str(v.trim_end_matches('/'));
        } else {
            encode(v, &mut out);
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    if out.starts_with("https://") { Ok(out) } else { Err(IntentError::NotHttps) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_encoded() {
        let v = BTreeMap::from([("text", "Hello & welcome #1"), ("url", "https://example.org/a?b=c")]);
        assert_eq!(
            fill("https://x.com/intent/tweet?text={text}&url={url}", &v).expect("ok"),
            "https://x.com/intent/tweet?text=Hello%20%26%20welcome%20%231&url=https%3A%2F%2Fexample.org%2Fa%3Fb%3Dc"
        );
    }

    #[test]
    fn target_base_is_validated() {
        let ok = BTreeMap::from([("target_base", "https://forum.example.org/"), ("title", "T"), ("body", "B")]);
        assert_eq!(
            fill("{target_base}/new-topic?title={title}&body={body}", &ok).expect("ok"),
            "https://forum.example.org/new-topic?title=T&body=B"
        );
        let bad = BTreeMap::from([("target_base", "javascript:alert(1)")]);
        assert_eq!(fill("{target_base}/x", &bad), Err(IntentError::BadBase));
        assert_eq!(fill("{text", &BTreeMap::new()), Err(IntentError::Malformed));
    }
}
