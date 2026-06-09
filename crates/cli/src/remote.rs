//! `devme remote` — bootstrap + live-sync a project to a remote dev host,
//! then attach to its (remote-primary) dev environment. See DEV-5.
//!
//! Split of responsibilities:
//! - **Pure** path/template/config logic lives in [`devme_config::remote`]
//!   and is unit-tested there.
//! - **This module** is the imperative half: it shells out to `mutagen`
//!   (sync sessions — *not* the daemon, which the OS keeps alive) and `ssh`
//!   (reachability + the attach command). devme stays decoupled from herdr:
//!   the attach command is an opaque, user-owned template.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use devme_config::{GlobalConfig, paths, remote};
use devme_config::remote::SyncHealth;

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

/// Create the two-way sync. `.git` is synced (shared-state, model 1a) but the
/// `index.lock` churn is always ignored regardless of the user's ignore list.
fn sync_create(r: &Resolved) -> Result<()> {
    let local = r.local_root.to_string_lossy().to_string();
    let mut args: Vec<String> = vec![
        "sync".into(),
        "create".into(),
        format!("--name={}", r.session),
        format!("--sync-mode={}", r.sync_mode),
        // Label so `devme remote wake` can flush only devme-managed syncs.
        format!("--label={DEVME_LABEL}"),
        // Git bookkeeping that flaps during remote commits — never sync it.
        "--ignore=.git/index.lock".into(),
    ];
    for ig in &r.ignores {
        args.push(format!("--ignore={ig}"));
    }
    args.push(local);
    args.push(r.beta.clone());

    let status = Command::new("mutagen")
        .args(&args)
        .status()
        .context("running `mutagen sync create`")?;
    if !status.success() {
        bail!("`mutagen sync create` failed (see output above)");
    }
    Ok(())
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
fn spawn_sync_watcher(session: String) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        let mut last: Option<SyncHealth> = None;
        let mut polls_in_problem = 0u32;
        let mut reminded = false;
        while !stop_thread.load(Ordering::Relaxed) {
            let (exists, status, conflicts) = observe_sync(&session);
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
    let cmd = format!("cd {} && devme up -d", shell_quote(&r.remote_path));
    eprintln!("devme remote: ensuring stack is up on {} …", r.host);
    let status = Command::new("ssh")
        .args(["-o", "BatchMode=yes", &r.host])
        .arg(&cmd)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!(
            "devme remote: warning: remote `devme up -d` exited {} (attaching anyway)",
            s.code().map(|c| c.to_string()).unwrap_or_else(|| "by signal".into())
        ),
        Err(e) => eprintln!("devme remote: warning: couldn't start remote stack: {e} (attaching anyway)"),
    }
}

// --- transparent proxy ------------------------------------------------------

/// Is a live sync session present for this repo? That's the signal that the
/// project is in **remote mode**: the stack runs on the VPS, so daemon-facing
/// commands forward there. Returns the resolved context when active.
fn remote_active(cwd: &Path) -> Option<Resolved> {
    let r = resolve(cwd).ok()?;
    if sync_exists(&r.session) { Some(r) } else { None }
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

/// POSIX single-quote a shell argument. Simple tokens (service names, flags,
/// numbers) pass through unquoted. The `'\''` escape is also valid in fish,
/// so this is safe whatever login shell the remote uses.
fn shell_quote(s: &str) -> String {
    // `~` is allowed unquoted so a remote path like `~/development/foo` keeps
    // its tilde expansion (single-quoting would make the remote shell `cd`
    // into a literal `~` directory).
    let simple = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "_./:=-~".contains(c));
    if simple {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
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

    // Bring the stack up under the supervisor before attaching, so herdr/ssh
    // attaches land in a project whose dev server is already running (and that
    // keeps running when you detach). `tui`/`tmux` would start it themselves,
    // but `up -d` first is idempotent and makes the herdr/ssh presets work too.
    if r.up_on_attach {
        remote_up(&r);
    }

    let cmd = remote::expand_attach(&r.attach, &r.host, &r.remote_path, &r.session, &r.url_host);
    eprintln!("devme remote: attaching ({}) → {}", r.attach, r.host);

    // Watch the sync in the background for the life of the session: a two-way-
    // safe halt on conflict is silent and laptop-side, so the remote TUI you're
    // attached to can't show it. The watcher notifies (desktop) on a health
    // change; it stays off stderr so it can't corrupt the remote TUI's screen.
    let (stop, watcher) = spawn_sync_watcher(r.session.clone());

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

/// `devme remote sync`: ensure + flush the sync without attaching. Handy from
/// a wake hook or a script.
pub fn sync(cwd: &Path) -> Result<()> {
    let r = resolve(cwd)?;
    require_mutagen()?;
    ensure_mutagen_daemon();
    if !ensure_sync(&r)? {
        sync_flush(&r.session)?;
    }
    eprintln!("devme remote: synced {} ⇄ {}", r.local_root.display(), r.beta);
    Ok(())
}

/// `devme remote stop`: terminate the sync session (the remote files stay;
/// the live link stops).
pub fn stop(cwd: &Path) -> Result<()> {
    let r = resolve(cwd)?;
    require_mutagen()?;
    if !sync_exists(&r.session) {
        eprintln!("devme remote: no live-sync for this project");
        return Ok(());
    }
    let status = Command::new("mutagen")
        .args(["sync", "terminate", &r.session])
        .status()
        .context("running `mutagen sync terminate`")?;
    if !status.success() {
        bail!("`mutagen sync terminate` failed");
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
        let value = serde_json::json!({
            "session": r.session,
            "exists": exists,
            "host": r.host,
            "remote_path": r.remote_path,
            "status": status_str,
            "conflicts": conflicts,
            "raw": raw,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Command as C;

    #[test]
    fn shell_quote_passes_simple_tokens_and_quotes_the_rest() {
        assert_eq!(shell_quote("api"), "api");
        assert_eq!(shell_quote("--tail"), "--tail");
        assert_eq!(shell_quote("200"), "200");
        assert_eq!(shell_quote("svc-1.2/x:y=z"), "svc-1.2/x:y=z");
        // Tilde paths pass through so remote `cd ~/…` still expands.
        assert_eq!(shell_quote("~/development/api-abc"), "~/development/api-abc");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn strip_marked_block_removes_only_the_devme_block() {
        let content = "#!/bin/sh\nuser-line\n# >>> devme wake-hook >>>\ndevme remote wake\n# <<< devme wake-hook <<<\nother\n";
        let out = strip_marked_block(content);
        assert!(out.contains("user-line"));
        assert!(out.contains("other"));
        assert!(!out.contains("devme remote wake"));
        assert!(!out.contains("devme wake-hook"));
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
