//! WI 0101 — amie container naming: round-trip parsing and slug validation.
//!
//! These duplicate (at the public-API surface) the in-file unit tests already
//! present in `src/engine/container/naming.rs` (owned by `engine-runtime`),
//! acting as a regression guard reachable from outside the crate: if a future
//! change makes `generate_amie_container_name` / `parse_amie_condition_slug`
//! diverge, both the in-file tests AND this integration test catch it.

use awman::engine::container::naming::{
    generate_amie_container_name, parse_amie_condition_slug, validate_condition_slug,
};

#[test]
fn amie_name_round_trips_for_slugs_containing_hyphens() {
    for slug in ["issue-triage", "a-b", "a-b-c-d", "release-notes-2026"] {
        let name = generate_amie_container_name(slug);
        assert!(
            name.starts_with("awman-amie-"),
            "generated name must carry the amie prefix: {name}"
        );
        assert_eq!(
            parse_amie_condition_slug(&name),
            Some(slug),
            "round trip failed for hyphenated slug {slug:?} (name {name})"
        );
    }
}

#[test]
fn non_amie_container_name_parses_to_none() {
    // The ephemeral session-container naming scheme (`awman-<pid>-<nanos>`)
    // must never be mistaken for an amie name.
    assert_eq!(parse_amie_condition_slug("awman-12345-678901234"), None);
    assert_eq!(parse_amie_condition_slug("nginx"), None);
    assert_eq!(parse_amie_condition_slug("awman-amie-"), None);
}

#[test]
fn condition_slug_validation_rejects_uppercase() {
    let err = validate_condition_slug("Issue-Triage").unwrap_err();
    assert!(
        err.to_string().contains("lowercase"),
        "error must explain the lowercase requirement: {err}"
    );
}

#[test]
fn condition_slug_validation_rejects_leading_and_trailing_hyphen() {
    assert!(validate_condition_slug("-leading").is_err());
    assert!(validate_condition_slug("trailing-").is_err());
}

#[test]
fn condition_slug_validation_rejects_characters_illegal_in_a_container_name() {
    for slug in ["has_underscore", "has space", "has.dot", "has/slash"] {
        assert!(
            validate_condition_slug(slug).is_err(),
            "expected {slug:?} to be rejected as an illegal container-name component"
        );
    }
}

#[test]
fn every_slug_validate_accepts_produces_a_legal_amie_container_name() {
    // A stored condition's slug, once validated, must always be able to
    // produce a container name every runtime tier accepts: lowercase
    // alphanumerics, hyphens, and underscores only, starting/ending
    // alphanumeric.
    for slug in ["issue-triage", "a", "deploy2", "x9-y8-z7"] {
        validate_condition_slug(slug).expect("slug must validate");
        let name = generate_amie_container_name(slug);
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "generated container name must only contain characters legal on every backend: {name}"
        );
    }
}
