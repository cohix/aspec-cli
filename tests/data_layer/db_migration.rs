//! Part 1 shared database relocation and migration tests.

use std::path::{Path, PathBuf};

use awman::data::config::env::{EnvSnapshot, AWMAN_CONFIG_HOME, XDG_DATA_HOME};
use awman::data::config::global::GlobalConfig;
use awman::data::fs::api_db::SqliteSessionStore;
use awman::data::fs::data_paths::{DataPaths, MigrationOutcome, DB_FILENAME};

fn roots(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf, DataPaths) {
    let legacy = tmp.path().join("api");
    let data = DataPaths::at_root(tmp.path().join("data"));
    std::fs::create_dir_all(&legacy).unwrap();
    (legacy, data.root().to_path_buf(), data)
}

fn sidecar(root: &Path, suffix: &str) -> PathBuf {
    root.join(format!("{}{}", DB_FILENAME, suffix))
}

#[test]
fn data_paths_db_path_matches_global_data_home_precedence() {
    let config_home = tempfile::tempdir().unwrap();
    let xdg_data_home = tempfile::tempdir().unwrap();
    let cases = [
        EnvSnapshot::with_overrides([
            (
                AWMAN_CONFIG_HOME,
                config_home.path().to_string_lossy().to_string(),
            ),
            (
                XDG_DATA_HOME,
                xdg_data_home.path().to_string_lossy().to_string(),
            ),
        ]),
        EnvSnapshot::with_overrides([(
            XDG_DATA_HOME,
            xdg_data_home.path().to_string_lossy().to_string(),
        )]),
        EnvSnapshot::empty(),
    ];

    for env in cases {
        let expected = GlobalConfig::data_home_with(&env)
            .unwrap()
            .join("data")
            .join(DB_FILENAME);
        assert_eq!(DataPaths::from_env(&env).unwrap().db_path(), expected);
    }
}

#[test]
fn migrate_legacy_db_is_noop_for_fresh_and_already_migrated_states() {
    let tmp = tempfile::tempdir().unwrap();
    let (legacy, _, data) = roots(&tmp);

    assert_eq!(
        data.migrate_legacy_db(&legacy).unwrap(),
        MigrationOutcome::FreshInstall
    );
    std::fs::write(data.db_path(), b"live database").unwrap();
    std::fs::write(legacy.join("awman.db.pre-migration"), b"retained backup").unwrap();
    assert_eq!(
        data.migrate_legacy_db(&legacy).unwrap(),
        MigrationOutcome::AlreadyMigrated
    );
}

#[test]
fn migrate_legacy_db_copies_db_and_sidecars_then_renames_originals_aside() {
    let tmp = tempfile::tempdir().unwrap();
    let (legacy, _, data) = roots(&tmp);
    let main = b"legacy main bytes";
    let wal = b"legacy wal bytes";
    let shm = b"legacy shm bytes";
    std::fs::write(legacy.join(DB_FILENAME), main).unwrap();
    std::fs::write(sidecar(&legacy, "-wal"), wal).unwrap();
    std::fs::write(sidecar(&legacy, "-shm"), shm).unwrap();

    let outcome = data.migrate_legacy_db(&legacy).unwrap();
    assert!(matches!(outcome, MigrationOutcome::Migrated { .. }));
    assert_eq!(std::fs::read(data.db_path()).unwrap(), main);
    assert_eq!(std::fs::read(sidecar(data.root(), "-wal")).unwrap(), wal);
    assert_eq!(std::fs::read(sidecar(data.root(), "-shm")).unwrap(), shm);
    assert!(!legacy.join(DB_FILENAME).exists());
    assert_eq!(
        std::fs::read(legacy.join("awman.db.pre-migration")).unwrap(),
        main
    );
    assert_eq!(
        std::fs::read(legacy.join("awman.db-wal.pre-migration")).unwrap(),
        wal
    );
    assert_eq!(
        std::fs::read(legacy.join("awman.db-shm.pre-migration")).unwrap(),
        shm
    );
}

#[test]
fn migrate_legacy_db_success_keeps_identical_main_backup_and_sidecar_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let (legacy, _, data) = roots(&tmp);
    for (suffix, bytes) in [("", b"main".as_slice()), ("-wal", b"wal"), ("-shm", b"shm")] {
        std::fs::write(sidecar(&legacy, suffix), bytes).unwrap();
    }

    data.migrate_legacy_db(&legacy).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let original_backup = legacy.join(format!("awman.db{}.pre-migration", suffix));
        let migrated = data.root().join(format!("awman.db{}", suffix));
        assert_eq!(
            std::fs::read(original_backup).unwrap(),
            std::fs::read(migrated).unwrap()
        );
    }
    assert!(!legacy.join(DB_FILENAME).exists());
}

#[test]
fn migrate_legacy_db_preserves_session_and_command_rows_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let (legacy, _, data) = roots(&tmp);
    let legacy_store = SqliteSessionStore::open(&legacy).unwrap();
    legacy_store
        .insert_session("legacy-session", "/legacy/repo", "2026-08-27T00:00:00Z")
        .unwrap();
    legacy_store
        .insert_command(
            "legacy-command",
            "legacy-session",
            "status",
            "[\"--json\"]",
            "/legacy/output.log",
        )
        .unwrap();
    let expected_session = legacy_store.get_session("legacy-session").unwrap();
    let expected_command = legacy_store.get_command("legacy-command").unwrap();
    drop(legacy_store);

    data.migrate_legacy_db(&legacy).unwrap();
    let migrated = SqliteSessionStore::open_at(&data.db_path()).unwrap();
    assert_eq!(
        migrated.get_session("legacy-session").unwrap(),
        expected_session
    );
    assert_eq!(
        migrated.get_command("legacy-command").unwrap(),
        expected_command
    );
}

#[test]
fn migrate_legacy_db_recovers_by_discarding_interrupted_target() {
    let tmp = tempfile::tempdir().unwrap();
    let (legacy, _, data) = roots(&tmp);
    std::fs::write(legacy.join(DB_FILENAME), b"authoritative legacy").unwrap();
    data.ensure_root().unwrap();
    std::fs::write(data.db_path(), b"untrusted partial target").unwrap();
    std::fs::write(sidecar(data.root(), "-wal"), b"partial wal").unwrap();

    assert!(matches!(
        data.migrate_legacy_db(&legacy).unwrap(),
        MigrationOutcome::RecoveredFromInterrupted { .. }
    ));
    assert_eq!(
        std::fs::read(data.db_path()).unwrap(),
        b"authoritative legacy"
    );
    assert!(!sidecar(data.root(), "-wal").exists());
    assert!(!legacy.join(DB_FILENAME).exists());
}

#[test]
fn migrate_legacy_db_does_not_overwrite_preexisting_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let (legacy, _, data) = roots(&tmp);
    std::fs::write(legacy.join(DB_FILENAME), b"new source").unwrap();
    std::fs::write(
        legacy.join("awman.db.pre-migration"),
        b"older preserved backup",
    )
    .unwrap();

    let outcome = data.migrate_legacy_db(&legacy).unwrap();
    assert!(matches!(
        outcome,
        MigrationOutcome::Migrated {
            backup_kept: false,
            ..
        }
    ));
    assert_eq!(
        std::fs::read(legacy.join("awman.db.pre-migration")).unwrap(),
        b"older preserved backup"
    );
    assert!(!legacy.join(DB_FILENAME).exists());
}

#[test]
fn migrate_legacy_db_failed_copy_keeps_legacy_files_and_removes_partial_target() {
    let tmp = tempfile::tempdir().unwrap();
    let (legacy, _, data) = roots(&tmp);
    std::fs::write(legacy.join(DB_FILENAME), b"authoritative source").unwrap();
    // A directory at the sidecar source makes the second copy fail after the
    // main target has already been copied, exercising partial-target cleanup.
    std::fs::create_dir(legacy.join("awman.db-wal")).unwrap();

    let error = data
        .migrate_legacy_db(&legacy)
        .expect_err("copying a sidecar directory must fail");
    assert!(error.to_string().contains("awman.db-wal"));
    assert!(legacy.join(DB_FILENAME).exists());
    assert!(legacy.join("awman.db-wal").is_dir());
    assert!(!data.db_path().exists());
    assert!(!sidecar(data.root(), "-wal").exists());
}
