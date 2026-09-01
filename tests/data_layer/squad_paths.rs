//! Part 1 SquadPaths environment and containment tests.

use std::path::{Path, PathBuf};

use awman::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME, AWMAN_SQUAD_ROOT, XDG_DATA_HOME};
use awman::data::fs::SquadPaths;

#[test]
fn squad_paths_env_precedence_prefers_squad_root_then_config_then_xdg() {
    let env = EnvSnapshot::with_overrides([
        (AWMAN_SQUAD_ROOT, "/explicit/squad"),
        (AWMAN_CONFIG_HOME, "/config"),
        (XDG_DATA_HOME, "/xdg/data"),
    ]);
    assert_eq!(
        SquadPaths::from_env(&env).unwrap().root(),
        Path::new("/explicit/squad")
    );

    let env =
        EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, "/config"), (XDG_DATA_HOME, "/xdg/data")]);
    assert_eq!(
        SquadPaths::from_env(&env).unwrap().root(),
        Path::new("/config/squad")
    );

    let env = EnvSnapshot::with_overrides([(XDG_DATA_HOME, "/xdg/data")]);
    assert_eq!(
        SquadPaths::from_env(&env).unwrap().root(),
        Path::new("/xdg/data/awman/squad")
    );
}

#[test]
fn squad_task_dir_rejects_paths_that_escape_root() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = SquadPaths::from_root(tmp.path());
    std::fs::create_dir_all(paths.tasks_dir()).unwrap();
    std::fs::create_dir_all(tmp.path().join("outside")).unwrap();

    assert!(paths.task_dir("../outside").is_err());
    assert_eq!(
        paths.task_dir("valid-name").unwrap(),
        PathBuf::from(tmp.path()).join("tasks/valid-name/workspace")
    );
}
