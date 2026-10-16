//! cargo-pool: share warm cargo `target/` directories between git worktrees.
//!
//! Each repository (identified by its git common dir) gets a pool of slots under
//! `$CARGO_POOL_DIR` (default `~/.cache/cargo-pool`). A worktree attaches to a slot
//! by having its `target` replaced with a symlink into the slot. A slot is leased
//! to at most one worktree at a time, so concurrent agents never fight over the
//! same cargo build lock. Leases are released explicitly with `detach`, or
//! implicitly when the worktree disappears (e.g. `git worktree remove`).

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

const HOOK_BEGIN: &str = "# >>> cargo-pool >>>";
const HOOK_END: &str = "# <<< cargo-pool <<<";
const NULL_SHA: &str = "0000000000000000000000000000000000000000";
/// A lease still being set up (slot seeding in progress) is honoured for this long.
const PENDING_TIMEOUT_SECS: u64 = 3 * 3600;

#[derive(Parser)]
#[command(version, about = "Share warm cargo target dirs between git worktrees")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Attach a worktree's `target` to a pooled target dir (idempotent).
    Attach {
        /// Worktree path (default: current directory).
        path: Option<PathBuf>,
        /// Don't seed a newly created slot by copying the closest existing target dir.
        #[arg(long)]
        no_seed: bool,
        /// Replace an existing non-empty `target` directory (its contents are deleted).
        #[arg(long)]
        force: bool,
        /// Also pool the main worktree (by default only linked worktrees are pooled).
        #[arg(long)]
        allow_main: bool,
        /// Invoked from the git hook: never fail, only act on cargo workspaces.
        #[arg(long, hide = true)]
        from_hook: bool,
    },
    /// Release a worktree's slot back to the pool and remove the `target` symlink.
    Detach {
        /// Worktree path (default: current directory).
        path: Option<PathBuf>,
    },
    /// Show the pool for the current repository (or all pools outside a repo).
    Status {
        /// Compute on-disk size of every slot (slow for big targets).
        #[arg(long)]
        size: bool,
    },
    /// Reclaim stale leases and free disk space if usage is above the limit.
    Gc {
        /// Also delete free slots unused for longer than this many days.
        #[arg(long)]
        max_age_days: Option<u64>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Install a post-checkout hook that attaches every newly created worktree.
    InstallHook {
        /// Repository path (default: current directory).
        path: Option<PathBuf>,
    },
    /// Remove the hook block installed by `install-hook`.
    UninstallHook {
        path: Option<PathBuf>,
    },
    /// Make cargo run every compiler call through cargo-pool (`build.rustc-wrapper` in
    /// `$CARGO_HOME/config.toml`), so disk usage is checked during every pooled build.
    InstallWrapper,
    /// Remove the `build.rustc-wrapper` entry added by `install-wrapper`.
    UninstallWrapper,
    /// Hook entry point (called by the installed post-checkout hook).
    #[command(hide = true)]
    PostCheckout { prev: String, new: String, flag: String },
}

fn main() {
    // Support `cargo pool ...`, which invokes us as `cargo-pool pool ...`.
    let mut args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "pool") {
        args.remove(1);
    }
    if is_rustc_wrapper(&args) {
        rustc_wrapper(args);
    }
    // Die quietly on a closed stdout (`cargo pool status | head`) instead of panicking.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse_from(args);
    let from_hook = matches!(
        cli.cmd,
        Cmd::PostCheckout { .. } | Cmd::Attach { from_hook: true, .. }
    );
    if let Err(e) = run(cli) {
        eprintln!("cargo-pool: error: {e:#}");
        // A failing hook must never break `git worktree add`.
        if !from_hook {
            std::process::exit(1);
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Attach { path, no_seed, force, allow_main, from_hook } => {
            attach(&path_or_cwd(path)?, AttachOpts { seed: !no_seed, force, allow_main, from_hook })
        }
        Cmd::Detach { path } => detach(&path_or_cwd(path)?),
        Cmd::Status { size } => status(size),
        Cmd::Gc { max_age_days, dry_run } => gc(max_age_days, dry_run),
        Cmd::InstallHook { path } => install_hook(&path_or_cwd(path)?),
        Cmd::UninstallHook { path } => uninstall_hook(&path_or_cwd(path)?),
        Cmd::InstallWrapper => install_wrapper(),
        Cmd::UninstallWrapper => uninstall_wrapper(),
        Cmd::PostCheckout { prev, .. } => {
            // A null previous HEAD means a fresh checkout: `git worktree add` or clone.
            if prev != NULL_SHA {
                return Ok(());
            }
            let opts = AttachOpts { seed: true, force: false, allow_main: false, from_hook: true };
            attach(&std::env::current_dir()?, opts)
        }
    }
}

fn path_or_cwd(p: Option<PathBuf>) -> Result<PathBuf> {
    let p = match p {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    fs::canonicalize(&p).with_context(|| format!("{} does not exist", p.display()))
}

// ---------------------------------------------------------------------------
// git helpers

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // Hooks run with GIT_DIR & co. pointing at the new worktree; we always
        // address repositories explicitly via `-C`, so drop them.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_PREFIX")
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Number of commits separating two revisions (sum of both sides of `a...b`).
fn distance(dir: &Path, a: Option<&str>, b: &str) -> u64 {
    let Some(a) = a else { return u64::MAX };
    if a == b {
        return 0;
    }
    git(dir, &["rev-list", "--count", "--left-right", &format!("{a}...{b}")])
        .ok()
        .and_then(|s| {
            let mut it = s.split_whitespace().map(|n| n.parse::<u64>().ok());
            Some(it.next()?? + it.next()??)
        })
        .unwrap_or(u64::MAX)
}

struct Repo {
    /// Top level of the worktree we're operating on.
    worktree: PathBuf,
    common_dir: PathBuf,
    /// Main worktree, if the repository isn't bare.
    main_worktree: Option<PathBuf>,
    /// Pool directory for this repository.
    pool: PathBuf,
}

impl Repo {
    fn discover(path: &Path) -> Result<Repo> {
        let worktree = PathBuf::from(git(path, &["rev-parse", "--show-toplevel"])?);
        let common = git(&worktree, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        let common_dir = fs::canonicalize(&common)?;
        let main_worktree = (common_dir.file_name().is_some_and(|n| n == ".git"))
            .then(|| common_dir.parent().unwrap().to_path_buf());
        let name = main_worktree
            .as_deref()
            .unwrap_or(&common_dir)
            .file_name()
            .map(|n| n.to_string_lossy().trim_end_matches(".git").to_string())
            .unwrap_or_else(|| "repo".into());
        let key = format!("{name}-{:016x}", fnv1a(common_dir.as_os_str().as_encoded_bytes()));
        let pool = pool_root()?.join(key);
        Ok(Repo { worktree, common_dir, main_worktree, pool })
    }

    fn is_main(&self) -> bool {
        self.main_worktree.as_deref() == Some(&self.worktree)
    }

    fn head(&self) -> Result<String> {
        git(&self.worktree, &["rev-parse", "HEAD"])
    }

    fn max_disk_percent(&self) -> f64 {
        setting(&self.common_dir, "CARGO_POOL_MAX_DISK_PERCENT", "cargo-pool.maxDiskPercent", 90.0)
    }

    fn slots_dir(&self) -> PathBuf {
        self.pool.join("slots")
    }

    fn lock(&self) -> Result<FileLock> {
        fs::create_dir_all(self.slots_dir())?;
        fs::write(self.pool.join("repo"), self.common_dir.as_os_str().as_encoded_bytes())?;
        FileLock::acquire(&self.pool.join(".lock"), true)?
            .ok_or_else(|| anyhow!("could not lock pool"))
    }
}

/// Env var, else git config of the repository at `git_dir` (any dir inside it), else default.
fn setting<T: std::str::FromStr>(git_dir: &Path, env: &str, git_key: &str, default: T) -> T {
    std::env::var(env)
        .ok()
        .or_else(|| git(git_dir, &["config", "--get", git_key]).ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn idle_hours(git_dir: &Path) -> u64 {
    setting(git_dir, "CARGO_POOL_IDLE_HOURS", "cargo-pool.idleHours", 48)
}

fn pool_root() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("CARGO_POOL_DIR") {
        return Ok(PathBuf::from(d));
    }
    if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(d).join("cargo-pool"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join(".cache/cargo-pool"))
}

/// Stable hash for directory names (std's hasher isn't guaranteed stable).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

// ---------------------------------------------------------------------------
// locking

/// Released when dropped (closing the fd drops the flock).
struct FileLock(#[allow(dead_code)] File);

impl FileLock {
    /// `blocking == false` returns `Ok(None)` if the lock is held elsewhere.
    fn acquire(path: &Path, blocking: bool) -> Result<Option<FileLock>> {
        let f = OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
        let op = libc::LOCK_EX | if blocking { 0 } else { libc::LOCK_NB };
        if unsafe { libc::flock(f.as_raw_fd(), op) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(err).with_context(|| format!("flock {}", path.display()));
        }
        Ok(Some(FileLock(f)))
    }
}

/// Cargo's build-directory locks (`target/<profile>/.cargo-lock`) inside a target dir.
fn cargo_locks(target: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![(target.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_name() == ".cargo-lock" {
                out.push(p);
            } else if depth < 2 && e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push((p, depth + 1));
            }
        }
    }
    out
}

/// Try to take every cargo lock in `target` so no build runs while we copy it.
fn lock_target(target: &Path) -> Option<Vec<FileLock>> {
    cargo_locks(target)
        .iter()
        .map(|p| FileLock::acquire(p, false).ok().flatten())
        .collect()
}

/// Last time cargo produced output in this target dir.
fn last_activity(target: &Path) -> u64 {
    let mut newest = mtime(target);
    let mut stack = vec![(target.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = e.file_name();
            if name == "deps" || name == ".fingerprint" || name == "incremental" {
                newest = newest.max(mtime(&e.path()));
            } else if depth < 2 {
                stack.push((e.path(), depth + 1));
            }
        }
    }
    newest
}

fn mtime(p: &Path) -> u64 {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

// ---------------------------------------------------------------------------
// slots

#[derive(Serialize, Deserialize, Clone)]
struct Lease {
    worktree: PathBuf,
    head: String,
    acquired_at: u64,
    /// Slot is still being seeded; the symlink doesn't exist yet.
    #[serde(default)]
    pending: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct SlotMeta {
    lease: Option<Lease>,
    /// Commit the target dir was last built for (best effort).
    last_head: Option<String>,
    last_used: u64,
}

struct Slot {
    id: u32,
    dir: PathBuf,
    meta: SlotMeta,
}

#[derive(PartialEq, Debug)]
enum State {
    Free,
    Held,
    Stale,
}

impl Slot {
    fn target(&self) -> PathBuf {
        self.dir.join("target")
    }

    fn save(&self) -> Result<()> {
        let tmp = self.dir.join("slot.json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&self.meta)?)?;
        fs::rename(tmp, self.dir.join("slot.json"))?;
        Ok(())
    }

    fn state(&self) -> State {
        let Some(l) = &self.meta.lease else { return State::Free };
        if l.pending && now().saturating_sub(l.acquired_at) < PENDING_TIMEOUT_SECS {
            return State::Held;
        }
        if fs::read_link(l.worktree.join("target")).is_ok_and(|t| t == self.target()) {
            State::Held
        } else {
            State::Stale
        }
    }

    /// Turn a lease into a free slot, remembering what it was built for.
    fn release(&mut self, head: Option<String>) {
        if let Some(l) = self.meta.lease.take() {
            self.meta.last_head = head.or(Some(l.head));
            self.meta.last_used = now().max(last_activity(&self.target()));
        }
    }
}

fn load_slots(slots_dir: &Path) -> Result<Vec<Slot>> {
    let mut slots = vec![];
    let Ok(rd) = fs::read_dir(slots_dir) else { return Ok(slots) };
    for e in rd.flatten() {
        let Some(id) = e.file_name().to_str().and_then(|s| s.parse().ok()) else { continue };
        let dir = e.path();
        let meta = fs::read(dir.join("slot.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        slots.push(Slot { id, dir, meta });
    }
    slots.sort_by_key(|s| s.id);
    Ok(slots)
}

/// Free every lease whose worktree is gone or no longer links to its slot.
fn reclaim_stale(slots: &mut [Slot]) -> Result<()> {
    for s in slots.iter_mut().filter(|s| s.state() == State::Stale) {
        s.release(None);
        s.save()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// disk pressure

/// Disk usage (in percent, as `df` reports it) of the filesystem holding `path`,
/// pretending `extra` more bytes had already been written.
fn disk_percent(path: &Path, extra: u64) -> Result<f64> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("statvfs {}", path.display()));
    }
    let bs = st.f_frsize as u64;
    let used = (st.f_blocks - st.f_bfree) as u64 * bs + extra;
    let avail = (st.f_bavail as u64 * bs).saturating_sub(extra);
    Ok(100.0 * used as f64 / (used + avail).max(1) as f64)
}

/// Bytes allocated on disk by everything under `p` (symlinks not followed).
fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
        total += m.blocks() * 512;
        if m.is_dir() {
            stack.extend(fs::read_dir(&p).into_iter().flatten().flatten().map(|e| e.path()));
        }
    }
    total
}

fn human(bytes: u64) -> String {
    let gb = bytes as f64 / (1u64 << 30) as f64;
    if gb >= 1.0 { format!("{gb:.1}G") } else { format!("{:.0}M", bytes as f64 / (1u64 << 20) as f64) }
}

/// Delete pooled target dirs, across every pool on this machine, until disk usage
/// (plus `extra` bytes about to be written) is below `max_percent`.
///
/// Free slots go first, then slots of worktrees idle for longer than their repo's
/// `idleHours`, least recently used first. Slots in `protect`, slots with a cargo
/// build running and pools locked by another cargo-pool process are never touched.
/// Must be called without holding any pool lock. Returns whether the goal was met.
fn free_disk(max_percent: f64, extra: u64, protect: &[&Path]) -> Result<bool> {
    let root = pool_root()?;
    fs::create_dir_all(&root)?;
    if disk_percent(&root, extra)? < max_percent {
        return Ok(true);
    }

    // (idle-held?, last used, pool, slot id)
    let mut candidates = vec![];
    for pool in fs::read_dir(&root)?.flatten().map(|e| e.path()) {
        let git_dir = PathBuf::from(fs::read_to_string(pool.join("repo")).unwrap_or_default());
        let cutoff = now().saturating_sub(idle_hours(&git_dir) * 3600);
        for s in load_slots(&pool.join("slots"))? {
            if protect.contains(&s.dir.as_path()) {
                continue;
            }
            let used = s.meta.last_used.max(last_activity(&s.target()));
            match (s.state(), &s.meta.lease) {
                (State::Free | State::Stale, _) => candidates.push((false, used, pool.clone(), s.id)),
                (State::Held, Some(l)) if !l.pending && used.max(l.acquired_at) < cutoff => {
                    candidates.push((true, used.max(l.acquired_at), pool.clone(), s.id))
                }
                _ => {}
            }
        }
    }
    candidates.sort();

    for (idle_held, _, pool, id) in candidates {
        let Some(_pool_lock) = FileLock::acquire(&pool.join(".lock"), false)? else { continue };
        // Re-check under the lock, things may have changed since the scan.
        let Some(s) = load_slots(&pool.join("slots"))?.into_iter().find(|s| s.id == id) else { continue };
        let state = s.state();
        if state == State::Held && !idle_held {
            continue;
        }
        let Some(_build_locks) = lock_target(&s.target()) else { continue };
        let mut from = String::new();
        if let (State::Held, Some(l)) = (state, &s.meta.lease) {
            let link = l.worktree.join("target");
            if fs::read_link(&link).is_ok_and(|t| t == s.target()) {
                fs::remove_file(&link)?;
            }
            from = format!(", taken from idle worktree {}", l.worktree.display());
        }
        let size = dir_size(&s.dir);
        fs::remove_dir_all(&s.dir).with_context(|| format!("removing {}", s.dir.display()))?;
        eprintln!(
            "cargo-pool: disk above {max_percent}%: deleted slot {id} of {} ({}{from})",
            pool.file_name().unwrap_or_default().to_string_lossy(),
            human(size)
        );
        if disk_percent(&root, extra)? < max_percent {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// rustc wrapper

/// Cargo runs a `build.rustc-wrapper` as `<wrapper> <rustc> <args...>`: a compiler path
/// (or name) in place of one of our subcommands means we're wrapping a compiler call.
fn is_rustc_wrapper(args: &[OsString]) -> bool {
    let Some(first) = args.get(1) else { return false };
    let name = Path::new(first).file_name().unwrap_or_default().to_string_lossy();
    let is_subcommand = <Cli as clap::CommandFactory>::command()
        .get_subcommands()
        .any(|c| c.get_name() == first.to_string_lossy());
    !is_subcommand && (Path::new(first).is_file() || name.starts_with("rustc") || name.starts_with("clippy-driver"))
}

/// Check disk usage if this compile writes into a pooled target dir, then exec the
/// compiler (through `CARGO_POOL_RUSTC_WRAPPER`, e.g. sccache, if set).
fn rustc_wrapper(args: Vec<OsString>) -> ! {
    if let Err(e) = check_disk_during_build(&args[2..]) {
        eprintln!("cargo-pool: warning: disk check failed: {e:#}");
    }
    let (program, rest) = match std::env::var_os("CARGO_POOL_RUSTC_WRAPPER").filter(|w| !w.is_empty()) {
        Some(inner) => (inner, &args[1..]),
        None => (args[1].clone(), &args[2..]),
    };
    let err = Command::new(&program).args(rest).exec();
    eprintln!("cargo-pool: failed to run {}: {err}", program.to_string_lossy());
    std::process::exit(1);
}

fn check_disk_during_build(rustc_args: &[OsString]) -> Result<()> {
    let Some(out_dir) = rustc_args.iter().position(|a| a == "--out-dir").and_then(|i| rustc_args.get(i + 1)) else {
        return Ok(());
    };
    let (Ok(out_dir), Ok(root)) = (fs::canonicalize(out_dir), fs::canonicalize(pool_root()?)) else {
        return Ok(());
    };
    let Some(pool) = out_dir.strip_prefix(&root).ok().and_then(|p| p.components().next()) else {
        return Ok(());
    };
    let git_dir = PathBuf::from(fs::read_to_string(root.join(pool).join("repo")).unwrap_or_default());
    let max_disk = setting(&git_dir, "CARGO_POOL_MAX_DISK_PERCENT", "cargo-pool.maxDiskPercent", 90.0);
    if disk_percent(&root, 0)? < max_disk {
        return Ok(());
    }
    // Over the limit. Many rustc processes run in parallel: let one of them clean up,
    // and don't rescan more than once a minute if the disk can't be brought under.
    // The marker holds the time of the last cleanup (its mtime changes when locking it).
    let marker = root.join(".disk-cleanup");
    let recent = || {
        let last: u64 = fs::read_to_string(&marker).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        now().saturating_sub(last) < 60
    };
    if recent() {
        return Ok(());
    }
    let Some(_lock) = FileLock::acquire(&marker, false)? else { return Ok(()) };
    if recent() {
        return Ok(());
    }
    let ok = free_disk(max_disk, 0, &[]);
    fs::write(&marker, now().to_string())?;
    if !ok? {
        eprintln!("cargo-pool: warning: disk is still above {max_disk}% after cleaning up all pools");
    }
    Ok(())
}

fn cargo_config() -> Result<PathBuf> {
    let home = match std::env::var_os("CARGO_HOME") {
        Some(h) => PathBuf::from(h),
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?).join(".cargo"),
    };
    Ok(home.join("config.toml"))
}

fn install_wrapper() -> Result<()> {
    let config = cargo_config()?;
    let exe = fs::canonicalize(std::env::current_exe()?)?;
    let current = fs::read_to_string(&config).unwrap_or_default();
    if let Some(line) = current.lines().find(|l| l.trim_start().starts_with("rustc-wrapper")) {
        if line.contains("cargo-pool") {
            println!("already installed in {}", config.display());
            return Ok(());
        }
        bail!(
            "{} already sets `{}`; remove it first (set CARGO_POOL_RUSTC_WRAPPER to keep using it through cargo-pool)",
            config.display(),
            line.trim()
        );
    }
    let entry = format!("rustc-wrapper = {:?}  # cargo-pool: disk check on every pooled build", exe.to_string_lossy());
    let updated = match current.lines().position(|l| l.trim() == "[build]") {
        Some(i) => {
            let mut lines: Vec<&str> = current.lines().collect();
            lines.insert(i + 1, &entry);
            lines.join("\n") + "\n"
        }
        None => {
            let sep = if current.is_empty() { "" } else if current.ends_with('\n') { "\n" } else { "\n\n" };
            format!("{current}{sep}[build]\n{entry}\n")
        }
    };
    fs::create_dir_all(config.parent().unwrap())?;
    fs::write(&config, updated)?;
    println!("installed rustc-wrapper in {}", config.display());
    if let Some(w) = std::env::var_os("RUSTC_WRAPPER").filter(|w| !w.is_empty()) {
        eprintln!(
            "cargo-pool: warning: RUSTC_WRAPPER={} is set in this environment and overrides the config",
            w.to_string_lossy()
        );
    }
    Ok(())
}

fn uninstall_wrapper() -> Result<()> {
    let config = cargo_config()?;
    let current = fs::read_to_string(&config).with_context(|| format!("reading {}", config.display()))?;
    let kept: Vec<&str> = current
        .lines()
        .filter(|l| !(l.trim_start().starts_with("rustc-wrapper") && l.contains("cargo-pool")))
        .collect();
    // Drop a `[build]` table we left empty.
    let mut out: Vec<&str> = vec![];
    for (i, l) in kept.iter().enumerate() {
        let next = kept[i + 1..].iter().find(|n| !n.trim().is_empty());
        if l.trim() == "[build]" && next.is_none_or(|n| n.trim_start().starts_with('[')) {
            continue;
        }
        out.push(l);
    }
    let text = out.join("\n").trim().to_string();
    if text.is_empty() {
        fs::remove_file(&config)?;
    } else {
        fs::write(&config, text + "\n")?;
    }
    println!("removed cargo-pool rustc-wrapper from {}", config.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// attach / detach

struct AttachOpts {
    seed: bool,
    force: bool,
    allow_main: bool,
    from_hook: bool,
}

fn attach(path: &Path, opts: AttachOpts) -> Result<()> {
    let repo = Repo::discover(path)?;
    if !repo.worktree.join("Cargo.toml").exists() {
        if !opts.from_hook {
            println!("{} is not a cargo workspace, nothing to do", repo.worktree.display());
        }
        return Ok(());
    }
    if repo.is_main() && !opts.allow_main {
        if !opts.from_hook {
            println!("skipping main worktree (pass --allow-main to pool it too)");
        }
        return Ok(());
    }

    let link = repo.worktree.join("target");
    let slots_dir = repo.slots_dir();
    match fs::symlink_metadata(&link) {
        Err(_) => {}
        Ok(m) if m.file_type().is_symlink() => {
            let dest = fs::read_link(&link)?;
            if dest.starts_with(&slots_dir) {
                let _lock = repo.lock()?;
                let held = load_slots(&repo.slots_dir())?.into_iter().any(|s| {
                    s.target() == dest && s.meta.lease.as_ref().is_some_and(|l| l.worktree == repo.worktree)
                });
                if held {
                    println!("already attached: {} -> {}", link.display(), dest.display());
                    return Ok(());
                }
                // Our lease was stolen or lost; drop the dangling link and re-attach.
                fs::remove_file(&link)?;
            } else if opts.force {
                fs::remove_file(&link)?;
            } else {
                bail!("{} is a symlink to {}; use --force to replace it", link.display(), dest.display());
            }
        }
        Ok(m) if m.is_dir() => {
            if fs::read_dir(&link)?.next().is_none() {
                fs::remove_dir(&link)?;
            } else if opts.force {
                fs::remove_dir_all(&link)?;
            } else {
                bail!("{} already exists and is not empty; use --force to discard it", link.display());
            }
        }
        Ok(_) => bail!("{} exists and is not a directory", link.display()),
    }

    let max_disk = repo.max_disk_percent();
    if !free_disk(max_disk, 0, &[])? {
        eprintln!("cargo-pool: warning: disk is still above {max_disk}% after cleaning up all pools");
    }

    let head = repo.head()?;
    let lease = |pending| Lease { worktree: repo.worktree.clone(), head: head.clone(), acquired_at: now(), pending };

    let lock = repo.lock()?;
    let mut slots = load_slots(&repo.slots_dir())?;
    reclaim_stale(&mut slots)?;

    // Best free slot: closest commit, then most recently used.
    let pick = slots
        .iter()
        .enumerate()
        .filter(|(_, s)| s.state() == State::Free)
        .min_by_key(|(_, s)| (distance(&repo.worktree, s.meta.last_head.as_deref(), &head), u64::MAX - s.meta.last_used))
        .map(|(i, _)| i);

    let slot_idx = match pick {
        Some(i) => {
            let s = &mut slots[i];
            let d = distance(&repo.worktree, s.meta.last_head.as_deref(), &head);
            s.meta.lease = Some(lease(false));
            fs::create_dir_all(s.target())?;
            s.save()?;
            report_pick(s, d);
            drop(lock);
            i
        }
        None => {
            let id = slots.iter().map(|s| s.id + 1).max().unwrap_or(0);
            let dir = slots_dir.join(id.to_string());
            fs::create_dir_all(&dir)?;
            let mut slot = Slot { id, dir, meta: SlotMeta { lease: Some(lease(true)), ..Default::default() } };
            slot.save()?;
            // Seed outside the pool lock: copying a big target takes a while.
            let sources = seed_sources(&repo, &slots);
            drop(lock);
            let seeded_from = if opts.seed { seed(&repo, &slot, &head, sources)? } else { None };
            fs::create_dir_all(slot.target())?;
            let _lock = repo.lock()?;
            slot.meta.last_head = seeded_from;
            slot.meta.lease = Some(lease(false));
            slot.save()?;
            slots.push(slot);
            slots.len() - 1
        }
    };

    let slot = &slots[slot_idx];
    symlink(slot.target(), &link).with_context(|| format!("creating {}", link.display()))?;
    exclude_target(&repo)?;
    println!("cargo-pool: {} -> slot {}", link.display(), slot.id);
    Ok(())
}

fn report_pick(s: &Slot, d: u64) {
    let built = match (&s.meta.last_head, d) {
        (None, _) => "unknown commit".to_string(),
        (Some(h), u64::MAX) => format!("{} (unrelated)", short(h)),
        (Some(h), d) => format!("{} ({d} commits away)", short(h)),
    };
    println!("cargo-pool: reusing slot {} last built at {built}", s.id);
}

/// Candidate target dirs to copy into a new slot: every slot plus the main checkout's.
fn seed_sources(repo: &Repo, slots: &[Slot]) -> Vec<(PathBuf, Option<String>)> {
    let mut out: Vec<_> = slots
        .iter()
        .map(|s| {
            let head = match &s.meta.lease {
                Some(l) => git(&l.worktree, &["rev-parse", "HEAD"]).ok().or(Some(l.head.clone())),
                None => s.meta.last_head.clone(),
            };
            (s.target(), head)
        })
        .collect();
    if let Some(main) = &repo.main_worktree {
        let t = main.join("target");
        if fs::symlink_metadata(&t).is_ok_and(|m| m.is_dir()) {
            out.push((t, git(main, &["rev-parse", "HEAD"]).ok()));
        }
    }
    out.retain(|(t, _)| fs::read_dir(t).is_ok_and(|mut d| d.next().is_some()));
    out
}

/// Copy the closest non-busy target dir into the slot. Returns the commit it was built for.
fn seed(repo: &Repo, slot: &Slot, head: &str, sources: Vec<(PathBuf, Option<String>)>) -> Result<Option<String>> {
    let mut ranked: Vec<_> = sources
        .into_iter()
        .map(|(t, h)| (distance(&repo.worktree, h.as_deref(), head), last_activity(&t), t, h))
        .collect();
    ranked.sort_by_key(|(d, act, ..)| (*d, u64::MAX - act));

    let max_disk = repo.max_disk_percent();
    for (d, _, src, h) in ranked {
        // Make room for the copy first; never let seeding push the disk over the limit.
        let size = dir_size(&src);
        let protect: Vec<&Path> = [src.parent(), Some(slot.dir.as_path())].into_iter().flatten().collect();
        if !free_disk(max_disk, size, &protect)? {
            eprintln!(
                "cargo-pool: copying {} ({}) would push the disk above {max_disk}%, skipping it",
                src.display(),
                human(size)
            );
            continue;
        }
        // Hold cargo's build locks so the source isn't modified mid-copy.
        let Some(_locks) = lock_target(&src) else {
            eprintln!("cargo-pool: {} is busy building, trying next candidate", src.display());
            continue;
        };
        let dist = if d == u64::MAX { "unrelated".into() } else { format!("{d} commits away") };
        eprintln!("cargo-pool: seeding slot {} from {} ({dist}), this can take a while...", slot.id, src.display());
        let tmp = slot.dir.join("target.seeding");
        let _ = fs::remove_dir_all(&tmp);
        let started = SystemTime::now();
        let status = Command::new("cp").arg("-a").arg("--reflink=auto").arg(&src).arg(&tmp).status()?;
        if !status.success() {
            let _ = fs::remove_dir_all(&tmp);
            eprintln!("cargo-pool: copy failed, trying next candidate");
            continue;
        }
        let _ = fs::remove_dir_all(slot.target());
        fs::rename(&tmp, slot.target())?;
        let secs = started.elapsed().unwrap_or(Duration::ZERO).as_secs();
        eprintln!("cargo-pool: seeded in {secs}s");
        return Ok(h);
    }
    eprintln!("cargo-pool: no seed available, slot {} starts empty", slot.id);
    Ok(None)
}

/// `target/` patterns in .gitignore don't match symlinks, so exclude the link explicitly.
fn exclude_target(repo: &Repo) -> Result<()> {
    let info = repo.common_dir.join("info");
    let exclude = info.join("exclude");
    let current = fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == "/target") {
        return Ok(());
    }
    fs::create_dir_all(&info)?;
    let mut f = OpenOptions::new().create(true).append(true).open(&exclude)?;
    if !current.is_empty() && !current.ends_with('\n') {
        writeln!(f)?;
    }
    writeln!(f, "# cargo-pool: worktree target symlinks\n/target")?;
    Ok(())
}

fn detach(path: &Path) -> Result<()> {
    let repo = Repo::discover(path)?;
    let link = repo.worktree.join("target");
    let dest = fs::read_link(&link).map_err(|_| anyhow!("{} is not attached to a pool", link.display()))?;
    let _lock = repo.lock()?;
    let mut slots = load_slots(&repo.slots_dir())?;
    let slot = slots
        .iter_mut()
        .find(|s| s.target() == dest)
        .ok_or_else(|| anyhow!("{} points to {}, which is not a slot of this pool", link.display(), dest.display()))?;
    fs::remove_file(&link)?;
    if slot.meta.lease.as_ref().is_some_and(|l| l.worktree == repo.worktree) {
        slot.release(repo.head().ok());
        slot.save()?;
    }
    println!("cargo-pool: released slot {}", slot.id);
    Ok(())
}

// ---------------------------------------------------------------------------
// status / gc

fn short(h: &str) -> &str {
    &h[..h.len().min(10)]
}

fn ago(t: u64) -> String {
    let d = now().saturating_sub(t);
    match d {
        _ if t == 0 => "never".into(),
        0..120 => format!("{d}s ago"),
        120..7200 => format!("{}m ago", d / 60),
        7200..172800 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

fn du(p: &Path) -> String {
    Command::new("du")
        .args(["-sh", "--"])
        .arg(p)
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().next().map(String::from))
        .unwrap_or_else(|| "?".into())
}

fn status(size: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let Ok(repo) = Repo::discover(&cwd) else {
        let root = pool_root()?;
        println!("pools in {}:", root.display());
        for e in fs::read_dir(&root).into_iter().flatten().flatten() {
            let repo = fs::read_to_string(e.path().join("repo")).unwrap_or_default();
            let n = fs::read_dir(e.path().join("slots")).map_or(0, |d| d.count());
            println!("  {:<40} {n} slots  {repo}", e.file_name().to_string_lossy());
        }
        return Ok(());
    };
    let _lock = repo.lock()?;
    let mut slots = load_slots(&repo.slots_dir())?;
    reclaim_stale(&mut slots)?;
    let head = repo.head().ok();
    println!(
        "pool {} (disk at {:.1}%, limit {}%)",
        repo.pool.display(),
        disk_percent(&repo.pool, 0)?,
        repo.max_disk_percent()
    );
    if slots.is_empty() {
        println!("  (no slots yet)");
    }
    for s in &slots {
        let built = s.meta.lease.as_ref().map(|l| l.head.as_str()).or(s.meta.last_head.as_deref());
        let dist = match (&head, built) {
            (Some(h), Some(b)) => match distance(&repo.worktree, Some(b), h) {
                u64::MAX => "unrelated".into(),
                d => format!("{d} from HEAD"),
            },
            _ => String::new(),
        };
        let state = match (&s.meta.lease, s.state()) {
            (Some(l), State::Held) if l.pending => format!("seeding for {}", l.worktree.display()),
            (Some(l), State::Held) => format!("held by {}", l.worktree.display()),
            _ => "free".into(),
        };
        let size = if size { format!("  {}", du(&s.target())) } else { String::new() };
        println!(
            "  slot {:<3} {state}\n           commit {} {dist}, last build {}{size}",
            s.id,
            built.map_or("?", short),
            ago(last_activity(&s.target()).max(s.meta.last_used)),
        );
    }
    Ok(())
}

fn gc(max_age_days: Option<u64>, dry_run: bool) -> Result<()> {
    let repo = Repo::discover(&std::env::current_dir()?)?;
    let lock = repo.lock()?;
    let mut slots = load_slots(&repo.slots_dir())?;
    if !dry_run {
        reclaim_stale(&mut slots)?;
    }
    let cutoff = max_age_days.map_or(0, |d| now().saturating_sub(d * 86400));
    let removed: BTreeMap<_, _> = slots
        .iter()
        .filter(|s| s.state() != State::Held && s.meta.last_used.max(last_activity(&s.target())) < cutoff)
        .map(|s| (s.id, s.dir.clone()))
        .collect();
    for (id, dir) in &removed {
        println!("{} slot {id} ({})", if dry_run { "would remove" } else { "removing" }, dir.display());
        if !dry_run {
            fs::remove_dir_all(dir)?;
        }
    }
    if let (Some(d), true) = (max_age_days, removed.is_empty()) {
        println!("no free slots unused for more than {d} days");
    }
    drop(lock);

    let max_disk = repo.max_disk_percent();
    let pct = disk_percent(&repo.pool, 0)?;
    if dry_run {
        println!("disk at {pct:.1}% (limit {max_disk}%)");
    } else if !free_disk(max_disk, 0, &[])? {
        eprintln!("cargo-pool: warning: disk is still above {max_disk}% after cleaning up all pools");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// hooks

fn hook_path(path: &Path) -> Result<PathBuf> {
    let p = PathBuf::from(git(path, &["rev-parse", "--path-format=absolute", "--git-path", "hooks"])?);
    Ok(p.join("post-checkout"))
}

fn hook_block() -> String {
    format!(
        "{HOOK_BEGIN}\n\
         # Attach new worktrees to a pooled cargo target dir. Never fails the checkout.\n\
         if command -v cargo-pool >/dev/null 2>&1; then\n\
         \x20 cargo-pool post-checkout \"$1\" \"$2\" \"$3\" || true\n\
         fi\n\
         {HOOK_END}\n"
    )
}

fn strip_block(s: &str) -> String {
    match (s.find(HOOK_BEGIN), s.find(HOOK_END)) {
        (Some(a), Some(b)) if b > a => {
            let end = s[b..].find('\n').map_or(s.len(), |n| b + n + 1);
            format!("{}{}", &s[..a], &s[end..])
        }
        _ => s.to_string(),
    }
}

fn install_hook(path: &Path) -> Result<()> {
    let hook = hook_path(path)?;
    let existing = fs::read_to_string(&hook).unwrap_or_default();
    let mut content = strip_block(&existing);
    if content.trim().is_empty() {
        content = "#!/bin/sh\n".into();
    }
    if content.lines().any(|l| l.trim_start().starts_with("exit")) {
        eprintln!(
            "cargo-pool: warning: {} contains an `exit` statement; the cargo-pool block is appended at the end and may not run",
            hook.display()
        );
    }
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&hook_block());
    fs::create_dir_all(hook.parent().unwrap())?;
    fs::write(&hook, content)?;
    let mut perm = fs::metadata(&hook)?.permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
    fs::set_permissions(&hook, perm)?;
    println!("installed cargo-pool block in {}", hook.display());
    Ok(())
}

fn uninstall_hook(path: &Path) -> Result<()> {
    let hook = hook_path(path)?;
    let existing = fs::read_to_string(&hook).with_context(|| format!("reading {}", hook.display()))?;
    let stripped = strip_block(&existing);
    if stripped.trim() == "#!/bin/sh" || stripped.trim().is_empty() {
        fs::remove_file(&hook)?;
    } else {
        fs::write(&hook, stripped)?;
    }
    println!("removed cargo-pool block from {}", hook.display());
    Ok(())
}
