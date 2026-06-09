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

use anyhow::{Context, Result, anyhow, bail};
use devme_config::{GlobalConfig, paths, remote};

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
        ignores: r.ignores(),
    })
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
    let simple = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "_./:=-".contains(c));
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
    let cmd = remote_devme_cmd(r, &forwarded_args());
    let status = Command::new("ssh").args(["-t", &r.host]).arg(&cmd).status();
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

    let cmd = remote::expand_attach(&r.attach, &r.host, &r.remote_path, &r.session);
    eprintln!("devme remote: attaching ({}) → {}", r.attach, r.host);
    // Hand the terminal to a real shell so all quoting in the attach template
    // is honored and a full-screen remote TUI gets the inherited TTY.
    let status = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .status()
        .context("running attach command")?;
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

/// `devme remote status`: conflict-aware sync state. Silent Mutagen halts on
/// conflict are the #1 failure mode, so the conflict count is surfaced first.
pub fn status(cwd: &Path, json: bool) -> Result<()> {
    let r = resolve(cwd)?;
    require_mutagen()?;
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
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn proxyable_commands_are_daemon_facing_only() {
        assert!(is_proxyable(&Some(C::Status { all: false })));
        assert!(is_proxyable(&Some(C::Logs {
            service: "api".into(),
            follow: false,
            tail: 200
        })));
        assert!(is_proxyable(&Some(C::Down { timeout: 10 })));
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
