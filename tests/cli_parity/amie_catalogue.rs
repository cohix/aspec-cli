//! WI 0102 — catalogue coverage for the amie CLI/TUI/API projections.
//!
//! These checks deliberately assert the complete shape of the amie subtree.
//! The cross-frontend `parity_test` is catalogue-driven and therefore covers
//! all leaf commands automatically; this module pins the non-leaf `amie`
//! flags and the user-facing command inventory as well.

use awman::command::dispatch::catalogue::{ArgumentKind, CommandCatalogue, FlagKind};

fn cat() -> &'static CommandCatalogue {
    CommandCatalogue::get()
}

fn flag_names(path: &[&str]) -> Vec<&'static str> {
    let mut names: Vec<_> = cat()
        .lookup(path)
        .unwrap_or_else(|| panic!("missing catalogue command {path:?}"))
        .flags
        .iter()
        .map(|flag| flag.long)
        .collect();
    names.sort_unstable();
    names
}

fn argument_names(path: &[&str]) -> Vec<&'static str> {
    let mut names: Vec<_> = cat()
        .lookup(path)
        .unwrap_or_else(|| panic!("missing catalogue command {path:?}"))
        .arguments
        .iter()
        .map(|argument| argument.name)
        .collect();
    names.sort_unstable();
    names
}

#[test]
fn amie_is_a_top_level_command_with_its_root_flags() {
    let top_level: Vec<_> = cat()
        .root()
        .subcommands
        .iter()
        .map(|spec| spec.name)
        .collect();
    assert!(
        top_level.contains(&"amie"),
        "amie must be in the top-level catalogue"
    );

    assert_eq!(flag_names(&["amie"]), vec!["json", "non-interactive"]);
    assert!(cat()
        .lookup(&["amie"])
        .unwrap()
        .find_flag("json")
        .unwrap()
        .implies
        .contains(&"non-interactive"));

    let non_interactive = cat()
        .lookup(&["amie"])
        .unwrap()
        .find_flag("non-interactive")
        .unwrap();
    assert_eq!(non_interactive.short, Some('n'));
    assert!(matches!(non_interactive.kind, FlagKind::Bool));
}

#[test]
fn every_amie_subcommand_has_the_contract_shape() {
    let expected = [
        (
            "start",
            vec![],
            vec!["background", "dangerously-skip-auth", "port", "refresh-key"],
        ),
        ("stop", vec![], vec![]),
        ("status", vec![], vec!["json"]),
        ("logs", vec![], vec!["follow"]),
        (
            "add",
            vec![],
            vec![
                "agent",
                "description",
                "interval",
                "interview",
                "model",
                "mount-scope",
                "name",
                "non-interactive",
                "repo",
            ],
        ),
        ("list", vec![], vec!["json"]),
        ("show", vec!["name"], vec!["json"]),
        ("remove", vec!["name"], vec!["yes"]),
        ("pause", vec!["name"], vec![]),
        ("resume", vec!["name"], vec![]),
        ("attach", vec!["name"], vec!["container"]),
    ];

    for (name, arguments, flags) in expected {
        let path = ["amie", name];
        let command = cat()
            .lookup(&path)
            .unwrap_or_else(|| panic!("missing amie command {name}"));
        assert_eq!(
            argument_names(&path),
            arguments,
            "arguments for amie {name}"
        );
        let mut expected_flags = flags;
        expected_flags.sort_unstable();
        assert_eq!(flag_names(&path), expected_flags, "flags for amie {name}");
        assert!(
            command.api_allowed || name == "attach",
            "unexpected API policy for amie {name}"
        );
    }

    let attach_name = cat().lookup(&["amie", "attach"]).unwrap().arguments[0];
    assert!(matches!(attach_name.kind, ArgumentKind::String));
}

#[test]
fn amie_start_does_not_reintroduce_the_retired_interval_flag() {
    assert!(cat()
        .lookup(&["amie", "start"])
        .unwrap()
        .find_flag("interval")
        .is_none());
}

#[test]
fn amie_flag_types_and_short_forms_are_projection_safe() {
    let add = cat().lookup(&["amie", "add"]).unwrap();
    assert!(matches!(
        add.find_flag("repo").unwrap().kind,
        FlagKind::Path
    ));
    assert!(matches!(
        add.find_flag("mount-scope").unwrap().kind,
        FlagKind::Enum(_)
    ));
    assert_eq!(add.find_flag("non-interactive").unwrap().short, Some('n'));

    let logs = cat().lookup(&["amie", "logs"]).unwrap();
    assert_eq!(logs.find_flag("follow").unwrap().short, Some('f'));

    let remove = cat().lookup(&["amie", "remove"]).unwrap();
    assert_eq!(remove.find_flag("yes").unwrap().short, Some('y'));
}
