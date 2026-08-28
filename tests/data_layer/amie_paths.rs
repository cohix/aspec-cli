//! Part 1 AmiePaths environment and containment tests.

use std::path::{Path, PathBuf};

use awman::data::config::env::{EnvSnapshot, AWMAN_AMIE_ROOT, AWMAN_CONFIG_HOME, XDG_DATA_HOME};
use awman::data::fs::AmiePaths;

#[test]
fn amie_paths_env_precedence_prefers_amie_root_then_config_then_xdg() {
    let env = EnvSnapshot::with_overrides([
        (AWMAN_AMIE_ROOT, "/explicit/amie"),
        (AWMAN_CONFIG_HOME, "/config"),
        (XDG_DATA_HOME, "/xdg/data"),
    ]);
    assert_eq!(
        AmiePaths::from_env(&env).unwrap().root(),
        Path::new("/explicit/amie")
    );

    let env =
        EnvSnapshot::with_overrides([(AWMAN_CONFIG_HOME, "/config"), (XDG_DATA_HOME, "/xdg/data")]);
    assert_eq!(
        AmiePaths::from_env(&env).unwrap().root(),
        Path::new("/config/amie")
    );

    let env = EnvSnapshot::with_overrides([(XDG_DATA_HOME, "/xdg/data")]);
    assert_eq!(
        AmiePaths::from_env(&env).unwrap().root(),
        Path::new("/xdg/data/awman/amie")
    );
}

#[test]
fn amie_condition_dir_rejects_paths_that_escape_root() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = AmiePaths::from_root(tmp.path());
    std::fs::create_dir_all(paths.conditions_dir()).unwrap();
    std::fs::create_dir_all(tmp.path().join("outside")).unwrap();

    assert!(paths.condition_dir("../outside").is_err());
    assert_eq!(
        paths.condition_dir("valid-name").unwrap(),
        PathBuf::from(tmp.path()).join("conditions/valid-name")
    );
}
