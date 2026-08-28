//! CLI parse-time safety checks for `new skill --pull` (WI 0103).

use std::process::Command;

fn awman_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_awman"))
}

/// Conflicting pull modes must be rejected by clap before main can resolve a
/// git root, construct a runtime, or invoke either external binary.  The fake
/// executables leave an on-disk marker if that invariant regresses.
#[test]
fn pull_conflicts_are_rejected_before_git_or_container_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    let marker = tmp.path().join("external-command-ran");
    std::fs::create_dir_all(&bin_dir).unwrap();

    for command in ["git", "docker"] {
        let script = bin_dir.join(command);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf invoked > '{}'\nexit 97\n",
                marker.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    for conflicting_flag in ["--interview", "--global"] {
        let output = Command::new(awman_bin())
            .env("PATH", &path)
            .env("AWMAN_CONFIG_HOME", tmp.path().join("home"))
            .args([
                "new",
                "skill",
                "--pull",
                "github.com/owner/library",
                conflicting_flag,
            ])
            .output()
            .expect("run awman with conflicting pull flags");
        assert!(
            !output.status.success(),
            "--pull {conflicting_flag} must fail at parse time"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("cannot be used with")
                || stderr.contains("conflicts with")
                || stderr.contains("--pull"),
            "parse error should name the conflicting pull flags: {stderr}"
        );
    }
    assert!(
        !marker.exists(),
        "parse rejection must happen before git/network or Docker/container work"
    );
}

// ─── Aggregate exit status and success-path output (WI-0103 remediation) ─────

fn git(dir: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Build a local `file://` remote holding `skills/<slug>/SKILL.md`, returning
/// `(source_worktree, bare_remote)`. No network access is involved.
fn make_local_remote(
    root: &std::path::Path,
    slug: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let src = root.join(format!("{slug}-src"));
    let bare = root.join(format!("{slug}.git"));
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["init", "--quiet"]);
    git(&src, &["config", "user.email", "tests@example.invalid"]);
    git(&src, &["config", "user.name", "awman binary smoke"]);
    let skill_dir = src.join("skills").join(slug);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), "# initial\n").unwrap();
    git(&src, &["add", "."]);
    git(&src, &["commit", "--quiet", "-m", "initial"]);
    git(&src, &["branch", "-M", "main"]);
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--quiet", "--bare"]);
    git(
        &src,
        &[
            "remote",
            "add",
            "origin",
            &format!("file://{}", bare.display()),
        ],
    );
    git(&src, &["push", "--quiet", "-u", "origin", "main"]);
    git(&bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    (src, bare)
}

/// Clone `bare` into `<home>/skills/.library/<slug>` and write the matching
/// `.awman.json`, i.e. the on-disk shape a prior `--pull` leaves behind.
fn preseed_library(home: &std::path::Path, slug: &str, bare: &std::path::Path) {
    let library_root = home.join("skills").join(".library");
    std::fs::create_dir_all(&library_root).unwrap();
    let url = format!("file://{}", bare.display());
    let dest = library_root.join(slug);
    let output = Command::new("git")
        .args(["clone", "--quiet", &url])
        .arg(&dest)
        .output()
        .expect("clone local remote");
    assert!(output.status.success(), "clone failed");
    std::fs::write(
        dest.join(".awman.json"),
        format!(r#"{{"source":"{url}","owner":"owner","repo":"{slug}","subdir":"skills"}}"#),
    )
    .unwrap();
}

/// `--pull-all` must refresh every reachable library even when one upstream is
/// gone, and the process must still exit non-zero so CI and scripts see the
/// failure. It must not print a skill-creation line either — nothing was
/// created and nothing is repo-scoped.
#[test]
fn pull_all_with_one_unreachable_remote_refreshes_the_rest_and_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "--quiet"]);

    let (good_src, good_bare) = make_local_remote(tmp.path(), "good");
    let (_bad_src, bad_bare) = make_local_remote(tmp.path(), "bad");
    preseed_library(&home, "good", &good_bare);
    preseed_library(&home, "bad", &bad_bare);

    // Publish a new skill upstream so a successful refresh is observable.
    let refreshed = good_src.join("skills").join("refreshed");
    std::fs::create_dir_all(&refreshed).unwrap();
    std::fs::write(refreshed.join("SKILL.md"), "# refreshed\n").unwrap();
    git(&good_src, &["add", "."]);
    git(&good_src, &["commit", "--quiet", "-m", "refreshed"]);
    git(&good_src, &["push", "--quiet", "origin", "main"]);

    // Exactly one upstream becomes unreachable.
    std::fs::rename(&bad_bare, tmp.path().join("bad.gone")).unwrap();

    let output = Command::new(awman_bin())
        .current_dir(&work)
        .env("AWMAN_CONFIG_HOME", &home)
        .args(["new", "skill", "--pull-all"])
        .output()
        .expect("run awman new skill --pull-all");

    assert_eq!(
        output.status.code(),
        Some(1),
        "a partially-failed --pull-all must exit non-zero; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        home.join("skills")
            .join(".library")
            .join("good")
            .join("skills")
            .join("refreshed")
            .join("SKILL.md")
            .is_file(),
        "the reachable library must still be refreshed"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("1 succeeded, 1 failed"),
        "output must summarise per-library results; got: {combined}"
    );
    assert!(
        !combined.contains("Created skill") && !combined.contains("Skill created"),
        "a pull must never be rendered as skill creation; got: {combined}"
    );
}

/// `--pull-all` with nothing pulled yet is informational, not an error.
#[test]
fn pull_all_with_no_libraries_is_informational_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "--quiet"]);

    let output = Command::new(awman_bin())
        .current_dir(&work)
        .env("AWMAN_CONFIG_HOME", tmp.path().join("home"))
        .args(["new", "skill", "--pull-all"])
        .output()
        .expect("run awman new skill --pull-all");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "an empty --pull-all must exit 0; got {:?}: {combined}",
        output.status.code()
    );
    assert!(
        combined.contains("no skill libraries pulled yet"),
        "output must say nothing has been pulled; got: {combined}"
    );
    assert!(
        !combined.contains("Skill created") && !combined.contains("Created skill"),
        "an empty --pull-all must not claim a skill was created; got: {combined}"
    );
}

/// Put a `docker` on `PATH` that records its own invocation, so a test can
/// prove a code path never reaches the container runtime. Returns the `PATH`
/// value to use and the marker file to check afterwards.
fn path_with_container_tripwire(tmp: &std::path::Path) -> (String, std::path::PathBuf) {
    let bin_dir = tmp.join("tripwire-bin");
    let marker = tmp.join("docker-was-invoked");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let script = bin_dir.join("docker");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf invoked > '{}'\nexit 97\n",
            marker.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (path, marker)
}

/// A fully successful refresh reports the pull and exits 0, again without a
/// skill-creation line — and, per the work item's security constraint, without
/// ever reaching the container runtime.
#[test]
fn pull_by_short_name_reports_the_refresh_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "--quiet"]);

    let (_src, bare) = make_local_remote(tmp.path(), "good");
    preseed_library(&home, "good", &bare);

    let (path, docker_marker) = path_with_container_tripwire(tmp.path());
    let output = Command::new(awman_bin())
        .current_dir(&work)
        .env("PATH", &path)
        .env("AWMAN_CONFIG_HOME", &home)
        .args(["new", "skill", "--pull", "good"])
        .output()
        .expect("run awman new skill --pull good");

    assert!(
        !docker_marker.exists(),
        "a pull is a pure host-side git operation and must never launch a container"
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "a successful refresh must exit 0; got {:?}: {combined}",
        output.status.code()
    );
    assert!(
        combined.contains("Pulled 'good' into"),
        "output must report the pull; got: {combined}"
    );
    assert!(
        !combined.contains("Created skill") && !combined.contains("Skill created"),
        "a pull must never be rendered as skill creation; got: {combined}"
    );
}
