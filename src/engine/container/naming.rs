//! Container naming helpers.
//!
//! `awman-<pid>-<nanos>` for ephemeral runs, and
//! `awman-amie-<condition-slug>-<8 hex>` for amie's background agents.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::engine::error::EngineError;

/// Generate an ephemeral container name: `awman-<pid>-<subsec-nanos>`.
pub fn generate_container_name() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("awman-{pid}-{nanos}")
}

// ─── amie container naming ───────────────────────────────────────────────────
//
// amie's background agents are named `awman-amie-<condition-slug>-<8 hex>`.
// The container name — not a label — is the single identity channel every
// runtime tier honours (Docker `--filter name=`, Apple client-side prefix,
// sandbox `sbx ls` prefix), so `awman status` and name-based discovery derive
// the amie marker from the name, never from a label (Apple cannot read labels
// back).

/// Common prefix on every amie container name.
pub const AMIE_NAME_PREFIX: &str = "awman-amie-";

/// Maximum condition-slug length (keeps the full container name a legal
/// identifier on every backend).
const CONDITION_SLUG_MAX_LEN: usize = 48;

/// Length of the fixed-width uniqueness token appended to an amie name.
const AMIE_UNIQUE_HEX_LEN: usize = 8;

/// Generate an amie container name: `awman-amie-<slug>-<8 lowercase hex>`.
///
/// The uniqueness token is the first 8 hex chars of a v4 UUID — random, not
/// time-based, so two conditions ticking in the same instant never collide.
/// Its **fixed** 8-char width is what makes [`parse_amie_condition_slug`]
/// unambiguous even when the slug itself contains hyphens.
///
/// The caller is expected to have validated `condition_slug` with
/// [`validate_condition_slug`] at condition-creation time.
pub fn generate_amie_container_name(condition_slug: &str) -> String {
    let unique: String = uuid::Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(AMIE_UNIQUE_HEX_LEN)
        .collect();
    format!("{AMIE_NAME_PREFIX}{condition_slug}-{unique}")
}

/// Recover the condition slug from an amie container name.
///
/// Strips the `awman-amie-` prefix and a trailing `-[0-9a-f]{8}` token,
/// returning the slug in between (which may itself contain hyphens). Returns
/// `None` for any name that is not a well-formed amie name — including
/// ephemeral `awman-<pid>-<nanos>` names and a name whose slug would be empty.
pub fn parse_amie_condition_slug(container_name: &str) -> Option<&str> {
    let rest = container_name.strip_prefix(AMIE_NAME_PREFIX)?;
    // The trailing token is a hyphen plus exactly 8 hex chars.
    if rest.len() <= AMIE_UNIQUE_HEX_LEN {
        return None;
    }
    let (slug, tail) = rest.split_at(rest.len() - AMIE_UNIQUE_HEX_LEN - 1);
    let mut tail_chars = tail.chars();
    if tail_chars.next() != Some('-') {
        return None;
    }
    if !tail_chars.all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)) {
        return None;
    }
    // Guard against `awman-amie--<hex>` (empty slug).
    if slug.is_empty() {
        return None;
    }
    Some(slug)
}

/// Validate a condition slug against `[a-z0-9]([a-z0-9-]*[a-z0-9])?`,
/// 1..=48 chars. Rejected slugs would produce an illegal container name on
/// one or more backends.
pub fn validate_condition_slug(slug: &str) -> Result<(), EngineError> {
    let invalid = |reason: &str| {
        EngineError::Config(format!(
            "invalid condition name {slug:?}: {reason}; \
             names must match [a-z0-9]([a-z0-9-]*[a-z0-9])? and be 1..={CONDITION_SLUG_MAX_LEN} chars"
        ))
    };
    if slug.is_empty() {
        return Err(invalid("must not be empty"));
    }
    if slug.len() > CONDITION_SLUG_MAX_LEN {
        return Err(invalid("too long"));
    }
    let is_lower_alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !slug.chars().all(|c| is_lower_alnum(c) || c == '-') {
        return Err(invalid(
            "may only contain lowercase letters, digits, and hyphens",
        ));
    }
    // First and last chars must be alphanumeric (no leading/trailing hyphen).
    let first = slug.chars().next().unwrap();
    let last = slug.chars().next_back().unwrap();
    if !is_lower_alnum(first) || !is_lower_alnum(last) {
        return Err(invalid("must start and end with a letter or digit"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_format_starts_with_awman() {
        let n = generate_container_name();
        assert!(n.starts_with("awman-"), "got: {n}");
        assert!(n.len() > "awman-".len() + 5);
    }

    #[test]
    fn names_are_unique_across_calls() {
        let a = generate_container_name();
        std::thread::sleep(std::time::Duration::from_nanos(2));
        let b = generate_container_name();
        assert_ne!(a, b);
    }

    // ─── amie naming ────────────────────────────────────────────────────────

    #[test]
    fn amie_name_has_prefix_and_8_hex_token() {
        let n = generate_amie_container_name("issue-triage");
        assert!(n.starts_with("awman-amie-issue-triage-"), "got: {n}");
        let token = n.rsplit('-').next().unwrap();
        assert_eq!(token.len(), 8, "unique token must be 8 chars: {n}");
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "token must be lowercase hex: {token}"
        );
    }

    #[test]
    fn amie_name_round_trips_including_hyphenated_slug() {
        for slug in ["issue-triage", "a", "a-b-c", "deploy2", "x9-y8-z7"] {
            let name = generate_amie_container_name(slug);
            assert_eq!(
                parse_amie_condition_slug(&name),
                Some(slug),
                "round trip failed for slug {slug:?} (name {name})"
            );
        }
    }

    #[test]
    fn parse_returns_none_for_non_amie_names() {
        assert_eq!(parse_amie_condition_slug("awman-12345-678"), None);
        assert_eq!(parse_amie_condition_slug("nginx"), None);
        // Prefix present but no room for a slug + `-` + 8 hex.
        assert_eq!(parse_amie_condition_slug("awman-amie-ab12cd34"), None);
        // Empty slug.
        assert_eq!(parse_amie_condition_slug("awman-amie--12345678"), None);
        // Trailing token not 8 hex.
        assert_eq!(parse_amie_condition_slug("awman-amie-foo-1234567g"), None);
        assert_eq!(parse_amie_condition_slug("awman-amie-foo-123456789"), None);
    }

    #[test]
    fn parse_recovers_hyphenated_slug_example() {
        assert_eq!(
            parse_amie_condition_slug("awman-amie-a-b-12345678"),
            Some("a-b")
        );
    }

    #[test]
    fn validate_condition_slug_accepts_legal_slugs() {
        for slug in ["a", "a1", "issue-triage", "x-y-z", "deploy2"] {
            assert!(
                validate_condition_slug(slug).is_ok(),
                "expected {slug:?} to be valid"
            );
        }
    }

    #[test]
    fn validate_condition_slug_rejects_illegal_slugs() {
        for slug in [
            "",
            "-lead",
            "trail-",
            "Upper",
            "has_underscore",
            "has space",
            "a--------------------------------------------------b", // > 48
        ] {
            assert!(
                validate_condition_slug(slug).is_err(),
                "expected {slug:?} to be rejected"
            );
        }
    }
}
