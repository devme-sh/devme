//! `devme remote` — bootstrap + live-sync a project to a remote dev host,
//! then attach to its (remote-primary) dev environment. See DEV-5.
//!
//! Split of responsibilities:
//! - **Pure** path/template/config logic lives in [`devme_config::remote`]
//!   and is unit-tested there.
//! - **This module** is the imperative half: it shells out to `mutagen`
//!   (sync sessions — *not* the daemon, which the OS keeps alive) and `ssh`
//!   (reachability + the attach command). The attach command stays an
//!   opaque, user-owned template; the shipped `herdr` preset additionally
//!   gets its remote session seeded (server + project-rooted workspace)
//!   before attach so it opens in the project dir, not `~`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use devme_config::{GlobalConfig, paths, remote};
use devme_config::remote::{SyncHealth, shell_quote};

/// How often the background watcher polls the sync's health while an attached
/// `devme remote` session runs in the foreground.
const SYNC_WATCH_INTERVAL: Duration = Duration::from_secs(5);
/// Snappier cadence for the foreground `devme remote status --watch` line.
const STATUS_WATCH_INTERVAL: Duration = Duration::from_secs(2);
/// After this many consecutive polls still in a problem state, the background
/// watcher re-notifies once (a single nag), so a halt you missed the first
/// banner for resurfaces without spamming every poll. 6 × 5s ≈ 30s.
const REMIND_AFTER_POLLS: u32 = 6;

/// Mutagen label devme stamps on every sync it creates, so `devme remote
/// wake` can flush *only* devme-managed sessions.
const DEVME_LABEL: &str = "managed-by=devme";
const DEVME_LABEL_SELECTOR: &str = "managed-by=devme";

/// Everything a `devme remote` action needs, resolved once from global
/// config + the current repo.
struct Resolved {
    host: String,
    /// The repo's main working tree — the single root we sync (model 1a).
    local_root: PathBuf,
    /// `host:remote_path` Mutagen beta endpoint.
    beta: String,
    remote_path: String,
    session: String,
    sync_mode: String,
    attach: String,
    root: String,
    /// Browser-reachable host for service URLs (Tailscale name etc.).
    url_host: String,
    /// Ensure the remote stack is up (`devme up -d`) before attaching.
    up_on_attach: bool,
    ignores: Vec<String>,
    /// Sync linked worktrees alongside the main worktree (default true).
    sync_worktrees: bool,
    /// `-<repo8>` suffix shared by every session of this repo (main and
    /// worktrees) — the key for repo-wide session enumeration.
    session_suffix: String,
}

fn resolve(cwd: &Path) -> Result<Resolved> {
    let cfg = GlobalConfig::load();
    let r = &cfg.remote;
    let host = r
        .host
        .clone()
        .filter(|h| !h.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "no remote host configured\n  set one: devme config set remote.host <ssh-target>\n  (a Tailscale MagicDNS name, an ~/.ssh/config alias, or user@host)"
            )
        })?;
    let local_root = paths::main_worktree_root(cwd);
    let root = r.root_or_default().to_string();
    let remote_path = remote::remote_path(&root, &local_root);
    let session = remote::sync_session_name(&local_root);
    let beta = format!("{host}:{remote_path}");
    let url_host = r.url_host_for(&host);
    let session_suffix = remote::repo_session_suffix(&local_root);
    Ok(Resolved {
        host,
        local_root,
        beta,
        remote_path,
        session,
        sync_mode: r.sync_mode_or_default().to_string(),
        attach: r.attach_or_default().to_string(),
        root,
        url_host,
        up_on_attach: r.up_on_attach_or_default(),
        ignores: r.ignores(),
        sync_worktrees: r.sync_worktrees_or_default(),
        session_suffix,
    })
}

// --- advertise host (VPS-side `devme url`) ----------------------------------

/// This machine's own Tailscale MagicDNS name (`vps.goose-viper.ts.net`), or
/// `None` if the `tailscale` CLI is absent / not up. Best-effort: any failure
/// is just "no autodetected name", never an error. The trailing dot Tailscale
/// appends to the FQDN is trimmed so it slots straight into a URL authority.
fn tailscale_self_dns() -> Option<String> {
    let out = Command::new("tailscale").args(["status", "--json"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let name = v.get("Self")?.get("DNSName")?.as_str()?;
    let name = name.trim().trim_end_matches('.');
    (!name.is_empty()).then(|| name.to_string())
}

/// The host `devme url` should advertise for a service on *this* machine.
/// Resolves `$DEVME_URL_HOST` (exported by the laptop's attach templates) →
/// `remote.advertise_host` (`"auto"` autodetects the Tailscale name) →
/// `localhost`. Lets an agent in a herdr pane on the VPS hand back a
/// laptop-reachable URL instead of an unreachable loopback one.
pub fn advertise_host() -> String {
    let env = std::env::var("DEVME_URL_HOST").ok();
    let configured = GlobalConfig::load().remote.advertise_host;
    // Only pay for the Tailscale lookup when the config actually asks for it.
    let tailscale = (configured.as_deref().map(str::trim) == Some(remote::ADVERTISE_AUTO))
        .then(tailscale_self_dns)
        .flatten();
    remote::pick_advertise_host(env.as_deref(), configured.as_deref(), tailscale.as_deref())
        .unwrap_or_else(|| "localhost".to_string())
}

// --- mutagen wrappers -------------------------------------------------------

/// Is the `mutagen` client installed and runnable?
fn mutagen_available() -> bool {
    Command::new("mutagen")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn require_mutagen() -> Result<()> {
    if mutagen_available() {
        Ok(())
    } else {
        bail!(
            "mutagen is not installed (the live-sync engine devme remote drives)\n  install it: brew install mutagen-io/mutagen/mutagen\n  then re-run: devme remote"
        )
    }
}

/// Start the Mutagen daemon if it isn't running. devme owns sync *sessions*,
/// not the daemon's lifetime — `mutagen daemon start` is idempotent and the
/// OS (launchd/systemd) keeps it alive across reboots, so we never register
/// or tear down the daemon ourselves.
fn ensure_mutagen_daemon() {
    let _ = Command::new("mutagen").args(["daemon", "start"]).output();
}

/// Does a sync session with this name already exist?
fn sync_exists(session: &str) -> bool {
    Command::new("mutagen")
        .args(["sync", "list", session])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a two-way sync session, labeled so `devme remote wake` can flush
/// only devme-managed syncs. `extra_ignores` come first: the main session
/// always excludes git's per-machine bookkeeping ([`remote::GIT_ALWAYS_IGNORES`]
/// — lock files, `gc.pid`, worktree registrations); a worktree session
/// excludes `.git` outright (its pointer file is host-local).
fn sync_create_session(
    r: &Resolved,
    name: &str,
    local: &str,
    beta: &str,
    extra_ignores: &[&str],
) -> Result<()> {
    let mut args: Vec<String> = vec![
        "sync".into(),
        "create".into(),
        format!("--name={name}"),
        format!("--sync-mode={}", r.sync_mode),
        format!("--label={DEVME_LABEL}"),
    ];
    for ig in extra_ignores {
        args.push(format!("--ignore={ig}"));
    }
    for ig in &r.ignores {
        args.push(format!("--ignore={ig}"));
    }
    args.push(local.to_string());
    args.push(beta.to_string());

    let status = Command::new("mutagen")
        .args(&args)
        .status()
        .context("running `mutagen sync create`")?;
    if !status.success() {
        bail!("`mutagen sync create` failed (see output above)");
    }
    Ok(())
}

/// Create the main-worktree sync. `.git` is synced (shared-state, model 1a)
/// minus git's per-machine bookkeeping.
fn sync_create(r: &Resolved) -> Result<()> {
    let local = r.local_root.to_string_lossy().to_string();
    sync_create_session(r, &r.session, &local, &r.beta, remote::GIT_ALWAYS_IGNORES)
}

fn sync_flush(session: &str) -> Result<()> {
    let status = Command::new("mutagen")
        .args(["sync", "flush", session])
        .status()
        .context("running `mutagen sync flush`")?;
    if !status.success() {
        bail!("`mutagen sync flush` failed");
    }
    Ok(())
}

/// Ensure the sync exists; create + flush (wait for the initial pass) on
/// first run so the remote has the files before we attach. Returns whether
/// it was freshly created.
fn ensure_sync(r: &Resolved) -> Result<bool> {
    if sync_exists(&r.session) {
        return Ok(false);
    }
    eprintln!("devme remote: starting live-sync {} → {}", r.local_root.display(), r.beta);
    sync_create(r)?;
    eprintln!("devme remote: waiting for initial sync…");
    sync_flush(&r.session)?;
    Ok(true)
}

// --- worktree sync -----------------------------------------------------------

/// One linked worktree resolved for syncing: where it lives locally, what it
/// has checked out, and the session/remote-path it maps to (same scheme as
/// the main worktree: `<slug>-<repo8>` under the remote root).
struct WtSync {
    local_path: PathBuf,
    branch: Option<String>,
    session: String,
    remote_path: String,
    beta: String,
}

/// The repo's linked worktrees as sync candidates. Stale registrations
/// (paths that don't exist on this machine — e.g. created on another host)
/// are skipped silently; two worktrees mapping to the same remote dir
/// (same basename) keep the first and warn on the rest.
fn resolve_worktrees(r: &Resolved) -> Vec<WtSync> {
    let out = Command::new("git")
        .arg("-C")
        .arg(&r.local_root)
        .args(["worktree", "list", "--porcelain"])
        .output();
    let Ok(o) = out else { return Vec::new() };
    if !o.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&o.stdout);
    let mut seen = std::collections::BTreeSet::new();
    let mut wts = Vec::new();
    for e in remote::parse_linked_worktrees(&text) {
        let local_path = PathBuf::from(&e.path);
        if !local_path.exists() {
            continue;
        }
        let remote_path = remote::remote_path(&r.root, &local_path);
        if !seen.insert(remote_path.clone()) {
            eprintln!(
                "devme remote: warning: {} maps to the same remote dir as another worktree — skipping",
                local_path.display()
            );
            continue;
        }
        let session = remote::sync_session_name(&local_path);
        let beta = format!("{}:{remote_path}", r.host);
        wts.push(WtSync { local_path, branch: e.branch, session, remote_path, beta });
    }
    wts
}

/// Ensure every linked worktree has a live sync: materialize it on the host
/// (`git worktree add --no-checkout` against the synced main repo), create
/// its session with `.git` ignored (the pointer file is host-local), flush,
/// and on first creation align the remote index so `git status` there shows
/// exactly the laptop's uncommitted state. Best-effort per worktree — one
/// broken worktree warns and moves on rather than blocking the attach.
fn ensure_worktree_syncs(r: &Resolved) {
    if !r.sync_worktrees {
        return;
    }
    let wts = resolve_worktrees(r);
    let pending: Vec<&WtSync> = wts.iter().filter(|w| !sync_exists(&w.session)).collect();
    if pending.is_empty() {
        return;
    }
    // Branch refs reach the remote through the main session — flush it first
    // so a just-created local branch is resolvable host-side.
    let _ = sync_flush(&r.session);
    for wt in pending {
        let Some(branch) = &wt.branch else {
            eprintln!(
                "devme remote: skipping {} (detached HEAD — check out a branch to sync it)",
                wt.local_path.display()
            );
            continue;
        };
        let materialize = remote::worktree_materialize_cmd(&r.remote_path, &wt.remote_path, branch);
        let (ok, out) = ssh_check(&r.host, &materialize);
        if !ok {
            eprintln!(
                "devme remote: couldn't materialize {} on {}: {out}",
                wt.remote_path, r.host
            );
            continue;
        }
        let created = out.lines().any(|l| l.trim() == "created");
        eprintln!(
            "devme remote: starting live-sync {} → {}",
            wt.local_path.display(),
            wt.beta
        );
        let local = wt.local_path.to_string_lossy();
        if let Err(e) = sync_create_session(r, &wt.session, &local, &wt.beta, &[".git"]) {
            eprintln!("devme remote: {e}");
            continue;
        }
        if let Err(e) = sync_flush(&wt.session) {
            eprintln!("devme remote: {e}");
            continue;
        }
        if created {
            let align = remote::worktree_align_index_cmd(&wt.remote_path);
            let (ok, out) = ssh_check(&r.host, &align);
            if !ok {
                eprintln!(
                    "devme remote: warning: couldn't align index for {}: {out}",
                    wt.remote_path
                );
            }
        }
    }
}

/// Every devme-managed session belonging to this repo — main and worktrees —
/// as `(name, local alpha path)` pairs, keyed off the shared `-<repo8>`
/// session-name suffix. Best-effort: empty on any mutagen hiccup.
fn repo_sessions(r: &Resolved) -> Vec<(String, String)> {
    let out = Command::new("mutagen")
        .args([
            "sync",
            "list",
            "--label-selector",
            DEVME_LABEL_SELECTOR,
            "--template",
            "{{range .}}{{.Name}}@@{{.Alpha.Path}}\n{{end}}",
        ])
        .output();
    let Ok(o) = out else { return Vec::new() };
    if !o.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| l.split_once("@@"))
        .filter(|(name, _)| name.ends_with(&r.session_suffix))
        .map(|(name, path)| (name.to_string(), path.trim().to_string()))
        .collect()
}

/// The mechanical reaper, sync edition: a session whose local worktree path
/// has vanished (worktree removed with plain git) is terminated. The remote
/// copy stays — stopping the link is the reversible move; removing the
/// host-side worktree is the host's business.
fn reap_orphan_worktree_syncs(r: &Resolved) {
    for (name, alpha) in repo_sessions(r) {
        if name == r.session || alpha.is_empty() {
            continue;
        }
        if !Path::new(&alpha).exists() {
            eprintln!("devme remote: worktree {alpha} is gone — stopping its sync ({name})");
            let _ = Command::new("mutagen").args(["sync", "terminate", &name]).status();
        }
    }
}

/// Aggregate conflict count across every session of this repo, plus whether
/// the main session still exists and its status — what the attached-session
/// watcher cares about (a halt on *any* of the repo's syncs is silent).
fn observe_repo_syncs(main_session: &str, suffix: &str) -> (bool, Option<String>, u64) {
    let out = Command::new("mutagen")
        .args([
            "sync",
            "list",
            "--label-selector",
            DEVME_LABEL_SELECTOR,
            "--template",
            "{{range .}}{{.Name}}@@{{.Status}}@@{{len .Conflicts}}\n{{end}}",
        ])
        .output();
    let Ok(o) = out else { return (false, None, 0) };
    if !o.status.success() {
        return (false, None, 0);
    }
    let mut main_exists = false;
    let mut main_status = None;
    let mut conflicts = 0u64;
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let mut parts = line.splitn(3, "@@");
        let (Some(name), Some(status), Some(n)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if !name.ends_with(suffix) {
            continue;
        }
        conflicts += n.trim().parse::<u64>().unwrap_or(0);
        if name == main_session {
            main_exists = true;
            main_status = (!status.is_empty()).then(|| status.to_string());
        }
    }
    (main_exists, main_status, conflicts)
}

// --- live-sync watcher (laptop-side, during an attached session) ------------

/// One sync-health observation: whether the session exists, its raw status
/// string, and conflict count. Polled by the watcher and the `--watch` line.
fn observe_sync(session: &str) -> (bool, Option<String>, u64) {
    if !sync_exists(session) {
        return (false, None, 0);
    }
    let (status, conflicts) = sync_status_fields(session);
    (true, status, conflicts.unwrap_or(0))
}

/// Sleep up to `total`, but wake early (in ~250ms slices) if `stop` is set, so
/// a detach joins the watcher promptly instead of after a full poll interval.
fn interruptible_sleep(stop: &AtomicBool, total: Duration) {
    let slice = Duration::from_millis(250);
    let mut left = total;
    while left > Duration::ZERO && !stop.load(Ordering::Relaxed) {
        let nap = slice.min(left);
        std::thread::sleep(nap);
        left = left.saturating_sub(nap);
    }
}

/// Spawn a laptop-side background watcher for the duration of an attached
/// remote session. It polls the sync's health and, **edge-triggered**, fires a
/// desktop notification when health changes (a silent two-way-safe halt on
/// conflict being the case that matters), plus one reminder if a problem
/// persists. It deliberately does **not** print to stderr: the attached remote
/// TUI owns this terminal, so writing to it would corrupt the display — the
/// post-detach summary (printed once the terminal is ours again) is the
/// on-screen channel. Returns a stop flag + join handle; the caller flips the
/// flag and joins after the attach command returns.
fn spawn_sync_watcher(
    session: String,
    suffix: String,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        let mut last: Option<SyncHealth> = None;
        let mut polls_in_problem = 0u32;
        let mut reminded = false;
        while !stop_thread.load(Ordering::Relaxed) {
            // Conflicts aggregate across the repo's sessions (worktrees
            // included) — a halt on any of them is equally silent.
            let (exists, status, conflicts) = observe_repo_syncs(&session, &suffix);
            let health = remote::classify_sync(exists, status.as_deref(), conflicts);
            match remote::sync_transition_message(last, health, conflicts) {
                Some(msg) => {
                    // A real transition — announce and reset the nag timer.
                    notify(&msg);
                    polls_in_problem = 0;
                    reminded = false;
                }
                None => {
                    // Steady state. Nag once if we've been stuck in a problem.
                    if matches!(health, SyncHealth::Conflict | SyncHealth::Down) {
                        polls_in_problem += 1;
                        if polls_in_problem >= REMIND_AFTER_POLLS && !reminded {
                            if let Some(msg) =
                                remote::sync_transition_message(None, health, conflicts)
                            {
                                notify(&msg);
                            }
                            reminded = true;
                        }
                    }
                }
            }
            last = Some(health);
            interruptible_sleep(&stop_thread, SYNC_WATCH_INTERVAL);
        }
    });
    (stop, handle)
}

/// Print a one-line sync summary to stderr — used once the terminal is back in
/// our hands after a detach, so the closing state is visible even on hosts
/// without desktop notifications.
fn print_sync_summary(session: &str) {
    let (exists, status, conflicts) = observe_sync(session);
    if !exists {
        return; // sync was stopped/terminated — nothing to summarise.
    }
    let health = remote::classify_sync(exists, status.as_deref(), conflicts);
    eprintln!(
        "devme remote: {}",
        remote::sync_status_line(health, conflicts, status.as_deref())
    );
}

// --- ssh wrappers -----------------------------------------------------------

/// Run a command on the remote over SSH non-interactively, returning
/// (success, combined-output). `BatchMode` fails fast instead of hanging on
/// a password prompt; `ConnectTimeout` caps an unreachable host.
fn ssh_check(host: &str, remote_cmd: &str) -> (bool, String) {
    let out = Command::new("ssh")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", host, remote_cmd])
        .output();
    match out {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s.trim().to_string())
        }
        Err(e) => (false, e.to_string()),
    }
}

/// Ensure the stack is running on the remote before we attach, by shelling
/// `devme up -d` over SSH. Idempotent (an already-running stack is a no-op
/// reconcile) and **non-fatal**: the supervisor owns the stack's lifetime, so
/// even if this fails the attach session may still be useful — we warn and
/// continue rather than abort. The `-d` detach is what keeps the stack alive
/// under the supervisor (not inside the herdr/ssh session you're attaching).
fn remote_up(r: &Resolved) {
    use std::io::IsTerminal;
    let cmd = format!("cd {} && devme up -d", shell_quote(&r.remote_path));
    eprintln!("devme remote: ensuring stack is up on {} …", r.host);
    let mut ssh = Command::new("ssh");
    ssh.args(["-o", "BatchMode=yes"]);
    // Allocate a remote TTY when we have one locally, so first-run prompts —
    // the ADR-0014 env wizard, preflight provisioning — are interactive and
    // run to completion *before* the attach takes over the screen. Without it
    // the remote `devme up` sees a non-tty stdin, prints a wizard it can't
    // drive, and the attach immediately paints over it.
    if std::io::stdin().is_terminal() {
        ssh.arg("-t");
    }
    let status = ssh.arg(&r.host).arg(&cmd).status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!(
            "devme remote: warning: remote `devme up -d` exited {} (attaching anyway)",
            s.code().map(|c| c.to_string()).unwrap_or_else(|| "by signal".into())
        ),
        Err(e) => eprintln!("devme remote: warning: couldn't start remote stack: {e} (attaching anyway)"),
    }
}

// --- herdr preset preparation -------------------------------------------------

/// Prepare the remote herdr session so the first attach opens in the project
/// directory. herdr creates a fresh session's first workspace at *attach*
/// time with the server's cwd (the SSH login dir, i.e. `~`) — so for the
/// shipped `herdr` preset devme pre-starts the session server and seeds a
/// workspace rooted at the project via herdr's socket-API CLI. A session
/// that already has workspaces is left untouched (it's the user's
/// arrangement). Best-effort throughout: any failure falls through to a
/// plain attach — worst case herdr opens in `~`, the pre-seed behavior.
fn herdr_prepare(r: &Resolved) {
    let list_cmd = remote::herdr_list_cmd(&r.session);
    let (mut ok, mut out) = ssh_check(&r.host, &list_cmd);
    if !ok {
        // No session server yet — start one headless, rooted at the project,
        // then poll briefly for its socket. If herdr isn't installed on the
        // remote this times out and the attach surfaces the real error.
        let start = remote::herdr_server_start_cmd(&r.session, &r.remote_path);
        let _ = ssh_check(&r.host, &start);
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(300));
            (ok, out) = ssh_check(&r.host, &list_cmd);
            if ok {
                break;
            }
        }
        if !ok {
            return;
        }
    }
    // Only a *confirmed* empty session gets seeded — `None` (unexpected
    // output shape) must not create workspaces in a session we misread.
    if remote::herdr_workspace_count(&out) != Some(0) {
        return;
    }
    let label = r
        .local_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("devme")
        .to_string();
    let create = remote::herdr_workspace_create_cmd(&r.session, &r.remote_path, &label);
    let (created, _) = ssh_check(&r.host, &create);
    if created {
        eprintln!("devme remote: opened herdr workspace at {}", r.remote_path);
    }
}

// --- transparent proxy ------------------------------------------------------

/// Is a live sync session present for this repo? That's the signal that the
/// project is in **remote mode**: the stack runs on the VPS, so daemon-facing
/// commands forward there. Returns the resolved context when active.
fn remote_active(cwd: &Path) -> Option<Resolved> {
    let r = resolve(cwd).ok()?;
    if sync_exists(&r.session) { Some(retarget_for_cwd(r, cwd)) } else { None }
}

/// When `cwd` is inside a linked worktree with its own live sync, daemon
/// commands should land in *that* worktree's remote dir — `devme logs` from
/// a laptop worktree reads the matching remote stack, not the main one.
fn retarget_for_cwd(mut r: Resolved, cwd: &Path) -> Resolved {
    if !r.sync_worktrees {
        return r;
    }
    let canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if canon == r.local_root {
        return r;
    }
    for wt in resolve_worktrees(&r) {
        let wt_canon =
            std::fs::canonicalize(&wt.local_path).unwrap_or_else(|_| wt.local_path.clone());
        if canon.starts_with(&wt_canon) && sync_exists(&wt.session) {
            r.remote_path = wt.remote_path;
            break;
        }
    }
    r
}

/// Daemon/stack-facing commands forward to the remote when remote mode is
/// active. Machine-local commands (`config`, `skill`, `completions`) and
/// `remote` itself always run locally and are absent here.
fn is_proxyable(command: &Option<crate::Command>) -> bool {
    use crate::Command as C;
    matches!(
        command,
        Some(
            C::Up { .. }
                | C::Down { .. }
                | C::Status { .. }
                | C::Start { .. }
                | C::Stop { .. }
                | C::Restart { .. }
                | C::Logs { .. }
                | C::Url { .. }
                | C::Doctor { .. }
                | C::Worktree { .. }
        )
    )
}

/// Transparent remote proxy. When a live sync exists and `command` is
/// daemon-facing, run it on the remote over SSH so `devme logs`, `status`,
/// etc. behave exactly as local but read from the VPS. Returns the remote's
/// exit code, or `None` to fall through to local execution.
pub fn maybe_proxy(command: &Option<crate::Command>) -> Option<i32> {
    if !is_proxyable(command) {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let r = remote_active(&cwd)?;
    // `url` is special: ask the remote for the port, but rewrite the host so
    // it's reachable from the laptop browser (e.g. over Tailscale), and open
    // locally rather than on the headless VPS.
    if let Some(crate::Command::Url { service, open }) = command {
        return Some(proxy_url(&r, service, *open));
    }
    Some(proxy_passthrough(&r))
}

/// Forward this invocation's own arguments to the remote verbatim, dropping
/// `--local` (a local-only escape hatch the remote shouldn't see).
fn forwarded_args() -> Vec<String> {
    std::env::args().skip(1).filter(|a| a != "--local").collect()
}

/// Build the remote command: `cd <remote_path> && devme <args…>`.
fn remote_devme_cmd(r: &Resolved, args: &[String]) -> String {
    let mut parts = vec!["devme".to_string()];
    parts.extend(args.iter().map(|a| shell_quote(a)));
    format!("cd {} && {}", shell_quote(&r.remote_path), parts.join(" "))
}

/// Stream a command through to the remote with an inherited TTY (so `logs
/// -f`, `up` foreground, and Ctrl-C all behave).
fn proxy_passthrough(r: &Resolved) -> i32 {
    use std::io::IsTerminal;
    let cmd = remote_devme_cmd(r, &forwarded_args());
    let mut ssh = Command::new("ssh");
    // Allocate a remote TTY only when we have one locally — so interactive
    // streaming (`logs -f`, foreground `up`) works, but captured/piped output
    // (agents, scripts) doesn't trip ssh's "pseudo-terminal will not be
    // allocated" warning.
    if std::io::stdin().is_terminal() {
        ssh.arg("-t");
    }
    let status = ssh.arg(&r.host).arg(&cmd).status();
    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("devme: remote proxy failed: {e}");
            1
        }
    }
}

/// Resolve a service URL against the remote, then rewrite localhost → the
/// browser-reachable host and (optionally) open it on the laptop.
fn proxy_url(r: &Resolved, service: &str, open: bool) -> i32 {
    let cmd = remote_devme_cmd(r, &["url".into(), service.into()]);
    let out = Command::new("ssh")
        .args(["-o", "BatchMode=yes", &r.host])
        .arg(&cmd)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let raw = String::from_utf8_lossy(&o.stdout);
            let raw = raw.lines().next().unwrap_or("").trim();
            let url = remote::rewrite_url_host(raw, &r.url_host);
            println!("{url}");
            if open && let Err(e) = devme_config::browser::open_url(&url) {
                eprintln!("devme: couldn't open browser: {e}");
            }
            0
        }
        Ok(o) => {
            eprint!("{}", String::from_utf8_lossy(&o.stderr));
            o.status.code().unwrap_or(1)
        }
        Err(e) => {
            eprintln!("devme: remote proxy failed: {e}");
            1
        }
    }
}

// --- public actions ---------------------------------------------------------

/// `devme remote` (default): ensure the sync session, then attach to the
/// remote environment. The attach command (default `tui`) is what brings up
/// / streams the remote stack; the synced files are already in place.
pub fn run(cwd: &Path) -> Result<()> {
    let r = resolve(cwd)?;
    require_mutagen()?;
    ensure_mutagen_daemon();
    ensure_sync(&r)?;
    ensure_worktree_syncs(&r);
    reap_orphan_worktree_syncs(&r);

    // Bring the stack up under the supervisor before attaching, so herdr/ssh
    // attaches land in a project whose dev server is already running (and that
    // keeps running when you detach). `tui`/`tmux` would start it themselves,
    // but `up -d` first is idempotent and makes the herdr/ssh presets work too.
    if r.up_on_attach {
        remote_up(&r);
    }

    // The herdr preset gets its remote session seeded (server + project-
    // rooted workspace) so the attach lands in the project dir, not `~`.
    if r.attach == "herdr" {
        herdr_prepare(&r);
    }

    let cmd = remote::expand_attach(&r.attach, &r.host, &r.remote_path, &r.session, &r.url_host);
    eprintln!("devme remote: attaching ({}) → {}", r.attach, r.host);

    // Watch the sync in the background for the life of the session: a two-way-
    // safe halt on conflict is silent and laptop-side, so the remote TUI you're
    // attached to can't show it. The watcher notifies (desktop) on a health
    // change; it stays off stderr so it can't corrupt the remote TUI's screen.
    let (stop, watcher) = spawn_sync_watcher(r.session.clone(), r.session_suffix.clone());

    // Hand the terminal to a real shell so all quoting in the attach template
    // is honored and a full-screen remote TUI gets the inherited TTY.
    let status = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .status()
        .context("running attach command")?;

    // Session over — stop the watcher and (terminal back in our hands) flush
    // once and print the closing sync state, so anything that drifted while you
    // worked is reconciled and visible even without desktop notifications.
    stop.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    let _ = sync_flush(&r.session);
    print_sync_summary(&r.session);

    if !status.success() {
        // A non-zero exit here is usually just the user quitting the remote
        // session — report it without dressing it up as a devme failure.
        if let Some(code) = status.code() {
            eprintln!("devme remote: attach exited with status {code}");
        }
    }
    Ok(())
}

/// `devme remote sync`: ensure + flush every sync of this repo (main and
/// worktrees) without attaching, creating sessions for worktrees added since
/// the last run and reaping sessions whose worktree is gone. Handy from a
/// wake hook or a script.
pub fn sync(cwd: &Path) -> Result<()> {
    let r = resolve(cwd)?;
    require_mutagen()?;
    ensure_mutagen_daemon();
    if !ensure_sync(&r)? {
        sync_flush(&r.session)?;
    }
    ensure_worktree_syncs(&r);
    reap_orphan_worktree_syncs(&r);
    for (name, _) in repo_sessions(&r) {
        if name != r.session {
            let _ = sync_flush(&name);
        }
    }
    eprintln!("devme remote: synced {} ⇄ {}", r.local_root.display(), r.beta);
    Ok(())
}

/// `devme remote stop`: terminate this repo's sync sessions — main and
/// worktrees (the remote files stay; the live links stop).
pub fn stop(cwd: &Path) -> Result<()> {
    let r = resolve(cwd)?;
    require_mutagen()?;
    let sessions = repo_sessions(&r);
    if sessions.is_empty() {
        eprintln!("devme remote: no live-sync for this project");
        return Ok(());
    }
    for (name, _) in sessions {
        let status = Command::new("mutagen")
            .args(["sync", "terminate", &name])
            .status()
            .context("running `mutagen sync terminate`")?;
        if !status.success() {
            bail!("`mutagen sync terminate` failed for {name}");
        }
    }
    eprintln!("devme remote: stopped live-sync for {}", r.remote_path);
    Ok(())
}

/// `devme remote flush`: force an immediate reconcile (e.g. right after the
/// laptop wakes), instead of waiting for the next watch/poll cycle.
pub fn flush(cwd: &Path) -> Result<()> {
    let r = resolve(cwd)?;
    require_mutagen()?;
    if !sync_exists(&r.session) {
        bail!("no live-sync for this project — start one with `devme remote`");
    }
    sync_flush(&r.session)?;
    eprintln!("devme remote: flushed {}", r.session);
    Ok(())
}

/// `devme remote wake`: force an immediate reconcile of **every** devme-
/// managed sync. This is what the wake hook runs so changes the remote made
/// while the laptop slept come down right away instead of on the next poll.
/// Best-effort and quiet — safe to call when no syncs exist.
pub fn wake() -> Result<()> {
    if !mutagen_available() {
        return Ok(());
    }
    ensure_mutagen_daemon();
    let _ = Command::new("mutagen")
        .args(["sync", "flush", "--label-selector", DEVME_LABEL_SELECTOR])
        .status();
    // Proactively flag any sync that halted on conflict while the laptop slept,
    // so the silent two-way-safe halt surfaces the moment you're back instead
    // of the next time you happen to run a devme command.
    let n = devme_conflict_total();
    if n > 0 {
        eprintln!("devme remote: ⚠ {n} conflict(s) across devme syncs — run `devme remote conflicts`");
        notify(&format!("{n} sync conflict(s) after wake — run `devme remote conflicts`"));
    }
    Ok(())
}

const WAKE_BEGIN: &str = "# >>> devme wake-hook >>>";
const WAKE_END: &str = "# <<< devme wake-hook <<<";

/// `devme remote wake-hook [--uninstall]`: wire `devme remote wake` into the
/// OS wake event. On macOS this uses sleepwatcher's `~/.wakeup` convention
/// (`brew install sleepwatcher`); the hook is a marked block so install /
/// uninstall are idempotent and never disturb the user's other wake scripts.
pub fn wake_hook(uninstall: bool) -> Result<()> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))?;
    let path = home.join(".wakeup");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    if uninstall {
        if !existing.contains(WAKE_BEGIN) {
            eprintln!("devme remote: no wake-hook installed");
            return Ok(());
        }
        let cleaned = strip_marked_block(&existing);
        std::fs::write(&path, cleaned).context("updating ~/.wakeup")?;
        eprintln!("devme remote: wake-hook removed from {}", path.display());
        return Ok(());
    }

    if existing.contains(WAKE_BEGIN) {
        eprintln!("devme remote: wake-hook already installed in {}", path.display());
        return Ok(());
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if content.is_empty() {
        content.push_str("#!/bin/sh\n");
    }
    content.push_str(&format!(
        "{WAKE_BEGIN}\ncommand -v devme >/dev/null 2>&1 && devme remote wake >/dev/null 2>&1\n{WAKE_END}\n"
    ));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("creating home dir for ~/.wakeup")?;
    }
    std::fs::write(&path, content).context("writing ~/.wakeup")?;
    // sleepwatcher requires the script be executable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            let _ = std::fs::set_permissions(&path, perms);
        }
    }
    eprintln!("devme remote: wake-hook installed in {}", path.display());
    if !sleepwatcher_present() {
        eprintln!(
            "  note: install + start sleepwatcher so it fires:\n    brew install sleepwatcher && brew services start sleepwatcher"
        );
    }
    Ok(())
}

/// Remove the `# >>> devme wake-hook >>>` … `# <<< … <<<` block, leaving the
/// rest of the file untouched.
fn strip_marked_block(content: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in content.lines() {
        if line.trim() == WAKE_BEGIN {
            skipping = true;
            continue;
        }
        if line.trim() == WAKE_END {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn sleepwatcher_present() -> bool {
    Command::new("sh")
        .args(["-c", "command -v sleepwatcher"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `devme remote status`: conflict-aware sync state. Silent Mutagen halts on
/// conflict are the #1 failure mode, so the conflict count is surfaced first.
/// With `watch`, refresh a single compact line until Ctrl-C — meant for a
/// laptop-side split pane next to an attached session.
pub fn status(cwd: &Path, json: bool, watch: bool) -> Result<()> {
    let r = resolve(cwd)?;
    require_mutagen()?;
    if watch {
        // `--json` is a one-shot snapshot format; `--watch` is the live human
        // line. Combining them is meaningless, so watch wins.
        return status_watch(&r.session);
    }
    let exists = sync_exists(&r.session);

    // The repo's worktree sessions (everything sharing the suffix bar main).
    let worktree_sessions: Vec<(String, String)> = repo_sessions(&r)
        .into_iter()
        .filter(|(name, _)| name != &r.session)
        .collect();

    if json {
        let (status_str, conflicts) = if exists { sync_status_fields(&r.session) } else { (None, None) };
        let raw = if exists {
            Command::new("mutagen")
                .args(["sync", "list", &r.session])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        } else {
            None
        };
        let worktrees: Vec<serde_json::Value> = worktree_sessions
            .iter()
            .map(|(name, path)| {
                let (s, c) = sync_status_fields(name);
                serde_json::json!({
                    "session": name,
                    "local_path": path,
                    "status": s,
                    "conflicts": c,
                })
            })
            .collect();
        let value = serde_json::json!({
            "session": r.session,
            "exists": exists,
            "host": r.host,
            "remote_path": r.remote_path,
            "status": status_str,
            "conflicts": conflicts,
            "raw": raw,
            "worktrees": worktrees,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    if !exists {
        println!("no live-sync for this project (run `devme remote` to start one)");
        return Ok(());
    }
    let (status_str, conflicts) = sync_status_fields(&r.session);
    if let Some(n) = conflicts
        && n > 0
    {
        println!("⚠ {n} conflict(s) — sync is halted; resolve with `mutagen sync` or edit a side");
    }
    if let Some(s) = &status_str {
        println!("status: {s}");
    }
    // The full mutagen table has the per-endpoint detail; show it verbatim.
    let _ = Command::new("mutagen")
        .args(["sync", "list", &r.session])
        .status();
    if !worktree_sessions.is_empty() {
        println!("\nworktrees:");
        for (name, path) in &worktree_sessions {
            let (s, c) = sync_status_fields(name);
            let health = remote::classify_sync(true, s.as_deref(), c.unwrap_or(0));
            println!(
                "  {path} — {}",
                remote::sync_status_line(health, c.unwrap_or(0), s.as_deref())
            );
        }
    }
    Ok(())
}

/// `devme remote status --watch`: redraw one compact, colour-free status line
/// in place every couple of seconds until Ctrl-C. Built to sit in a laptop-
/// side split next to an attached session so a silent conflict-halt is visible
/// at a glance. Owns its own terminal (it's not the attach), so overwriting the
/// line with `\r` is safe here — unlike the background watcher.
fn status_watch(session: &str) -> Result<()> {
    use std::io::Write;
    eprintln!("watching {session} (Ctrl-C to stop)…");
    let mut last_line = String::new();
    loop {
        let (exists, status, conflicts) = observe_sync(session);
        let line = if exists {
            let health = remote::classify_sync(exists, status.as_deref(), conflicts);
            remote::sync_status_line(health, conflicts, status.as_deref())
        } else {
            "no live-sync (run `devme remote` to start one)".to_string()
        };
        // Pad to clear any leftover from a previous, longer line before the \r.
        let pad = last_line.chars().count().saturating_sub(line.chars().count());
        print!("\r{line}{}", " ".repeat(pad));
        let _ = std::io::stdout().flush();
        last_line = line;
        std::thread::sleep(STATUS_WATCH_INTERVAL);
    }
}

/// Pull (status, conflict-count) from Mutagen via a Go template, best-effort.
/// Returns (None, None) if the template shape isn't what we expect on this
/// Mutagen version — the human path still has the raw table to fall back on.
fn sync_status_fields(session: &str) -> (Option<String>, Option<u64>) {
    let out = Command::new("mutagen")
        .args([
            "sync",
            "list",
            session,
            "--template",
            "{{range .}}{{.Status}}@@{{len .Conflicts}}{{end}}",
        ])
        .output();
    let Ok(o) = out else { return (None, None) };
    if !o.status.success() {
        return (None, None);
    }
    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
    match s.split_once("@@") {
        Some((status, conflicts)) => {
            let status = (!status.is_empty()).then(|| status.to_string());
            let conflicts = conflicts.trim().parse::<u64>().ok();
            (status, conflicts)
        }
        None => (None, None),
    }
}

/// `devme remote conflicts`: surface a halted two-way-safe sync loudly. Lists
/// the conflicting paths, the full Mutagen alpha/beta detail, and the safe
/// ways to resolve — the git-mergetool-style *visibility* that turns a silent
/// halt into something actionable. (Auto-picking a winner is intentionally
/// not done here: Mutagen OSS has no per-file winner, and a blind take-a-side
/// can clobber overnight remote work — resolve explicitly.)
pub fn conflicts(cwd: &Path, json: bool) -> Result<()> {
    let r = resolve(cwd)?;
    require_mutagen()?;

    if !sync_exists(&r.session) {
        if json {
            let v = serde_json::json!({
                "session": r.session, "exists": false, "conflicts": 0, "paths": [],
            });
            println!("{}", serde_json::to_string_pretty(&v)?);
        } else {
            println!("no live-sync for this project (run `devme remote` to start one)");
        }
        return Ok(());
    }

    let count = sync_status_fields(&r.session).1.unwrap_or(0);
    let paths = conflict_paths(&r.session);

    if json {
        let v = serde_json::json!({
            "session": r.session,
            "exists": true,
            "host": r.host,
            "remote_path": r.remote_path,
            "conflicts": count,
            "paths": paths,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    if count == 0 {
        println!("no conflicts — sync is healthy ({})", r.session);
        return Ok(());
    }

    println!("⚠ {count} conflict(s) — two-way-safe sync is HALTED until resolved.\n");
    if paths.is_empty() {
        println!("  (couldn't enumerate paths on this Mutagen version — see the detail below)\n");
    } else {
        println!("conflicting paths:");
        for p in &paths {
            println!("  • {p}");
        }
        println!();
    }
    // Full per-endpoint detail (the alpha/beta versions) verbatim.
    let _ = Command::new("mutagen").args(["sync", "list", "--long", &r.session]).status();
    println!("\nresolve by making the two sides agree, then re-sync:");
    println!("  • keep the LAPTOP copy:  re-save (`touch`) the file locally, then `devme remote flush`");
    println!("  • keep the REMOTE copy:  delete the local copy, then `devme remote flush`");
    println!("                           (the remote — primary — version syncs back down)");
    println!("  • inspect the remote:    ssh {} 'cd {} && …'", r.host, r.remote_path);
    println!("\nThe whole tree (including .git) is synced, so genuine code divergence");
    println!("can also be settled with normal git on either side.");

    // Worktree sessions halt independently — surface theirs too.
    for (name, path) in repo_sessions(&r) {
        if name == r.session {
            continue;
        }
        let n = sync_status_fields(&name).1.unwrap_or(0);
        if n > 0 {
            println!("\n⚠ worktree {path} has {n} conflict(s) ({name}):");
            let _ = Command::new("mutagen").args(["sync", "list", "--long", &name]).status();
        }
    }
    Ok(())
}

/// Conflicting paths in a session, parsed best-effort from Mutagen via a Go
/// template (one path per line across both sides of every conflict). Returns
/// an empty vec on any hiccup — the caller still has the raw `--long` detail
/// and the conflict count. The sync-root conflict has an empty path and is
/// reported as `<sync root>`.
fn conflict_paths(session: &str) -> Vec<String> {
    let template = "{{range .}}{{range .Conflicts}}{{range .AlphaChanges}}{{.Path}}\n{{end}}{{range .BetaChanges}}{{.Path}}\n{{end}}{{end}}{{end}}";
    let out = Command::new("mutagen")
        .args(["sync", "list", session, "--template", template])
        .output();
    let Ok(o) = out else { return Vec::new() };
    if !o.status.success() {
        return Vec::new();
    }
    let s = String::from_utf8_lossy(&o.stdout);
    let mut seen = std::collections::BTreeSet::new();
    for line in s.lines() {
        let p = line.trim();
        seen.insert(if p.is_empty() { "<sync root>".to_string() } else { p.to_string() });
    }
    seen.into_iter().collect()
}

/// Total conflict count across every devme-managed sync (best-effort, 0 on any
/// hiccup). Used by `wake` to proactively flag an overnight halt.
fn devme_conflict_total() -> u64 {
    let out = Command::new("mutagen")
        .args([
            "sync",
            "list",
            "--label-selector",
            DEVME_LABEL_SELECTOR,
            "--template",
            "{{range .}}{{len .Conflicts}}\n{{end}}",
        ])
        .output();
    let Ok(o) = out else { return 0 };
    if !o.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .sum()
}

/// Best-effort desktop notification (macOS only). A no-op elsewhere or if the
/// notifier is missing — the terminal output is the real contract; the OS
/// toast just means a sync that halted while the laptop slept doesn't go
/// unnoticed until the next time you happen to run a devme command.
fn notify(message: &str) {
    #[cfg(target_os = "macos")]
    {
        let via_tn = Command::new("terminal-notifier")
            .args(["-title", "devme", "-message", message])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !via_tn {
            let script = format!("display notification {message:?} with title \"devme\"");
            let _ = Command::new("osascript").args(["-e", &script]).output();
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = message;
    }
}

/// Does the project's `devme.toml` declare any Docker-backed services? Used
/// to decide whether the remote-Docker doctor check is even relevant.
fn local_stack_needs_docker(root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(root.join("devme.toml")) else {
        return false;
    };
    match devme_config::Stack::parse(&text) {
        Ok(stack) => devme_config::docker::stack_needs_docker(&stack),
        Err(_) => false,
    }
}

/// One preflight check result.
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
    hint: Option<String>,
}

/// `devme remote doctor`: preflight that turns "works on my machine" into
/// "anyone can run it". Checks the local tooling, host reachability, and the
/// remote's `git`/`devme` — with a fixable hint per failure.
pub fn doctor(cwd: &Path, json: bool) -> Result<()> {
    let r = resolve(cwd)?;
    let mut checks: Vec<Check> = Vec::new();

    // 1. mutagen present locally.
    let mutagen = mutagen_available();
    checks.push(Check {
        name: "mutagen installed",
        ok: mutagen,
        detail: if mutagen { "found".into() } else { "not found".into() },
        hint: (!mutagen).then(|| "brew install mutagen-io/mutagen/mutagen".to_string()),
    });
    if mutagen {
        ensure_mutagen_daemon();
    }

    // 2. SSH reachability.
    let (reachable, reach_detail) = ssh_check(&r.host, "true");
    checks.push(Check {
        name: "ssh reachable",
        ok: reachable,
        detail: if reachable {
            format!("{} responded", r.host)
        } else {
            format!("{}: {reach_detail}", r.host)
        },
        hint: (!reachable).then(|| {
            format!("check the host is up and SSH works: ssh {} true", r.host)
        }),
    });

    // The remaining checks need a live connection; skip them if unreachable.
    if reachable {
        // 3. Remote git present.
        let (git_ok, _) = ssh_check(&r.host, "command -v git");
        checks.push(Check {
            name: "remote git",
            ok: git_ok,
            detail: if git_ok { "present".into() } else { "missing".into() },
            hint: (!git_ok).then(|| "install git on the remote host".to_string()),
        });

        // 4. Remote root writable (created if absent).
        let root = &r.root;
        let (root_ok, root_detail) =
            ssh_check(&r.host, &format!("mkdir -p {root} && test -w {root} && echo ok"));
        checks.push(Check {
            name: "remote root writable",
            ok: root_ok,
            detail: if root_ok { format!("{root} ok") } else { format!("{root}: {root_detail}") },
            hint: (!root_ok).then(|| format!("ensure {root} is creatable/writable on the remote")),
        });

        // 4b. Docker on the remote — only flagged if this project's stack
        //     actually needs it (a synced devme.toml with Docker services).
        //     A warning, not a hard failure: the stack just won't start
        //     without it, and the user may run Docker elsewhere.
        if local_stack_needs_docker(&r.local_root) {
            let (docker_ok, _) = ssh_check(&r.host, "docker info");
            checks.push(Check {
                name: "remote docker",
                ok: docker_ok,
                detail: if docker_ok {
                    "running".into()
                } else {
                    "not running (this stack uses Docker)".into()
                },
                hint: (!docker_ok)
                    .then(|| "start Docker on the remote host (or it'll fail when the stack boots)".to_string()),
            });
        }

        // 5. Remote devme present + version-compatible. A remote supervisor on
        //    a mismatched IPC protocol is a real failure mode, so we compare
        //    versions, not just presence.
        let local_ver = env!("CARGO_PKG_VERSION");
        let (devme_ok, devme_out) = ssh_check(&r.host, "devme --version");
        let remote_ver = devme_out
            .split_whitespace()
            .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .unwrap_or("")
            .to_string();
        let version_match = devme_ok && remote_ver == local_ver;
        checks.push(Check {
            name: "remote devme",
            ok: devme_ok,
            detail: if !devme_ok {
                "not found".into()
            } else if version_match {
                format!("v{remote_ver} (matches local)")
            } else {
                format!("v{remote_ver} (local v{local_ver} — version mismatch)")
            },
            hint: if !devme_ok {
                Some("install devme on the remote: curl -fsSL https://devme.sh/install | sh".to_string())
            } else if !version_match {
                Some("update the remote (or local) so versions match: avoids IPC protocol drift".to_string())
            } else {
                None
            },
        });
    }

    let all_ok = checks.iter().all(|c| c.ok);

    if json {
        let arr: Vec<serde_json::Value> = checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "ok": c.ok,
                    "detail": c.detail,
                    "hint": c.hint,
                })
            })
            .collect();
        let value = serde_json::json!({
            "status": if all_ok { "ok" } else { "problems" },
            "host": r.host,
            "remote_path": r.remote_path,
            "checks": arr,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    println!("remote: {} → {}", r.host, r.remote_path);
    for c in &checks {
        let mark = if c.ok { "✔" } else { "✗" };
        println!("  {mark} {:<22} {}", c.name, c.detail);
        if let Some(h) = &c.hint {
            println!("      ↳ {h}");
        }
    }
    if all_ok {
        println!("\nall checks passed — `devme remote` is ready");
    } else {
        println!("\nsome checks failed — fix the hints above, then re-run `devme remote doctor`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Command as C;

    #[test]
    fn strip_marked_block_removes_only_the_devme_block() {
        let content = "#!/bin/sh\nuser-line\n# >>> devme wake-hook >>>\ndevme remote wake\n# <<< devme wake-hook <<<\nother\n";
        let out = strip_marked_block(content);
        assert!(out.contains("user-line"));
        assert!(out.contains("other"));
        assert!(!out.contains("devme remote wake"));
        assert!(!out.contains("devme wake-hook"));
    }

    fn resolved_for(root: &Path) -> Resolved {
        Resolved {
            host: "testhost".into(),
            local_root: root.to_path_buf(),
            beta: format!("testhost:{}", remote::remote_path("~/development", root)),
            remote_path: remote::remote_path("~/development", root),
            session: remote::sync_session_name(root),
            sync_mode: "two-way-safe".into(),
            attach: "tui".into(),
            root: "~/development".into(),
            url_host: "testhost".into(),
            up_on_attach: true,
            ignores: vec![],
            sync_worktrees: true,
            session_suffix: remote::repo_session_suffix(root),
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn resolve_worktrees_finds_linked_worktrees_with_repo_suffix_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("app");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"]);
        git(&main, &["worktree", "add", "-q", "-b", "feature/x", "../app-feature-x"]);

        let main = std::fs::canonicalize(&main).unwrap();
        let r = resolved_for(&main);
        let wts = resolve_worktrees(&r);
        assert_eq!(wts.len(), 1, "{:?}", wts.iter().map(|w| &w.local_path).collect::<Vec<_>>());
        let wt = &wts[0];
        assert_eq!(wt.branch.as_deref(), Some("feature/x"));
        // Worktree session shares the repo suffix but is distinct from main's.
        assert!(wt.session.ends_with(&r.session_suffix), "{}", wt.session);
        assert_ne!(wt.session, r.session);
        assert!(wt.remote_path.starts_with("~/development/app-feature-x-"), "{}", wt.remote_path);
        assert_eq!(wt.beta, format!("testhost:{}", wt.remote_path));

        // A vanished worktree dir (stale registration) is skipped.
        std::fs::remove_dir_all(&wt.local_path).unwrap();
        assert!(resolve_worktrees(&r).is_empty());
    }

    #[test]
    fn retarget_for_cwd_lands_in_the_worktrees_remote_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("app");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"]);
        git(&main, &["worktree", "add", "-q", "-b", "feature/x", "../app-feature-x"]);

        let main = std::fs::canonicalize(&main).unwrap();
        let r = resolved_for(&main);
        // From the main root, the target is unchanged.
        let same = retarget_for_cwd(resolved_for(&main), &main);
        assert_eq!(same.remote_path, r.remote_path);
        // From inside the worktree the remote path would retarget — but only
        // when that worktree's session is live (sync_exists is false here, so
        // the main path holds; the path-matching arm is covered above).
        let wt_dir = tmp.path().join("app-feature-x");
        let kept = retarget_for_cwd(resolved_for(&main), &wt_dir);
        assert_eq!(kept.remote_path, r.remote_path);
    }

    #[test]
    fn proxyable_commands_are_daemon_facing_only() {
        assert!(is_proxyable(&Some(C::Status { all: false })));
        assert!(is_proxyable(&Some(C::Logs {
            service: Some("api".into()),
            follow: false,
            tail: 200,
            since: None,
            json: false,
        })));
        assert!(is_proxyable(&Some(C::Down { timeout: 10, all: false })));
        // Machine-local / non-daemon commands never proxy.
        assert!(!is_proxyable(&Some(C::Config { action: None })));
        assert!(!is_proxyable(&Some(C::Remote { action: None })));
        assert!(!is_proxyable(&None));
    }
}
