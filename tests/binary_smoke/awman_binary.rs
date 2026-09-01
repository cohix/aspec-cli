//! Stable resolution for subprocess tests that execute the `awman` CLI.
//!
//! Cargo publishes `CARGO_BIN_EXE_awman` with an unlink-then-relink sequence.
//! Copy the freshly built binary once before the smoke tests use it so parallel
//! subprocess launches cannot race that publication and observe `ENOENT`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub(crate) fn awman_bin() -> PathBuf {
    static SHARED: OnceLock<PathBuf> = OnceLock::new();
    SHARED.get_or_init(build_shared_awman_copy).clone()
}

fn build_shared_awman_copy() -> PathBuf {
    // Keep the directory until process exit so every test shares the same,
    // immutable executable rather than repeatedly reading Cargo's live path.
    let dir = tempfile::Builder::new()
        .prefix("awman-under-test-")
        .tempdir()
        .expect("temp dir for awman copy")
        .keep();
    let dest = dir.join("awman");

    let top_level = PathBuf::from(env!("CARGO_BIN_EXE_awman"));
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut backoff = Duration::from_millis(25);
    let mut last_copy_err = None;
    loop {
        if let Some(src) = resolve_awman_source() {
            match std::fs::copy(&src, &dest) {
                Ok(_) => break,
                Err(err) => {
                    last_copy_err = Some(format!("last copy from {}: {err:?}", src.display()));
                }
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "awman binary never became usable within 120s; top-level {} try_exists={:?}; {}",
                top_level.display(),
                top_level.try_exists(),
                last_copy_err
                    .as_deref()
                    .unwrap_or("top-level absent and no real-CLI deps/awman-<hash> found"),
            );
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_millis(500));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .expect("mark awman copy executable");
    }
    dest
}

fn resolve_awman_source() -> Option<PathBuf> {
    let top_level = PathBuf::from(env!("CARGO_BIN_EXE_awman"));
    if !matches!(top_level.try_exists(), Ok(false)) {
        return Some(top_level);
    }
    newest_real_cli_deps_binary(&top_level)
}

#[cfg(unix)]
fn newest_real_cli_deps_binary(top_level: &Path) -> Option<PathBuf> {
    use std::time::SystemTime;

    let deps = top_level.parent()?.join("deps");
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(deps).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("awman-") || name.contains('.') {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        candidates.push((
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            entry.path(),
        ));
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0));
    candidates
        .into_iter()
        .map(|(_, path)| path)
        .find(|path| probes_as_awman_cli(path))
}

#[cfg(not(unix))]
fn newest_real_cli_deps_binary(_top_level: &Path) -> Option<PathBuf> {
    None
}

fn probes_as_awman_cli(path: &Path) -> bool {
    match Command::new(path).arg("--version").output() {
        Ok(output) => output.status.success() && output.stdout.starts_with(b"awman "),
        Err(_) => false,
    }
}
