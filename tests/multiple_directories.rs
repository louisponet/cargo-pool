use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture {
    dir: PathBuf,
    repo: PathBuf,
    roots: [PathBuf; 2],
}

impl Fixture {
    fn new() -> Self {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("cargo-pool-test-{}-{stamp}", std::process::id()));
        let repo = dir.join("repo");
        fs::create_dir_all(repo.join("src")).unwrap();
        let fixture = Self { roots: [dir.join("pool one"), dir.join("pool two")], dir, repo };
        fs::write(fixture.repo.join("Cargo.toml"), "[package]\nname = \"example\"\nversion = \"0.1.0\"\n").unwrap();
        fs::write(fixture.repo.join("src/main.rs"), "fn main() {}\n").unwrap();
        fixture.git(&fixture.repo, &["init", "-q"]);
        fixture.git(&fixture.repo, &["add", "."]);
        fixture.git(&fixture.repo, &["commit", "-qm", "initial"]);
        fixture
    }

    fn command(&self, program: &str, cwd: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.current_dir(cwd)
            .env("HOME", &self.dir)
            .env("XDG_CACHE_HOME", self.dir.join("cache"))
            .env("CARGO_HOME", self.dir.join(".cargo"))
            .env("GIT_CONFIG_GLOBAL", self.dir.join(".gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_COUNT", "0")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("CARGO_POOL_MAX_DISK_PERCENT", "100")
            .env_remove("CARGO_POOL_DIR")
            .env_remove("CARGO_POOL_DIRS")
            .env_remove("CARGO_POOL_RUSTC_WRAPPER")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR");
        cmd
    }

    fn git(&self, cwd: &Path, args: &[&str]) -> String {
        success(self.command("git", cwd).args(["-c", "core.hooksPath=/dev/null"]).args(args).output().unwrap())
    }

    fn worktree(&self, name: &str) -> PathBuf {
        let path = self.dir.join(name);
        self.git(&self.repo, &["worktree", "add", "-qb", name, path.to_str().unwrap()]);
        path
    }

    fn pool(&self, cwd: &Path, roots: &[PathBuf], args: &[&str]) -> String {
        success(
            self.command(env!("CARGO_BIN_EXE_cargo-pool"), cwd)
                .env("CARGO_POOL_DIRS", std::env::join_paths(roots).unwrap())
                .args(args)
                .output()
                .unwrap(),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for root in &self.roots {
            if !root.starts_with(&self.dir) && root.exists() {
                fs::remove_dir_all(root).unwrap();
            }
        }
        fs::remove_dir_all(&self.dir).unwrap();
    }
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn reuses_slots_and_preserves_attachments_across_directories() {
    let f = Fixture::new();
    let a = f.worktree("a");
    let b = f.worktree("b");
    let c = f.worktree("c");
    f.pool(&a, &f.roots[..1], &["attach", "--no-seed"]);
    let original = fs::read_link(a.join("target")).unwrap();
    f.pool(&a, &f.roots[..1], &["detach"]);
    f.pool(&b, &f.roots[1..], &["attach", "--no-seed"]);
    let other = fs::read_link(b.join("target")).unwrap();
    f.pool(&c, &f.roots, &["attach", "--no-seed"]);
    assert_eq!(fs::read_link(c.join("target")).unwrap(), original);
    f.pool(&b, &f.roots, &["attach"]);
    assert_eq!(fs::read_link(b.join("target")).unwrap(), other);
    let status = f.pool(&b, &f.roots, &["status"]);
    for root in &f.roots {
        assert!(status.contains(root.to_str().unwrap()));
    }
    let status = f.pool(&f.dir, &f.roots, &["status"]);
    assert!(status.contains("pool one") && status.contains("pool two"));
    for wt in [&b, &c] {
        f.pool(wt, &f.roots, &["detach"]);
    }
    for target in [&original, &other] {
        let meta = target.parent().unwrap().join("slot.json");
        let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
        json["last_used"] = 0.into();
        fs::write(meta, serde_json::to_vec(&json).unwrap()).unwrap();
    }
    f.pool(&b, &f.roots, &["gc", "--max-age-days", "0", "--dry-run"]);
    assert!(original.exists() && other.exists());
    f.pool(&b, &f.roots, &["gc", "--max-age-days", "0"]);
    assert!(!original.exists() && !other.exists());
}

#[test]
fn seeds_from_another_directory_and_tracks_builds_there() {
    let f = Fixture::new();
    let a = f.worktree("a");
    let b = f.worktree("b");
    f.pool(&a, &f.roots[..1], &["attach", "--no-seed"]);
    fs::write(a.join("target/cached-artifact"), "warm").unwrap();
    let triple = "x86_64-unknown-linux-gnu/debug";
    for dir in [
        "debug/.fingerprint",
        "debug/incremental/example-0123",
        "debug/deps",
        "wasm/debug/incremental",
        &format!("{triple}/.fingerprint"),
        &format!("{triple}/incremental"),
    ] {
        fs::create_dir_all(a.join("target").join(dir)).unwrap();
    }
    fs::write(a.join("target/debug/deps/example-0123"), "bin").unwrap();
    fs::hard_link(a.join("target/debug/deps/example-0123"), a.join("target/debug/example")).unwrap();
    fs::create_dir(a.join("target/.rustc_info.json")).unwrap();
    f.pool(&b, &f.roots[1..], &["attach"]);
    let target = fs::read_link(b.join("target")).unwrap();
    f.pool(&b, &f.roots, &["/bin/true", "-vV"]);
    assert_eq!(fs::read_to_string(target.join("cached-artifact")).unwrap(), "warm");
    assert!(!target.join(".rustc_info.json").is_dir(), "a failed probe's guard must not disable another slot's cache");
    assert!(target.join("debug/.fingerprint").exists() && target.join(triple).join(".fingerprint").exists());
    assert!(!target.join("debug/incremental").exists(), "incremental caches are not seeded");
    assert!(!target.join(triple).join("incremental").exists(), "nor under a target triple");
    assert!(target.join("wasm/debug/incremental").exists(), "a dir without fingerprints is copied whole");
    let ino = |p: &str| fs::metadata(target.join(p)).unwrap().ino();
    assert_eq!(ino("debug/example"), ino("debug/deps/example-0123"), "hard links survive the copy");
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(target.parent().unwrap().join("slot.json")).unwrap()).unwrap();
    assert_eq!(metadata["seed_pending"], false);
    let out = target.join("debug/deps");
    fs::create_dir_all(&out).unwrap();
    let lock = fs::File::create(target.parent().unwrap().join("snapshot.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    f.pool(&b, &f.roots, &["/bin/true", "--out-dir", out.to_str().unwrap()]);
    assert!(target.parent().unwrap().join("last-compile").exists());
}

#[test]
fn global_directories_and_legacy_environment_override() {
    let f = Fixture::new();
    let a = f.worktree("a");
    let b = f.worktree("b");
    let local_config = f.dir.join(".gitconfig.local");
    f.git(&f.repo, &["config", "--global", "include.path", local_config.to_str().unwrap()]);
    for root in &f.roots {
        f.git(
            &f.repo,
            &[
                "config",
                "--file",
                local_config.to_str().unwrap(),
                "--add",
                "cargo-pool.directory",
                root.to_str().unwrap(),
            ],
        );
    }
    let run = |cwd: &Path, single: Option<&Path>, multiple: Option<&[PathBuf]>| {
        let mut cmd = f.command(env!("CARGO_BIN_EXE_cargo-pool"), cwd);
        if let Some(root) = single {
            cmd.env("CARGO_POOL_DIR", root);
        }
        if let Some(roots) = multiple {
            cmd.env("CARGO_POOL_DIRS", std::env::join_paths(roots).unwrap());
        }
        success(cmd.args(["attach", "--no-seed"]).output().unwrap())
    };
    run(&a, Some(&f.roots[0]), None);
    assert!(fs::read_link(a.join("target")).unwrap().starts_with(&f.roots[0]));
    run(&b, Some(&f.roots[0]), Some(&f.roots[1..]));
    assert!(fs::read_link(b.join("target")).unwrap().starts_with(&f.roots[1]));
    let status = success(f.command(env!("CARGO_BIN_EXE_cargo-pool"), &a).arg("status").output().unwrap());
    assert!(status.contains("pool one") && status.contains("pool two"));
}

#[test]
fn allocates_by_available_space_and_cleans_only_the_target_filesystem() {
    let mut f = Fixture::new();
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
        eprintln!("requires /dev/shm on a separate filesystem");
        return;
    }
    f.roots[1] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
    for root in &f.roots {
        fs::create_dir_all(root).unwrap();
    }
    let available = |path: &Path| {
        let output = success(Command::new("df").args(["-B1", "--output=avail"]).arg(path).output().unwrap());
        output.lines().last().unwrap().trim().parse::<u64>().unwrap()
    };
    if available(&f.roots[0]) > available(&f.roots[1]) {
        f.roots.reverse();
    }
    let a = f.worktree("a");
    f.pool(&a, &f.roots, &["attach", "--no-seed"]);
    let a_target = fs::read_link(a.join("target")).unwrap();
    assert!(a_target.starts_with(&f.roots[1]), "new slot should use the filesystem with more free space");
    let b = f.worktree("b");
    f.pool(&b, &f.roots[..1], &["attach", "--no-seed"]);
    let b_target = fs::read_link(b.join("target")).unwrap();
    f.pool(&b, &f.roots, &["detach"]);
    let c = f.worktree("c");
    f.pool(&c, &f.roots[1..], &["attach", "--no-seed"]);
    let c_target = fs::read_link(c.join("target")).unwrap();
    f.pool(&c, &f.roots, &["detach"]);
    let out = a_target.join("debug/deps");
    fs::create_dir_all(&out).unwrap();
    let lock = fs::File::create(a_target.parent().unwrap().join("snapshot.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    success(
        f.command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
            .env("CARGO_POOL_DIRS", std::env::join_paths(&f.roots).unwrap())
            .env("CARGO_POOL_MAX_DISK_PERCENT", "0")
            .args(["/bin/true", "--out-dir", out.to_str().unwrap()])
            .output()
            .unwrap(),
    );
    assert!(!c_target.exists(), "cleanup should remove the free slot on the target filesystem");
    assert!(b_target.exists(), "cleanup should preserve slots on another filesystem");
    assert!(a_target.exists(), "cleanup should preserve the active slot");
}

#[test]
fn default_pool_still_uses_xdg_cache_home() {
    let f = Fixture::new();
    let a = f.worktree("a");
    success(f.command(env!("CARGO_BIN_EXE_cargo-pool"), &a).args(["attach", "--no-seed"]).output().unwrap());
    assert!(fs::read_link(a.join("target")).unwrap().starts_with(f.dir.join("cache/cargo-pool")));
}

#[test]
fn records_metrics() {
    let f = Fixture::new();
    let roots = &f.roots[..1];
    let a = f.worktree("a");
    let b = f.worktree("b");
    f.pool(&a, roots, &["attach", "--no-seed"]);
    fs::write(a.join("target/cached-artifact"), "warm").unwrap();
    f.pool(&b, roots, &["attach"]);
    f.pool(&b, roots, &["/bin/true", "-vV"]);
    let target = fs::read_link(b.join("target")).unwrap();
    let slot = target.parent().unwrap();
    let out = target.join("debug/deps");
    fs::create_dir_all(&out).unwrap();
    {
        // Keep the wrapper from spawning the background waiter; run it ourselves below.
        let lock = fs::File::create(slot.join("snapshot.lock")).unwrap();
        assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
        for _ in 0..2 {
            f.pool(&b, roots, &["/bin/true", "--out-dir", out.to_str().unwrap()]);
        }
    }
    f.pool(&b, roots, &["snapshot-after-build", slot.to_str().unwrap()]);
    f.pool(&a, roots, &["detach"]);
    let c = f.worktree("c");
    f.pool(&c, roots, &["attach"]);
    let stats = f.pool(&c, roots, &["stats"]);
    for expected in [
        "2 slots, 2 in use",
        "attached   3 worktrees: 1 to a warm slot, 2 to a new one",
        "seeded     1 of 1 new slots",
        "prepared   3 slots",
        "built      1 cargo commands, 2 units compiled",
        "snapshots  1 in the background",
    ] {
        assert!(stats.contains(expected), "missing {expected:?} in:\n{stats}");
    }
    assert!(f.pool(&c, roots, &["stats", "--days", "1"]).contains("attached   3 worktrees"));
}

#[test]
fn hook_leases_on_the_first_cargo_command_and_eviction_rearms_it() {
    let f = Fixture::new();
    let roots = &f.roots[..1];
    f.pool(&f.repo, roots, &["install-wrapper"]);
    let a = f.worktree("a");
    f.pool(&a, roots, &["attach", "--no-seed"]);
    fs::write(a.join("target/cached-artifact"), "warm").unwrap();
    let warm = fs::read_link(a.join("target")).unwrap();
    f.pool(&a, roots, &["detach"]);

    let b = f.worktree("b");
    let head = f.git(&b, &["rev-parse", "HEAD"]);
    f.pool(&b, roots, &["post-checkout", "0000000000000000000000000000000000000000", head.trim(), "1"]);
    let placeholder = fs::symlink_metadata(b.join("target")).unwrap();
    assert!(placeholder.is_dir() && fs::read_dir(b.join("target")).unwrap().next().is_none(), "the hook must not lease a slot");
    assert!(f.pool(&b, roots, &["status"]).contains("free"));

    // Cargo parses the probe's stdout: attaching must not write to it.
    assert_eq!(f.pool(&b, roots, &["/bin/true", "-vV"]), "");
    assert_eq!(fs::read_link(b.join("target")).unwrap(), warm);
    assert_eq!(fs::read_to_string(b.join("target/cached-artifact")).unwrap(), "warm");

    success(
        f.command(env!("CARGO_BIN_EXE_cargo-pool"), &b)
            .env("CARGO_POOL_DIRS", std::env::join_paths(roots).unwrap())
            .env("CARGO_POOL_MAX_DISK_PERCENT", "0")
            .arg("gc")
            .output()
            .unwrap(),
    );
    assert!(!warm.exists(), "held slots are evicted under disk pressure too");
    assert!(fs::symlink_metadata(b.join("target")).unwrap().is_dir());
    fs::create_dir_all(b.join("target/flycheck0")).unwrap();
    fs::write(b.join("target/flycheck0/stdout"), "after eviction").unwrap();
    f.pool(&b, roots, &["/bin/true", "-vV"]);
    assert!(fs::read_link(b.join("target")).unwrap().exists(), "an evicted worktree re-attaches on its next build");
    assert_eq!(fs::read_to_string(b.join("target/flycheck0/stdout")).unwrap(), "after eviction");
}

#[test]
fn deferred_attach_preserves_stray_files_with_warm_reuse_and_seeding() {
    use std::io::Write;
    let f = Fixture::new();
    let roots = &f.roots[..1];
    f.pool(&f.repo, roots, &["install-wrapper"]);
    let a = f.worktree("a");
    f.pool(&a, roots, &["attach", "--no-seed"]);
    fs::write(a.join("target/value-tests.log"), "old log").unwrap();
    fs::write(a.join("target/cached-artifact"), "warm").unwrap();
    f.pool(&a, roots, &["detach"]);
    for name in ["reuse", "seed"] {
        let b = f.worktree(name);
        f.pool(&b, roots, &["attach", "--from-hook"]);
        fs::create_dir_all(b.join("target/flycheck0")).unwrap();
        fs::write(b.join("target/flycheck0/stdout"), "flycheck").unwrap();
        let mut log = fs::File::create(b.join("target/value-tests.log")).unwrap();
        log.write_all(b"before").unwrap();
        f.pool(&b, roots, &["/bin/true", "-vV"]);
        assert!(fs::read_link(b.join("target")).is_ok());
        log.write_all(b" after").unwrap();
        assert_eq!(fs::read_to_string(b.join("target/value-tests.log")).unwrap(), "before after");
        assert_eq!(fs::read_to_string(b.join("target/flycheck0/stdout")).unwrap(), "flycheck");
        assert_eq!(fs::read_to_string(b.join("target/cached-artifact")).unwrap(), "warm");
        assert!(
            fs::read_dir(b.join("target"))
                .unwrap()
                .flatten()
                .any(|e| { fs::read_to_string(e.path()).is_ok_and(|s| s == "old log") }),
            "a conflicting warm file must be preserved too"
        );
    }
}

#[test]
fn failed_deferred_attach_keeps_marker_and_retries() {
    let f = Fixture::new();
    let roots = &f.roots[..1];
    f.pool(&f.repo, roots, &["install-wrapper"]);
    let a = f.worktree("a");
    f.pool(&a, roots, &["attach", "--from-hook"]);
    let marker = PathBuf::from(f.git(&a, &["rev-parse", "--absolute-git-dir"]).trim()).join("cargo-pool-attach");
    let blocked = f.dir.join("not-a-directory");
    fs::write(&blocked, "blocked").unwrap();
    let output = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIR", &blocked)
        .args(["/bin/true", "-vV"])
        .output()
        .unwrap();
    assert!(!output.status.success(), "an attach failure must not let cargo build in place");
    assert!(marker.exists(), "failed attach must remain retryable");
    f.pool(&a, roots, &["/bin/true", "-vV"]);
    assert!(fs::read_link(a.join("target")).is_ok());
    assert!(!marker.exists());
}

#[test]
fn adopts_main_target_and_reuses_its_artifacts() {
    let f = Fixture::new();
    let roots = &f.roots[..1];
    fs::create_dir_all(f.repo.join("target/debug/deps")).unwrap();
    fs::write(f.repo.join("target/debug/.cargo-lock"), "").unwrap();
    fs::write(f.repo.join("target/debug/deps/artifact"), "existing build").unwrap();
    let inode = fs::metadata(f.repo.join("target/debug/deps/artifact")).unwrap().ino();
    f.pool(&f.repo, roots, &["attach"]);
    let target = fs::read_link(f.repo.join("target")).unwrap();
    assert_eq!(fs::metadata(target.join("debug/deps/artifact")).unwrap().ino(), inode);
    f.pool(&f.repo, roots, &["detach"]);
    let a = f.worktree("a");
    f.pool(&a, roots, &["attach"]);
    assert_eq!(fs::read_link(a.join("target")).unwrap(), target);
    assert_eq!(fs::read_to_string(a.join("target/debug/deps/artifact")).unwrap(), "existing build");
}

#[test]
fn adoption_refuses_a_running_build() {
    let f = Fixture::new();
    let a = f.worktree("a");
    fs::create_dir_all(a.join("target/debug/deps")).unwrap();
    let lock = fs::File::create(a.join("target/debug/.cargo-lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let output = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIR", &f.roots[0])
        .arg("attach")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("busy"));
    assert!(fs::symlink_metadata(a.join("target")).unwrap().is_dir());
    let marker = PathBuf::from(f.git(&a, &["rev-parse", "--absolute-git-dir"]).trim()).join("cargo-pool-attach");
    fs::write(&marker, "").unwrap();
    let mut probe = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIR", &f.roots[0])
        .args(["/bin/true", "-vV"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let waiting = probe.try_wait().unwrap().is_none();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) }, 0);
    let output = probe.wait_with_output().unwrap();
    assert!(waiting, "the compiler probe must wait for an unwrapped build instead of failing it");
    success(output);
    assert!(!marker.exists());
}

#[test]
fn adopted_cargo_build_stays_fresh_in_another_worktree() {
    for cross_device in [false, true] {
        let mut f = Fixture::new();
        if cross_device {
            if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
                continue;
            }
            f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
        }
        fs::write(
            f.repo.join("build.rs"),
            r#"fn main() {
            println!("cargo:rerun-if-changed=build.rs");
            eprintln!("build script output");
            let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
            std::fs::write(out.join("payload.txt"), "generated").unwrap();
            let _ = std::fs::remove_file(out.join("config.log"));
            std::os::unix::fs::symlink(out.join("payload.txt"), out.join("config.log")).unwrap();
        }"#,
        )
        .unwrap();
        fs::write(
            f.repo.join("src/main.rs"),
            r#"fn main() { println!("{}", include_str!(concat!(env!("OUT_DIR"), "/config.log"))); }"#,
        )
        .unwrap();
        f.git(&f.repo, &["add", "build.rs", "src/main.rs"]);
        f.git(&f.repo, &["commit", "-qm", "add build script"]);
        let roots = &f.roots[..1];
        let source = if cross_device { f.worktree("source") } else { f.repo.clone() };
        let cargo = success(Command::new("rustup").args(["which", "cargo"]).output().unwrap());
        let rustc = success(Command::new("rustup").args(["which", "rustc"]).output().unwrap());
        success(
            f.command(cargo.trim(), &source)
                .env("RUSTC", rustc.trim())
                .env("RUSTC_WRAPPER", "")
                .arg("build")
                .output()
                .unwrap(),
        );
        let rustdoc = Path::new(rustc.trim()).with_file_name("rustdoc");
        success(
            f.command(cargo.trim(), &source)
                .env("RUSTC", rustc.trim())
                .env("RUSTDOC", &rustdoc)
                .env("RUSTC_WRAPPER", "")
                .args(["doc", "--no-deps"])
                .output()
                .unwrap(),
        );
        f.pool(&source, roots, &["attach"]);
        let inode = fs::metadata(source.join("target/debug/example")).unwrap().ino();
        f.pool(&source, roots, &["detach"]);
        if cross_device {
            f.git(&f.repo, &["worktree", "remove", "--force", source.to_str().unwrap()]);
        }
        let a = f.worktree("a");
        f.pool(&a, roots, &["attach"]);
        let output = f
            .command(cargo.trim(), &a)
            .env("RUSTC", rustc.trim())
            .env("RUSTC_WRAPPER", "")
            .args(["build", "--verbose"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Fresh example"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::metadata(a.join("target/debug/example")).unwrap().ino(), inode);
        success(
            f.command(cargo.trim(), &a)
                .env("RUSTC", rustc.trim())
                .env("RUSTDOC", &rustdoc)
                .env("RUSTC_WRAPPER", "")
                .args(["doc", "--no-deps"])
                .output()
                .unwrap(),
        );
        fs::write(
            a.join("src/main.rs"),
            r#"fn main() { println!("changed {}", include_str!(concat!(env!("OUT_DIR"), "/config.log"))); }"#,
        )
        .unwrap();
        success(
            f.command(cargo.trim(), &a)
                .env("RUSTC", rustc.trim())
                .env("RUSTC_WRAPPER", "")
                .arg("build")
                .output()
                .unwrap(),
        );
        fs::write(a.join("build.rs"), "fn main() { eprintln!(\"changed build script\"); }\n").unwrap();
        success(
            f.command(cargo.trim(), &a)
                .env("RUSTC", rustc.trim())
                .env("RUSTC_WRAPPER", "")
                .arg("build")
                .output()
                .unwrap(),
        );
        if !cross_device {
            success(f.command(a.join("target/debug/example").to_str().unwrap(), &a).output().unwrap());
        }
    }
}

#[test]
fn adoption_across_filesystems_preserves_artifacts_and_checks_capacity() {
    let mut f = Fixture::new();
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
        return;
    }
    f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
    for name in ["fit", "no-room"] {
        let a = f.worktree(name);
        fs::create_dir_all(a.join("target/debug/deps")).unwrap();
        fs::write(a.join("target/debug/.cargo-lock"), "").unwrap();
        fs::write(a.join("target/debug/deps/artifact"), "keep me").unwrap();
        let output = f
            .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
            .env("CARGO_POOL_DIR", &f.roots[0])
            .env("CARGO_POOL_MAX_DISK_PERCENT", if name == "fit" { "100" } else { "0" })
            .arg("attach")
            .output()
            .unwrap();
        if name == "fit" {
            success(output);
            assert!(fs::read_link(a.join("target")).unwrap().starts_with(&f.roots[0]));
        } else {
            assert!(!output.status.success());
            assert!(fs::symlink_metadata(a.join("target")).unwrap().is_dir());
        }
        assert_eq!(fs::read_to_string(a.join("target/debug/deps/artifact")).unwrap(), "keep me");
    }
}

#[test]
fn cross_device_placeholders_attach_on_the_first_probe() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
        return;
    }
    f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
    let roots = &f.roots[..1];
    f.pool(&f.repo, roots, &["install-wrapper"]);
    for name in ["empty", "logs", "empty-doc", "doc-log"] {
        let a = f.worktree(name);
        f.pool(&a, roots, &["attach", "--from-hook"]);
        let mut log = if name == "logs" {
            fs::create_dir_all(a.join("target/flycheck0")).unwrap();
            fs::write(a.join("target/flycheck0/stdout"), "flycheck").unwrap();
            let mut log = fs::File::create(a.join("target/tests.log")).unwrap();
            log.write_all(b"before").unwrap();
            Some(log)
        } else {
            None
        };
        if name == "logs" {
            let bin = f.dir.join("bin");
            fs::create_dir(&bin).unwrap();
            fs::write(bin.join("cp"), "#!/bin/sh\nexit 1\n").unwrap();
            fs::set_permissions(bin.join("cp"), fs::Permissions::from_mode(0o755)).unwrap();
            let path = std::env::join_paths(
                std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap();
            let failed = f
                .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
                .env("CARGO_POOL_DIRS", std::env::join_paths(roots).unwrap())
                .env("PATH", path)
                .args(["/bin/true", "-vV"])
                .output()
                .unwrap();
            assert!(!failed.status.success());
            assert_eq!(fs::read_to_string(a.join("target/tests.log")).unwrap(), "before");
            fs::create_dir_all(a.join("target/debug/deps")).unwrap();
            fs::write(a.join("target/debug/.cargo-lock"), "").unwrap();
            fs::write(a.join("target/debug/deps/artifact"), "built after failure").unwrap();
        }
        if name == "empty-doc" || name == "doc-log" {
            fs::create_dir_all(a.join("target/doc")).unwrap();
            if name == "doc-log" {
                fs::write(a.join("target/doc/stdout"), "documentation log").unwrap();
            }
        }
        f.pool(&a, roots, &["/bin/true", "-vV"]);
        assert!(fs::read_link(a.join("target")).unwrap().starts_with(&f.roots[0]));
        if name == "doc-log" {
            assert_eq!(fs::read_to_string(a.join("target/doc/stdout")).unwrap(), "documentation log");
        }
        if let Some(log) = &mut log {
            log.write_all(b" after").unwrap();
            assert_eq!(fs::read_to_string(a.join("target/tests.log")).unwrap(), "before after");
            assert_eq!(fs::read_to_string(a.join("target/flycheck0/stdout")).unwrap(), "flycheck");
            assert_eq!(fs::read_to_string(a.join("target/debug/deps/artifact")).unwrap(), "built after failure");
        }
    }
}

#[test]
fn failed_adoption_retains_ownership_and_strays_after_the_pending_timeout() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
        return;
    }
    f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
    let roots = &f.roots[..1];
    let a = f.worktree("a");
    fs::create_dir_all(a.join("target/debug/deps")).unwrap();
    fs::write(a.join("target/debug/.cargo-lock"), "").unwrap();
    fs::write(a.join("target/debug/deps/artifact"), "cargo output").unwrap();
    let mut log = fs::File::create(a.join("target/tests.log")).unwrap();
    log.write_all(b"before").unwrap();
    let bin = f.dir.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(
        bin.join("cp"),
        "#!/bin/sh\nfor dest; do :; done\nmkdir -p \"$dest\"\necho partial > \"$dest/partial\"\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(bin.join("cp"), fs::Permissions::from_mode(0o755)).unwrap();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())))
            .unwrap();
    let failed = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIRS", std::env::join_paths(roots).unwrap())
        .env("PATH", path)
        .arg("attach")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("original target kept"));
    assert_eq!(fs::read_to_string(a.join("target/tests.log")).unwrap(), "before");
    let pool = fs::read_dir(&f.roots[0]).unwrap().next().unwrap().unwrap().path();
    let slot = pool.join("slots/0");
    assert!(!slot.join("target.adopting").exists(), "a failed copy must discard its partial output");
    let meta = slot.join("slot.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
    json["lease"]["acquired_at"] = 0.into();
    fs::write(&meta, serde_json::to_vec(&json).unwrap()).unwrap();
    let marker = PathBuf::from(f.git(&a, &["rev-parse", "--absolute-git-dir"]).trim()).join("cargo-pool-attach");
    fs::write(&marker, "").unwrap();
    assert!(marker.exists());
    let forced = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIRS", std::env::join_paths(roots).unwrap())
        .args(["attach", "--force"])
        .output()
        .unwrap();
    assert!(!forced.status.success());
    assert!(a.join("target/debug/deps/artifact").exists(), "force must refuse before deleting an adoption source");
    let b = f.worktree("b");
    f.pool(&b, roots, &["attach", "--no-seed"]);
    assert_ne!(fs::read_link(b.join("target")).unwrap(), slot.join("target"));
    f.pool(&a, roots, &["attach"]);
    assert_eq!(fs::read_link(a.join("target")).unwrap(), slot.join("target"));
    log.write_all(b" after").unwrap();
    assert_eq!(fs::read_to_string(a.join("target/tests.log")).unwrap(), "before after");
    assert_eq!(fs::read_to_string(a.join("target/debug/deps/artifact")).unwrap(), "cargo output");
}

#[test]
fn detach_can_cancel_an_adoption_before_its_copy_finishes() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
        return;
    }
    f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
    let a = f.worktree("a");
    fs::create_dir_all(a.join("target/debug/deps")).unwrap();
    fs::write(a.join("target/debug/.cargo-lock"), "").unwrap();
    fs::write(a.join("target/debug/deps/artifact"), "warm").unwrap();
    let mut log = fs::File::create(a.join("target/tests.log")).unwrap();
    log.write_all(b"before").unwrap();
    let bin = f.dir.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("cp"), "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(bin.join("cp"), fs::Permissions::from_mode(0o755)).unwrap();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())))
            .unwrap();
    let failed = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIR", &f.roots[0])
        .env("PATH", path)
        .arg("attach")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let marker = PathBuf::from(f.git(&a, &["rev-parse", "--absolute-git-dir"]).trim()).join("cargo-pool-attach");
    fs::write(&marker, "").unwrap();
    fs::create_dir(a.join("target/.rustc_info.json")).unwrap();
    f.pool(&a, &f.roots[..1], &["detach"]);
    assert!(!marker.exists());
    assert!(!a.join("target/.rustc_info.json").exists(), "cancellation must restore Cargo's probe cache");
    assert!(fs::symlink_metadata(a.join("target")).unwrap().is_dir());
    assert_eq!(fs::read_to_string(a.join("target/debug/deps/artifact")).unwrap(), "warm");
    log.write_all(b" after").unwrap();
    assert_eq!(fs::read_to_string(a.join("target/tests.log")).unwrap(), "before after");
    fs::create_dir(f.repo.join("target")).unwrap();
    fs::write(f.repo.join("target/cached-artifact"), "warm seed").unwrap();
    let b = f.worktree("b");
    f.pool(&b, &f.roots[..1], &["attach"]);
    f.pool(&b, &f.roots[..1], &["/bin/true", "-vV"]);
    assert_eq!(fs::read_to_string(b.join("target/cached-artifact")).unwrap(), "warm seed");
    assert!(!b.join("target/tests.log").exists());
}

#[test]
fn attach_preserves_open_logs_and_relative_links() {
    for cross_device in [false, true] {
        use std::io::Write;
        let mut f = Fixture::new();
        if cross_device {
            if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
                continue;
            }
            f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
        }
        let a = f.worktree("a");
        fs::create_dir_all(a.join("target/debug/deps")).unwrap();
        fs::write(a.join("target/debug/.cargo-lock"), "").unwrap();
        fs::write(a.join("target/debug/deps/artifact"), "cargo output").unwrap();
        fs::write(f.dir.join("external.log"), "external").unwrap();
        std::os::unix::fs::symlink("../../external.log", a.join("target/relative.log")).unwrap();
        std::os::unix::fs::symlink("../../future/out.log", a.join("target/later.log")).unwrap();
        fs::create_dir_all(a.join("target/flycheck0")).unwrap();
        std::os::unix::fs::symlink("../../../external.log", a.join("target/flycheck0/relative.log")).unwrap();
        let paths = [
            "tests.log",
            "debug/results.txt",
            "debug/tests.log",
            "debug/deps/tests.log",
            "debug/build/example/out/stdout",
        ];
        let mut logs: Vec<_> = paths
            .iter()
            .map(|p| {
                let path = a.join("target").join(p);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                let mut log = fs::File::create(path).unwrap();
                log.write_all(b"before").unwrap();
                log
            })
            .collect();
        let out = a.join("target/debug/build/example/out");
        fs::write(out.join("payload.txt"), "payload").unwrap();
        std::os::unix::fs::symlink(out.join("payload.txt"), out.join("generated.txt")).unwrap();
        std::os::unix::fs::symlink(f.dir.join("external.log"), out.join("external.txt")).unwrap();
        f.pool(&a, &f.roots[..1], &["attach", "--no-seed"]);
        for (path, log) in paths.iter().zip(&mut logs) {
            log.write_all(b" after").unwrap();
            assert_eq!(fs::read_to_string(a.join("target").join(path)).unwrap(), "before after");
        }
        assert_eq!(fs::read_to_string(a.join("target/relative.log")).unwrap(), "external");
        fs::create_dir(f.dir.join("future")).unwrap();
        fs::write(f.dir.join("future/out.log"), "created later").unwrap();
        assert_eq!(fs::read_to_string(a.join("target/later.log")).unwrap(), "created later");
        assert_eq!(fs::read_to_string(a.join("target/flycheck0/relative.log")).unwrap(), "external");
        assert!(f.git(&a, &["status", "--porcelain"]).is_empty());
        assert!(
            !fs::read_dir(a.join("target"))
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains("cargo-pool-saved")),
            "a retained directory must not leave a conflict copy behind"
        );
        f.pool(&a, &f.roots[..1], &["detach"]);
        f.git(&f.repo, &["worktree", "remove", "--force", a.to_str().unwrap()]);
        let b = f.worktree("b");
        f.pool(&b, &f.roots[..1], &["attach", "--no-seed"]);
        let out = b.join("target/debug/build/example/out");
        assert_eq!(fs::read_to_string(out.join("generated.txt")).unwrap(), "payload");
        assert_eq!(fs::read_to_string(out.join("external.txt")).unwrap(), "external");
    }
}

#[test]
fn disk_pressure_can_evict_the_slot_that_attach_would_reuse() {
    for abandoned in [false, true] {
        let f = Fixture::new();
        let a = f.worktree("a");
        f.pool(&a, &f.roots[..1], &["attach", "--no-seed"]);
        fs::write(a.join("target/artifact"), vec![1; 4 << 20]).unwrap();
        if abandoned {
            let target = fs::read_link(a.join("target")).unwrap();
            let slot = target.parent().unwrap();
            fs::create_dir(slot.join("target.adopting")).unwrap();
            fs::rename(target.join("artifact"), slot.join("target.adopting/artifact")).unwrap();
            let meta = slot.join("slot.json");
            let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
            json["lease"]["pending"] = true.into();
            json["lease"]["acquired_at"] = 0.into();
            json["adoption"] = "copying".into();
            fs::write(meta, serde_json::to_vec(&json).unwrap()).unwrap();
            f.git(&f.repo, &["worktree", "remove", "--force", a.to_str().unwrap()]);
        } else {
            f.pool(&a, &f.roots[..1], &["detach"]);
        }
        let path = std::ffi::CString::new(f.roots[0].as_os_str().as_encoded_bytes()).unwrap();
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::statvfs(path.as_ptr(), &mut st) }, 0);
        let used = (st.f_blocks - st.f_bfree) * st.f_frsize;
        let avail = st.f_bavail * st.f_frsize;
        let limit = 100.0 * (used - (1 << 20)) as f64 / (used + avail) as f64;
        let b = f.worktree("b");
        success(
            f.command(env!("CARGO_BIN_EXE_cargo-pool"), &b)
                .env("CARGO_POOL_DIR", &f.roots[0])
                .env("CARGO_POOL_MAX_DISK_PERCENT", limit.to_string())
                .args(["attach", "--no-seed"])
                .output()
                .unwrap(),
        );
        assert!(fs::read_link(b.join("target")).is_ok());
    }
}

#[test]
fn seeding_never_evicts_a_held_slot() {
    let f = Fixture::new();
    let a = f.worktree("a");
    let b = f.worktree("b");
    let c = f.worktree("c");
    for wt in [&a, &c] {
        f.pool(wt, &f.roots[..1], &["attach", "--no-seed"]);
        fs::write(wt.join("target/artifact"), vec![1; 8 << 20]).unwrap();
    }
    let held = fs::read_link(c.join("target")).unwrap();
    f.pool(&b, &f.roots[..1], &["attach"]);
    let path = std::ffi::CString::new(f.roots[0].as_os_str().as_encoded_bytes()).unwrap();
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::statvfs(path.as_ptr(), &mut st) }, 0);
    let used = (st.f_blocks - st.f_bfree) * st.f_frsize;
    let avail = st.f_bavail * st.f_frsize;
    // Room for half a copy: it would fit only by deleting another worktree's slot.
    let limit = 100.0 * (used + (4 << 20)) as f64 / (used + avail) as f64;
    success(
        f.command(env!("CARGO_BIN_EXE_cargo-pool"), &b)
            .env("CARGO_POOL_DIRS", &f.roots[0])
            .env("CARGO_POOL_MAX_DISK_PERCENT", limit.to_string())
            .args(["/bin/true", "-vV"])
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read_link(c.join("target")).unwrap(), held);
    assert!(held.join("artifact").exists());
    let target = fs::read_link(b.join("target")).unwrap();
    assert!(target.is_dir() && !target.join("artifact").exists(), "the slot starts empty");
}

#[test]
fn adoption_capacity_excludes_strays_kept_on_the_source_filesystem() {
    let mut f = Fixture::new();
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
        return;
    }
    f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
    let a = f.worktree("a");
    fs::create_dir_all(a.join("target/debug/deps")).unwrap();
    fs::write(a.join("target/debug/.cargo-lock"), "").unwrap();
    fs::write(a.join("target/debug/deps/artifact"), "cargo output").unwrap();
    fs::write(a.join("target/tests.log"), vec![1; 32 << 20]).unwrap();
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::statvfs(c"/dev/shm".as_ptr(), &mut st) }, 0);
    let used = (st.f_blocks - st.f_bfree) * st.f_frsize;
    let avail = st.f_bavail * st.f_frsize;
    let limit = 100.0 * (used + (16 << 20)) as f64 / (used + avail) as f64;
    success(
        f.command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
            .env("CARGO_POOL_DIR", &f.roots[0])
            .env("CARGO_POOL_MAX_DISK_PERCENT", limit.to_string())
            .arg("attach")
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read(a.join("target/tests.log")).unwrap(), vec![1; 32 << 20]);
}

#[test]
fn recovers_a_linked_pending_lease() {
    let f = Fixture::new();
    let a = f.worktree("a");
    f.pool(&a, &f.roots[..1], &["attach", "--no-seed"]);
    let target = fs::read_link(a.join("target")).unwrap();
    let meta = target.parent().unwrap().join("slot.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
    json["lease"]["pending"] = true.into();
    fs::write(&meta, serde_json::to_vec(&json).unwrap()).unwrap();
    f.pool(&a, &f.roots[..1], &["attach"]);
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
    assert_eq!(json["lease"]["pending"], false);
}

#[test]
fn age_gc_cannot_delete_an_adoption_that_exceeds_the_pending_timeout() {
    let f = Fixture::new();
    let a = f.worktree("a");
    f.pool(&a, &f.roots[..1], &["attach", "--no-seed"]);
    let target = fs::read_link(a.join("target")).unwrap();
    fs::remove_file(a.join("target")).unwrap();
    let dir = target.parent().unwrap();
    let lock = fs::File::create(dir.join("seed.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let meta = dir.join("slot.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
    json["lease"]["pending"] = true.into();
    json["lease"]["acquired_at"] = 0.into();
    fs::write(&meta, serde_json::to_vec(&json).unwrap()).unwrap();
    f.pool(&a, &f.roots[..1], &["gc", "--max-age-days", "0"]);
    assert!(target.exists());
}

#[test]
fn adoption_retry_removes_abandoned_copy_before_checking_capacity() {
    use std::os::unix::fs::PermissionsExt;
    for (ready, staged, removed) in
        [(false, true, false), (true, true, false), (true, false, false), (false, true, true)]
    {
        let mut f = Fixture::new();
        if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
            return;
        }
        f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
        let a = f.worktree("a");
        fs::create_dir_all(a.join("target/debug/deps")).unwrap();
        fs::write(a.join("target/debug/.cargo-lock"), "").unwrap();
        fs::write(a.join("target/debug/deps/artifact"), vec![1; 1 << 20]).unwrap();
        fs::write(a.join("target/tests.log"), "keep log").unwrap();
        let bin = f.dir.join("bin");
        fs::create_dir(&bin).unwrap();
        let script = if ready {
            "#!/bin/sh\nif [ \"$1\" = \"-as\" ]; then exit 1; fi\nexec /bin/cp \"$@\"\n"
        } else {
            "#!/bin/sh\nexit 1\n"
        };
        fs::write(bin.join("cp"), script).unwrap();
        fs::set_permissions(bin.join("cp"), fs::Permissions::from_mode(0o755)).unwrap();
        let path =
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())))
                .unwrap();
        let failed = f
            .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
            .env("CARGO_POOL_DIR", &f.roots[0])
            .env("PATH", path)
            .arg("attach")
            .output()
            .unwrap();
        assert!(!failed.status.success());
        let pool = fs::read_dir(&f.roots[0]).unwrap().next().unwrap().unwrap().path();
        let target = pool.join("slots/0/target");
        let scratch = target.parent().unwrap().join("target.adopting");
        if staged {
            if ready {
                fs::rename(&target, &scratch).unwrap();
                fs::create_dir(&target).unwrap();
            } else {
                fs::create_dir(&scratch).unwrap();
                fs::write(scratch.join("incomplete"), vec![2; 8 << 20]).unwrap();
            }
        }
        if removed {
            fs::remove_dir_all(a.join("target")).unwrap();
        }
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::statvfs(c"/dev/shm".as_ptr(), &mut st) }, 0);
        let used = (st.f_blocks - st.f_bfree) * st.f_frsize;
        let avail = st.f_bavail * st.f_frsize;
        let budget = if ready { used + (512 << 10) } else { used - (8 << 20) + (3 << 20) };
        let limit = 100.0 * budget as f64 / (used + avail) as f64;
        success(
            f.command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
                .env("CARGO_POOL_DIR", &f.roots[0])
                .env("CARGO_POOL_MAX_DISK_PERCENT", limit.to_string())
                .arg("attach")
                .output()
                .unwrap(),
        );
        assert_eq!(fs::read_link(a.join("target")).unwrap(), target);
        assert_eq!(fs::read_to_string(a.join("target/tests.log")).unwrap(), "keep log");
        assert!(!scratch.exists());
        if removed {
            assert!(!a.join("target/debug/deps/artifact").exists());
            continue;
        }
        assert_eq!(fs::read(a.join("target/debug/deps/artifact")).unwrap(), vec![1; 1 << 20]);
        assert!(
            !fs::read_dir(target.join("debug/deps"))
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().contains("cargo-pool-saved"))
        );
    }
}

#[test]
fn adoption_retry_keeps_builds_after_a_completed_copy() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
        return;
    }
    f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
    let a = f.worktree("a");
    fs::create_dir_all(a.join("target/debug/deps")).unwrap();
    fs::write(a.join("target/debug/.cargo-lock"), "").unwrap();
    fs::write(a.join("target/debug/deps/artifact"), "old build").unwrap();
    fs::write(a.join("target/tests.log"), "keep log").unwrap();
    let bin = f.dir.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("cp"), "#!/bin/sh\nif [ \"$1\" = \"-as\" ]; then exit 1; fi\nexec /bin/cp \"$@\"\n").unwrap();
    fs::set_permissions(bin.join("cp"), fs::Permissions::from_mode(0o755)).unwrap();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())))
            .unwrap();
    let failed = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIR", &f.roots[0])
        .env("PATH", path)
        .arg("attach")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert_eq!(fs::read_to_string(a.join("target/tests.log")).unwrap(), "keep log");
    let b = f.worktree("b");
    f.pool(&b, &f.roots[..1], &["attach"]);
    f.pool(&b, &f.roots[..1], &["/bin/true", "-vV"]);
    assert!(!b.join("target/debug/deps/artifact").exists(), "unfinished adoption is not a seed source");
    fs::write(a.join("target/debug/deps/artifact"), "new build").unwrap();
    fs::write(a.join("target/debug/deps/new-artifact"), "added build").unwrap();
    f.pool(&a, &f.roots[..1], &["attach"]);
    assert_eq!(fs::read_to_string(a.join("target/debug/deps/artifact")).unwrap(), "new build");
    assert_eq!(fs::read_to_string(a.join("target/debug/deps/new-artifact")).unwrap(), "added build");
    assert_eq!(fs::read_to_string(a.join("target/tests.log")).unwrap(), "keep log");
}

#[test]
fn adoption_retry_keeps_the_complete_copy_after_interrupted_cleanup() {
    let mut f = Fixture::new();
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
        return;
    }
    f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
    let a = f.worktree("a");
    fs::create_dir_all(a.join("target/debug/deps")).unwrap();
    fs::write(a.join("target/debug/.cargo-lock"), "").unwrap();
    for name in ["artifact-a", "artifact-b"] {
        fs::write(a.join("target/debug/deps").join(name), name).unwrap();
    }
    let shim = f.dir.join("interrupt.c");
    fs::write(
        &shim,
        r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <signal.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
int unlinkat(int fd, const char *path, int flags) {
    int (*real_unlinkat)(int, const char *, int) = dlsym(RTLD_NEXT, "unlinkat");
    char proc[128], parent[4096];
    snprintf(proc, sizeof(proc), "/proc/self/fd/%d", fd);
    ssize_t n = readlink(proc, parent, sizeof(parent)-1);
    if (n >= 0) parent[n] = 0;
    int result = real_unlinkat(fd, path, flags);
    const char *root = getenv("CARGO_POOL_TEST_SOURCE");
    if (result == 0 && n >= 0 && root && strncmp(parent, root, strlen(root)) == 0 && strcmp(path, "artifact-a") == 0)
        kill(getpid(), SIGKILL);
    return result;
}
"#,
    )
    .unwrap();
    let lib = f.dir.join("interrupt.so");
    success(f.command("cc", &a).args(["-shared", "-fPIC", "-o"]).arg(&lib).arg(&shim).arg("-ldl").output().unwrap());
    let failed = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIR", &f.roots[0])
        .env("LD_PRELOAD", &lib)
        .env("CARGO_POOL_TEST_SOURCE", a.join("target"))
        .arg("attach")
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let detach = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIR", &f.roots[0])
        .arg("detach")
        .output()
        .unwrap();
    assert!(!detach.status.success(), "cleanup must finish before releasing the lease");

    f.pool(&a, &f.roots[..1], &["attach"]);
    for name in ["artifact-a", "artifact-b"] {
        assert_eq!(fs::read_to_string(a.join("target/debug/deps").join(name)).unwrap(), name);
    }
}

#[test]
fn adoption_preserves_a_new_profile_built_during_the_copy() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new();
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
        return;
    }
    f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
    let a = f.worktree("a");
    let cargo = success(Command::new("rustup").args(["which", "cargo"]).output().unwrap());
    let rustc = success(Command::new("rustup").args(["which", "rustc"]).output().unwrap());
    success(
        f.command(cargo.trim(), &a).env("RUSTC", rustc.trim()).env("RUSTC_WRAPPER", "").arg("build").output().unwrap(),
    );
    let bin = f.dir.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("cp"), "#!/bin/sh\n/bin/cp \"$@\" || exit $?\n\"$TEST_CARGO\" build --release --offline\n")
        .unwrap();
    fs::set_permissions(bin.join("cp"), fs::Permissions::from_mode(0o755)).unwrap();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())))
            .unwrap();
    let output = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIR", &f.roots[0])
        .env("PATH", path)
        .env("TEST_CARGO", cargo.trim())
        .env("RUSTC", rustc.trim())
        .env("RUSTC_WRAPPER", "")
        .arg("attach")
        .output()
        .unwrap();
    assert!(a.join("target/release/example").exists(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!output.status.success(), "the stale copy must not replace the new profile");
    assert!(fs::symlink_metadata(a.join("target")).unwrap().is_dir());
    f.pool(&a, &f.roots[..1], &["attach"]);
    assert!(a.join("target/debug/example").exists());
    assert!(a.join("target/release/example").exists());
}

#[test]
fn zero_copy_deferred_attachment_works_above_the_disk_limit() {
    let f = Fixture::new();
    let roots = &f.roots[..1];
    f.pool(&f.repo, roots, &["install-wrapper"]);
    let a = f.worktree("a");
    f.pool(&a, roots, &["attach", "--from-hook"]);
    success(
        f.command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
            .env("CARGO_POOL_DIR", &f.roots[0])
            .env("CARGO_POOL_MAX_DISK_PERCENT", "0")
            .args(["/bin/true", "-vV"])
            .output()
            .unwrap(),
    );
    assert!(fs::read_link(a.join("target")).is_ok());
}

#[test]
fn cargo_retries_a_failed_deferred_probe_without_caching_its_failure() {
    let f = Fixture::new();
    let roots = &f.roots[..1];
    f.pool(&f.repo, roots, &["install-wrapper"]);
    let a = f.worktree("a");
    f.pool(&a, roots, &["attach", "--from-hook"]);
    fs::write(a.join("target/tests.log"), "keep this log").unwrap();
    let blocked = f.dir.join("blocked");
    fs::write(&blocked, "not a directory").unwrap();
    let cargo = success(Command::new("rustup").args(["which", "cargo"]).output().unwrap());
    let rustc = success(Command::new("rustup").args(["which", "rustc"]).output().unwrap());
    let build = |root: &Path| {
        f.command(cargo.trim(), &a)
            .env("RUSTC", rustc.trim())
            .env("RUSTC_WRAPPER", env!("CARGO_BIN_EXE_cargo-pool"))
            .env("CARGO_POOL_DIR", root)
            .args(["build", "--offline"])
            .output()
            .unwrap()
    };
    assert!(!build(&blocked).status.success());
    success(build(&f.roots[0]));
    assert!(fs::read_link(a.join("target")).is_ok());
    assert_eq!(fs::read_to_string(a.join("target/tests.log")).unwrap(), "keep this log");
}

#[test]
fn adoption_recovers_without_atomic_exchange_support() {
    let mut f = Fixture::new();
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
        return;
    }
    f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
    let a = f.worktree("a");
    fs::create_dir_all(a.join("target/debug/deps")).unwrap();
    fs::write(a.join("target/debug/.cargo-lock"), "").unwrap();
    fs::write(a.join("target/debug/deps/artifact"), "warm").unwrap();
    let shim = f.dir.join("rename.c");
    fs::write(
        &shim,
        r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <unistd.h>
int renameat2(int a, const char *b, int c, const char *d, unsigned int flags) {
    errno = EINVAL;
    return -1;
}
int symlink(const char *from, const char *to) {
    const char *target = getenv("CARGO_POOL_TEST_TARGET");
    if (target && strcmp(to, target) == 0) _exit(70);
    const char *publishing = getenv("CARGO_POOL_TEST_PUBLISH");
    if (publishing && strcmp(to, publishing) == 0) {
        int fd = open(getenv("CARGO_POOL_TEST_LOCK"), O_WRONLY);
        if (fd < 0) _exit(72);
        if (flock(fd, LOCK_EX | LOCK_NB) == 0) _exit(71);
        close(fd);
    }
    int (*real_symlink)(const char *, const char *) = dlsym(RTLD_NEXT, "symlink");
    return real_symlink(from, to);
}
"#,
    )
    .unwrap();
    let lib = f.dir.join("rename.so");
    success(f.command("cc", &a).args(["-shared", "-fPIC", "-o"]).arg(&lib).arg(&shim).arg("-ldl").output().unwrap());
    let failed = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIR", &f.roots[0])
        .env("LD_PRELOAD", &lib)
        .env("CARGO_POOL_TEST_TARGET", a.join("target"))
        .arg("attach")
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(70), "{}", String::from_utf8_lossy(&failed.stderr));
    let anchors = f.repo.join(".git/cargo-pool-locks");
    let anchor = fs::read_dir(anchors).unwrap().next().unwrap().unwrap().path();
    success(
        f.command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
            .env("CARGO_POOL_DIR", &f.roots[0])
            .env("LD_PRELOAD", &lib)
            .env("CARGO_POOL_TEST_PUBLISH", a.join("target"))
            .env("CARGO_POOL_TEST_LOCK", anchor)
            .arg("attach")
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read_to_string(a.join("target/debug/deps/artifact")).unwrap(), "warm");
    let saved = a.join("target.cargo-pool-files");
    assert!(!fs::read_dir(saved).unwrap().flatten().any(|e| e.path().extension().is_some_and(|x| x == "attaching")));
}

#[test]
fn linked_pending_attach_finishes_preparing_the_reused_slot() {
    let f = Fixture::new();
    let a = f.worktree("a");
    f.pool(&a, &f.roots[..1], &["attach", "--no-seed"]);
    let target = fs::read_link(a.join("target")).unwrap();
    let unit = target.join("debug/.fingerprint/example-1234");
    fs::create_dir_all(&unit).unwrap();
    let foreign_git = f.repo.join(".git/HEAD");
    fs::write(unit.join("dep-bin-example"), foreign_git.as_os_str().as_encoded_bytes()).unwrap();
    let meta = target.parent().unwrap().join("slot.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
    json["lease"]["pending"] = true.into();
    fs::write(meta, serde_json::to_vec(&json).unwrap()).unwrap();
    f.pool(&a, &f.roots[..1], &["attach"]);
    assert!(!unit.exists(), "recovery must invalidate the previous worktree's git inputs");
}

#[test]
fn failed_preparation_leaves_the_attachment_pending_for_retry() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let a = f.worktree("a");
    let bin = f.dir.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("git"), "#!/bin/sh\ncase \"$*\" in *'rev-parse --path-format=absolute --git-dir') exit 1;; esac\nexec /usr/bin/git \"$@\"\n").unwrap();
    fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())))
            .unwrap();
    let failed = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIR", &f.roots[0])
        .env("PATH", path)
        .args(["attach", "--no-seed"])
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let target = fs::read_link(a.join("target")).unwrap();
    let meta = target.parent().unwrap().join("slot.json");
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
    assert_eq!(json["lease"]["pending"], true);
    f.pool(&a, &f.roots[..1], &["attach"]);
    let json: serde_json::Value = serde_json::from_slice(&fs::read(meta).unwrap()).unwrap();
    assert_eq!(json["lease"]["pending"], false);
}

#[test]
fn pending_recovery_releases_the_pool_lock_while_waiting_for_cargo() {
    let f = Fixture::new();
    let a = f.worktree("a");
    f.pool(&a, &f.roots[..1], &["attach", "--no-seed"]);
    let target = fs::read_link(a.join("target")).unwrap();
    fs::create_dir_all(target.join("debug")).unwrap();
    let build = fs::File::create(target.join("debug/.cargo-lock")).unwrap();
    assert_eq!(unsafe { libc::flock(build.as_raw_fd(), libc::LOCK_EX) }, 0);
    let meta = target.parent().unwrap().join("slot.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
    json["lease"]["pending"] = true.into();
    fs::write(meta, serde_json::to_vec(&json).unwrap()).unwrap();
    let marker = PathBuf::from(f.git(&a, &["rev-parse", "--absolute-git-dir"]).trim()).join("cargo-pool-attach");
    fs::write(marker, "").unwrap();
    let mut probe = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
        .env("CARGO_POOL_DIR", &f.roots[0])
        .args(["/bin/true", "-vV"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let waiting = probe.try_wait().unwrap().is_none();
    let pool = fs::File::open(target.ancestors().nth(3).unwrap().join(".lock")).unwrap();
    let pool_available = unsafe { libc::flock(pool.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    assert_eq!(unsafe { libc::flock(pool.as_raw_fd(), libc::LOCK_UN) }, 0);
    assert_eq!(unsafe { libc::flock(build.as_raw_fd(), libc::LOCK_UN) }, 0);
    success(probe.wait_with_output().unwrap());
    assert!(waiting);
    assert!(pool_available, "waiting for this worktree's build must not block other worktrees' attachments");
}

#[test]
fn main_and_similarly_named_worktrees_invalidate_foreign_git_inputs() {
    for main in [false, true] {
        let f = Fixture::new();
        let source = f.worktree("wt1");
        f.pool(&source, &f.roots[..1], &["attach", "--no-seed"]);
        let target = fs::read_link(source.join("target")).unwrap();
        let unit = target.join("debug/.fingerprint/example-1234");
        fs::create_dir_all(&unit).unwrap();
        let foreign = f.repo.join(".git/worktrees/wt1/HEAD");
        fs::write(unit.join("dep-bin-example"), foreign.as_os_str().as_encoded_bytes()).unwrap();
        f.pool(&source, &f.roots[..1], &["detach"]);
        let dest = if main { f.repo.clone() } else { f.worktree("wt") };
        f.pool(&dest, &f.roots[..1], &["attach", "--no-seed"]);
        assert!(!unit.exists(), "main and wt must not keep a fingerprint tracking wt1's HEAD");
    }
}

#[test]
fn backdate_permission_errors_do_not_block_a_warm_attachment() {
    let f = Fixture::new();
    let a = f.worktree("a");
    fs::File::open(a.join("src/main.rs")).unwrap().set_modified(UNIX_EPOCH).unwrap();
    fs::create_dir_all(a.join("target/debug")).unwrap();
    fs::write(a.join("target/debug/.cargo-lock"), "").unwrap();
    f.pool(&a, &f.roots[..1], &["attach", "--no-seed"]);
    let target = fs::read_link(a.join("target")).unwrap();
    f.pool(&a, &f.roots[..1], &["detach"]);
    let b = f.worktree("b");
    let shim = f.dir.join("timestamps.c");
    fs::write(&shim, "#include <errno.h>\n#include <sys/stat.h>\nint futimens(int fd, const struct timespec times[2]) { errno = EPERM; return -1; }\n").unwrap();
    let lib = f.dir.join("timestamps.so");
    success(f.command("cc", &b).args(["-shared", "-fPIC", "-o"]).arg(&lib).arg(&shim).output().unwrap());
    success(
        f.command(env!("CARGO_BIN_EXE_cargo-pool"), &b)
            .env("CARGO_POOL_DIR", &f.roots[0])
            .env("LD_PRELOAD", lib)
            .args(["attach", "--no-seed"])
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read_link(b.join("target")).unwrap(), target);
    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(target.parent().unwrap().join("slot.json")).unwrap()).unwrap();
    assert_eq!(meta["lease"]["pending"], false);
}

#[test]
fn pressure_can_evict_adopted_slots_after_the_repository_disappears() {
    let mut f = Fixture::new();
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
        return;
    }
    f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
    fs::create_dir_all(f.repo.join("target/debug/deps")).unwrap();
    fs::write(f.repo.join("target/debug/.cargo-lock"), "").unwrap();
    fs::write(f.repo.join("target/debug/deps/artifact"), "warm").unwrap();
    f.pool(&f.repo, &f.roots[..1], &["attach", "--no-seed"]);
    let target = fs::read_link(f.repo.join("target")).unwrap();
    fs::remove_dir_all(&f.repo).unwrap();
    f.git(&f.dir, &["init", "-q"]);
    success(
        f.command(env!("CARGO_BIN_EXE_cargo-pool"), &f.dir)
            .env("CARGO_POOL_DIR", &f.roots[0])
            .env("CARGO_POOL_MAX_DISK_PERCENT", "0")
            .arg("gc")
            .output()
            .unwrap(),
    );
    assert!(!target.exists(), "a deleted repository's missing lock anchor must not pin its artifacts");
}

#[test]
fn pressure_discards_abandoned_partial_copies_before_warm_slots() {
    let f = Fixture::new();
    let a = f.worktree("a");
    let b = f.worktree("b");
    f.pool(&a, &f.roots[..1], &["attach", "--no-seed"]);
    f.pool(&b, &f.roots[..1], &["attach", "--no-seed"]);
    let target = fs::read_link(a.join("target")).unwrap();
    let warm = fs::read_link(b.join("target")).unwrap();
    fs::write(warm.join("artifact"), vec![1; 4 << 20]).unwrap();
    f.pool(&b, &f.roots[..1], &["detach"]);
    fs::remove_file(a.join("target")).unwrap();
    fs::create_dir(a.join("target")).unwrap();
    let slot = target.parent().unwrap();
    let staged = slot.join("target.adopting");
    fs::create_dir(&staged).unwrap();
    fs::write(staged.join("partial"), vec![2; 32 << 20]).unwrap();
    let meta = slot.join("slot.json");
    let mut json: serde_json::Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
    json["lease"]["pending"] = true.into();
    json["adoption"] = "copying".into();
    fs::write(&meta, serde_json::to_vec(&json).unwrap()).unwrap();
    let path = std::ffi::CString::new(f.roots[0].as_os_str().as_encoded_bytes()).unwrap();
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::statvfs(path.as_ptr(), &mut st) }, 0);
    let used = (st.f_blocks - st.f_bfree) * st.f_frsize;
    let avail = st.f_bavail * st.f_frsize;
    let limit = 100.0 * (used - (16 << 20)) as f64 / (used + avail) as f64;
    success(
        f.command(env!("CARGO_BIN_EXE_cargo-pool"), &a)
            .env("CARGO_POOL_DIR", &f.roots[0])
            .env("CARGO_POOL_MAX_DISK_PERCENT", limit.to_string())
            .arg("gc")
            .output()
            .unwrap(),
    );
    assert!(!staged.exists());
    assert!(warm.join("artifact").exists(), "partial copies must go before reusable artifacts");
    fs::create_dir(&staged).unwrap();
    fs::write(staged.join("partial"), "leftover").unwrap();
    json["lease"]["acquired_at"] = 0.into();
    fs::write(meta, serde_json::to_vec(&json).unwrap()).unwrap();
    f.git(&f.repo, &["worktree", "remove", "--force", a.to_str().unwrap()]);
    f.pool(&f.repo, &f.roots[..1], &["gc"]);
    assert!(!staged.exists(), "released slots must not retain partial copies");
}

#[test]
fn cross_device_adoption_preserves_cargo_lock_identity() {
    let mut f = Fixture::new();
    if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
        return;
    }
    f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
    let a = f.worktree("a");
    fs::create_dir_all(a.join("target/debug/deps")).unwrap();
    fs::write(a.join("target/debug/deps/artifact"), "warm").unwrap();
    let original = fs::File::create(a.join("target/debug/.cargo-lock")).unwrap();
    f.pool(&a, &f.roots[..1], &["attach", "--no-seed"]);
    let pooled = fs::File::open(a.join("target/debug/.cargo-lock")).unwrap();
    let identity = |f: &fs::File| {
        let m = f.metadata().unwrap();
        (m.dev(), m.ino())
    };
    assert_eq!(identity(&original), identity(&pooled), "waiting and newly arriving Cargo commands need the same lock");
    assert_eq!(unsafe { libc::flock(original.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    assert_eq!(unsafe { libc::flock(pooled.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, -1);
    assert_eq!(unsafe { libc::flock(original.as_raw_fd(), libc::LOCK_UN) }, 0);
    let seeded = f.worktree("seeded");
    f.pool(&seeded, &f.roots[..1], &["attach"]);
    f.pool(&seeded, &f.roots[..1], &["/bin/true", "-vV"]);
    assert_eq!(fs::read_to_string(seeded.join("target/debug/deps/artifact")).unwrap(), "warm");
    let independent = fs::File::create(seeded.join("target/debug/.cargo-lock")).unwrap();
    assert_ne!(identity(&original), identity(&independent), "seeded slots must have independent locks");
    drop(original);
    drop(pooled);
    f.pool(&a, &f.roots[..1], &["detach"]);
    f.git(&f.repo, &["worktree", "remove", "--force", a.to_str().unwrap()]);
    let b = f.worktree("b");
    f.pool(&b, &f.roots[..1], &["attach", "--no-seed"]);
    let lock = fs::File::create(b.join("target/debug/.cargo-lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    assert_eq!(fs::read_to_string(b.join("target/debug/deps/artifact")).unwrap(), "warm");
}

#[test]
fn reused_and_seeded_slots_do_not_inherit_another_worktrees_log_links() {
    use std::io::Write;
    for (cross_device, seed) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut f = Fixture::new();
        if cross_device {
            if !fs::metadata("/dev/shm").is_ok_and(|m| m.dev() != fs::metadata(&f.dir).unwrap().dev()) {
                continue;
            }
            f.roots[0] = Path::new("/dev/shm").join(f.dir.file_name().unwrap());
        }
        let a = f.worktree("a");
        fs::create_dir_all(a.join("target/debug/deps")).unwrap();
        fs::create_dir_all(a.join("target/flycheck0")).unwrap();
        fs::write(a.join("target/debug/.cargo-lock"), "").unwrap();
        fs::write(a.join("target/debug/deps/generated.log"), "generated artifact").unwrap();
        let paths = ["results.txt", "debug/deps/results.txt", "flycheck0/stdout"];
        let mut logs: Vec<_> = paths
            .iter()
            .map(|p| {
                let mut log = fs::File::create(a.join("target").join(p)).unwrap();
                log.write_all(b"A's output").unwrap();
                log
            })
            .collect();
        f.pool(&a, &f.roots[..1], &["attach", "--no-seed"]);
        let backing: Vec<_> = paths.iter().map(|p| fs::canonicalize(a.join("target").join(p)).unwrap()).collect();
        if !seed {
            f.pool(&a, &f.roots[..1], &["detach"]);
        }
        let b = f.worktree("b");
        f.pool(&b, &f.roots[..1], &["attach"]);
        f.pool(&b, &f.roots[..1], &["/bin/true", "-vV"]);
        assert_eq!(fs::read_to_string(b.join("target/debug/deps/generated.log")).unwrap(), "generated artifact");
        for ((path, original), log) in paths.iter().zip(&backing).zip(&mut logs) {
            let to = b.join("target").join(path);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::write(&to, "B's output").unwrap();
            log.write_all(b" after").unwrap();
            assert_eq!(fs::read_to_string(original).unwrap(), "A's output after");
        }
        f.git(&f.repo, &["worktree", "remove", "--force", a.to_str().unwrap()]);
        for path in paths {
            fs::write(b.join("target").join(path), "still writable").unwrap();
        }
    }
}

#[test]
fn job_slots_cap_concurrent_compilers_across_cargo_invocations() {
    let f = Fixture::new();
    let compiler = f.dir.join("fake-rustc");
    // Leaves a daemon behind, like an sccache client starting its server: it mustn't keep the slot.
    let script = "#!/bin/sh\nsleep 10 >/dev/null 2>&1 &\necho \"start $(date +%s%N)\" >> \"$1\"\nsleep 0.3\necho \"end $(date +%s%N)\" >> \"$1\"\n";
    fs::write(&compiler, script).unwrap();
    fs::set_permissions(&compiler, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let log = f.dir.join("compiles");
    let started = std::time::Instant::now();
    let compiles: Vec<_> = (0..3)
        .map(|_| {
            f.command(env!("CARGO_BIN_EXE_cargo-pool"), &f.repo)
                .env("CARGO_POOL_JOBS", "1")
                .arg(&compiler)
                .arg(&log)
                .args(["--out-dir", "."])
                .spawn()
                .unwrap()
        })
        .collect();
    for mut c in compiles {
        assert!(c.wait().unwrap().success());
    }
    assert!(started.elapsed().as_secs() < 5, "a daemon kept the slot");
    let log = fs::read_to_string(&log).unwrap();
    let events: Vec<_> = log.lines().map(|l| l.split(' ').next().unwrap()).collect();
    assert_eq!(events, ["start", "end", "start", "end", "start", "end"]);
}

#[test]
fn compiles_nested_in_a_slot_holder_do_not_wait_for_a_slot() {
    let f = Fixture::new();
    let compiler = f.dir.join("fake-rustc");
    // Like a proc macro building a crate: compiles again through the wrapper before exiting.
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = outer ]; then \"{}\" \"$0\" inner --out-dir . || exit 1; fi\nexit 0\n",
        env!("CARGO_BIN_EXE_cargo-pool")
    );
    fs::write(&compiler, script).unwrap();
    fs::set_permissions(&compiler, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut outer = f
        .command(env!("CARGO_BIN_EXE_cargo-pool"), &f.repo)
        .env("CARGO_POOL_JOBS", "1")
        .args([compiler.as_os_str(), "outer".as_ref(), "--out-dir".as_ref(), ".".as_ref()])
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while outer.try_wait().unwrap().is_none() {
        if started.elapsed().as_secs() >= 5 {
            outer.kill().unwrap();
            panic!("the nested compile waited for the slot its parent holds");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(outer.wait().unwrap().success());
}
