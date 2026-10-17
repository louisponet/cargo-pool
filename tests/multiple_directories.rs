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
    f.pool(&b, &f.roots[1..], &["attach"]);
    let target = fs::read_link(b.join("target")).unwrap();
    f.pool(&b, &f.roots, &["/bin/true", "-vV"]);
    assert_eq!(fs::read_to_string(target.join("cached-artifact")).unwrap(), "warm");
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
