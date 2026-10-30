//! Identifier shapes shared by the seed format, the API and the database CHECK constraints.

/// A roadmap phase, item or task key: `^[a-z0-9][a-z0-9-]{0,62}$` (the seed schema's `key`).
pub fn is_key(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// A platform definition code: `^[A-Z][A-Z0-9_]{0,40}$`.
pub fn is_platform_code(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 41
        && b[0].is_ascii_uppercase()
        && b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// Every credential variable's name starts with this, so a target can only name a variable that was put in the
/// worker's environment for a platform (never `DATABASE_URL` or another process setting).
pub const SECRET_PREFIX: &str = "PLATFORM_";

/// The NAME of the environment variable that holds a target's credential: `^PLATFORM_[A-Z0-9_]{1,55}$`.
/// A URL, a token or anything with lower-case letters, punctuation or spaces is refused, so a secret value
/// pasted into the field by mistake never reaches the database.
pub fn is_secret_ref(s: &str) -> bool {
    let Some(rest) = s.strip_prefix(SECRET_PREFIX) else { return false };
    let b = rest.as_bytes();
    !b.is_empty() && b.len() <= 55 && b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// A Sui address as the chain writes it: `0x` and exactly 64 hexadecimal digits.
pub fn is_sui_address(s: &str) -> bool {
    s.strip_prefix("0x").is_some_and(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sui_addresses_have_a_prefix_and_sixty_four_hex_digits() {
        assert!(is_sui_address(&format!("0x{}", "ab".repeat(32))));
        assert!(is_sui_address(&format!("0x{}", "AB".repeat(32))));
        for bad in [
            "",
            "0x",
            "0x12",
            &format!("ab{}", "ab".repeat(32)),
            &format!("0x{}", "zz".repeat(32)),
            &format!("0x{}", "ab".repeat(33)),
        ] {
            assert!(!is_sui_address(bad), "{bad}");
        }
    }

    /// The public seed schema states the same shapes (it is what operators validate their seed against).
    #[test]
    fn the_seed_schema_uses_the_same_patterns() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../schema/roadmap-seed.schema.json")).expect("schema parses");
        assert_eq!(schema["$defs"]["key"]["pattern"], "^[a-z0-9][a-z0-9-]{0,62}$");
        assert_eq!(schema["$defs"]["task"]["properties"]["platforms"]["items"]["pattern"], "^[A-Z][A-Z0-9_]{0,40}$");
    }

    #[test]
    fn keys() {
        for ok in ["a", "0", "phase-1", "a-b-c", &"a".repeat(63)] {
            assert!(is_key(ok), "{ok}");
        }
        for bad in ["", "-a", "A", "a_b", "a b", "ä", &"a".repeat(64)] {
            assert!(!is_key(bad), "{bad}");
        }
    }

    #[test]
    fn platform_codes() {
        for ok in ["X", "OWNED_FEED", "DISCORD_ANNOUNCEMENTS", "A1", &"A".repeat(41)] {
            assert!(is_platform_code(ok), "{ok}");
        }
        for bad in ["", "x", "_X", "1X", "OWNED-FEED", &"A".repeat(42)] {
            assert!(!is_platform_code(bad), "{bad}");
        }
    }

    #[test]
    fn secret_refs_are_names_never_values() {
        assert!(is_secret_ref("PLATFORM_DISCORD_ANNOUNCEMENTS_WEBHOOK_URL"));
        assert!(is_secret_ref(&format!("PLATFORM_{}", "A".repeat(55))));
        for value in [
            "DISCORD_ANNOUNCEMENTS_WEBHOOK_URL",
            "DATABASE_URL",
            "PLATFORM_",
            &format!("PLATFORM_{}", "A".repeat(56)),
            "https://discord.com/api/webhooks/1/abc",
            "xoxb-123",
            "sk_live_abc",
            "Bearer abc",
            "lower_case",
            "",
            &"A".repeat(65),
        ] {
            assert!(!is_secret_ref(value), "{value}");
        }
    }
}
