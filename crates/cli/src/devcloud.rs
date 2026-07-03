//! `devcloud` — remote project context resolution.
//!
//! This module is intentionally local-only for the first slice: it derives a
//! project identity from Git origin and global config, then formats the values
//! scripts need. SSH execution and remote clone convergence are later issues.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, Subcommand};
use devme_config::DevcloudConfig;

#[derive(Debug, Parser, PartialEq, Eq)]
#[command(
    name = "devcloud",
    version,
    about = "Resolve Git projects into remote dev context"
)]
pub struct DevcloudCli {
    #[command(subcommand)]
    pub command: DevcloudCommand,

    /// Emit machine-readable JSON instead of human-friendly output.
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum DevcloudCommand {
    /// Print the project name as one machine-readable line.
    Name,
    /// Print the canonical remote project path as one machine-readable line.
    Path,
    /// Show the local Git identity, configured host, and remote path.
    Status,
    /// Check that local Git identity can be resolved.
    Doctor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectContext {
    pub local_root: PathBuf,
    pub origin: GitOrigin,
    pub host: String,
    pub remote_path: String,
}

impl ProjectContext {
    pub fn name(&self) -> &str {
        &self.origin.repo
    }

    pub fn identity(&self) -> String {
        self.origin.identity()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOrigin {
    pub raw: String,
    pub provider_host: String,
    pub namespace: Vec<String>,
    pub repo: String,
}

impl GitOrigin {
    pub fn identity(&self) -> String {
        format!("{}/{}", self.namespace.join("/"), self.repo)
    }

    pub fn remote_path(&self, root: &str) -> String {
        let root = root.trim_end_matches('/');
        let mut parts = Vec::with_capacity(self.namespace.len() + 2);
        parts.push(self.provider_host.as_str());
        parts.extend(self.namespace.iter().map(String::as_str));
        parts.push(&self.repo);
        format!("{root}/{}", parts.join("/"))
    }
}

pub fn resolve_project(cwd: &Path, cfg: &DevcloudConfig) -> Result<ProjectContext, String> {
    let local_root = git_output(cwd, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .map_err(|_| {
            "not a Git repository\n  run devcloud from inside a Git checkout".to_string()
        })?;
    let raw_origin = git_output(cwd, &["remote", "get-url", "origin"]).map_err(|_| {
        "no origin remote configured\n  set one with: git remote add origin <url>".to_string()
    })?;
    let origin = parse_git_origin(&raw_origin)?;
    let host = cfg.host_or_default().to_string();
    let remote_path = origin.remote_path(cfg.root_or_default());

    Ok(ProjectContext {
        local_root,
        origin,
        host,
        remote_path,
    })
}

pub fn run_command(
    command: DevcloudCommand,
    cwd: &Path,
    cfg: &DevcloudConfig,
    as_json: bool,
) -> Result<String, String> {
    match command {
        DevcloudCommand::Name => {
            let ctx = resolve_project(cwd, cfg)?;
            if as_json {
                Ok(json_line(serde_json::json!({
                    "schema_version": 1,
                    "name": ctx.name(),
                })))
            } else {
                Ok(format!("{}\n", ctx.name()))
            }
        }
        DevcloudCommand::Path => {
            let ctx = resolve_project(cwd, cfg)?;
            if as_json {
                Ok(json_line(serde_json::json!({
                    "schema_version": 1,
                    "path": ctx.remote_path,
                })))
            } else {
                Ok(format!("{}\n", ctx.remote_path))
            }
        }
        DevcloudCommand::Status => {
            let ctx = resolve_project(cwd, cfg)?;
            if as_json {
                Ok(json_line(status_json(&ctx, None)))
            } else {
                Ok(format_status(&ctx))
            }
        }
        DevcloudCommand::Doctor => {
            let ctx = resolve_project(cwd, cfg)?;
            if as_json {
                Ok(json_line(status_json(&ctx, Some("ok"))))
            } else {
                Ok(format!(
                    "devcloud doctor: ok\nGit: ok ({})\nOrigin: ok ({})\nHost: {}\nRemote path: {}\n",
                    ctx.local_root.display(),
                    ctx.origin.raw,
                    ctx.host,
                    ctx.remote_path
                ))
            }
        }
    }
}

pub fn format_status(ctx: &ProjectContext) -> String {
    format!(
        "Project: {}\nName: {}\nLocal root: {}\nOrigin: {}\nProvider: {}\nHost: {}\nRemote path: {}\n",
        ctx.identity(),
        ctx.name(),
        ctx.local_root.display(),
        ctx.origin.raw,
        ctx.origin.provider_host,
        ctx.host,
        ctx.remote_path
    )
}

fn status_json(ctx: &ProjectContext, status: Option<&str>) -> serde_json::Value {
    let mut value = serde_json::json!({
        "schema_version": 1,
        "name": ctx.name(),
        "project": ctx.identity(),
        "local_root": ctx.local_root.display().to_string(),
        "origin": {
            "raw": &ctx.origin.raw,
            "provider_host": &ctx.origin.provider_host,
            "namespace": &ctx.origin.namespace,
            "repo": &ctx.origin.repo,
        },
        "host": &ctx.host,
        "remote_path": &ctx.remote_path,
    });
    if let Some(status) = status {
        value["status"] = serde_json::Value::String(status.to_string());
    }
    value
}

fn json_line(value: serde_json::Value) -> String {
    format!("{value}\n")
}

pub fn parse_git_origin(raw: &str) -> Result<GitOrigin, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("origin URL is empty".into());
    }

    let (host, path) = if let Some(rest) = trimmed.strip_prefix("git@") {
        parse_scp_like(rest).ok_or_else(|| format!("unsupported origin URL: {trimmed}"))?
    } else if let Some(rest) = trimmed.strip_prefix("ssh://") {
        parse_url_like(rest).ok_or_else(|| format!("unsupported origin URL: {trimmed}"))?
    } else if let Some(rest) = trimmed.strip_prefix("https://") {
        parse_url_like(rest).ok_or_else(|| format!("unsupported origin URL: {trimmed}"))?
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        parse_url_like(rest).ok_or_else(|| format!("unsupported origin URL: {trimmed}"))?
    } else {
        return Err(format!("unsupported origin URL: {trimmed}"));
    };

    origin_from_parts(trimmed, host, path)
}

fn parse_scp_like(rest: &str) -> Option<(&str, &str)> {
    let (host, path) = rest.split_once(':')?;
    non_empty_pair(host, path)
}

fn parse_url_like(rest: &str) -> Option<(&str, &str)> {
    let rest = rest.split_once("://").map(|(_, tail)| tail).unwrap_or(rest);
    let without_user = rest.rsplit_once('@').map(|(_, tail)| tail).unwrap_or(rest);
    let (authority, path) = without_user.split_once('/')?;
    let host = authority
        .split_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority);
    non_empty_pair(host, path)
}

fn non_empty_pair<'a>(host: &'a str, path: &'a str) -> Option<(&'a str, &'a str)> {
    (!host.is_empty() && !path.is_empty()).then_some((host, path))
}

fn origin_from_parts(raw: &str, host: &str, path: &str) -> Result<GitOrigin, String> {
    let provider_host = host.trim().trim_end_matches('/').to_ascii_lowercase();
    let clean_path = path.trim().trim_start_matches('/').trim_end_matches('/');
    let clean_path = clean_path.strip_suffix(".git").unwrap_or(clean_path);
    let segments: Vec<String> = clean_path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if segments.len() < 2 {
        return Err(format!("origin URL must include owner and repo: {raw}"));
    }
    let repo = segments.last().cloned().unwrap();
    let namespace = segments[..segments.len() - 1].to_vec();
    Ok(GitOrigin {
        raw: raw.to_string(),
        provider_host,
        namespace,
        repo,
    })
}

fn git_output(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8(out.stdout).map_err(|e| e.to_string())?;
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("git returned an empty value".into());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parses_github_scp_like_origin() {
        let origin = parse_git_origin("git@github.com:devme-sh/devme.git").unwrap();
        assert_eq!(origin.provider_host, "github.com");
        assert_eq!(origin.identity(), "devme-sh/devme");
        assert_eq!(origin.repo, "devme");
    }

    #[test]
    fn parses_https_origin() {
        let origin = parse_git_origin("https://github.com/devme-sh/devme.git").unwrap();
        assert_eq!(origin.provider_host, "github.com");
        assert_eq!(origin.identity(), "devme-sh/devme");
        assert_eq!(
            origin.remote_path("~/src"),
            "~/src/github.com/devme-sh/devme"
        );
    }

    #[test]
    fn parses_ssh_url_origin() {
        let origin = parse_git_origin("ssh://git@gitlab.com/devme-sh/devme.git").unwrap();
        assert_eq!(origin.provider_host, "gitlab.com");
        assert_eq!(origin.identity(), "devme-sh/devme");
    }

    #[test]
    fn parses_nested_gitlab_groups() {
        let origin = parse_git_origin("git@gitlab.example.com:platform/tools/devme.git").unwrap();
        assert_eq!(origin.provider_host, "gitlab.example.com");
        assert_eq!(origin.identity(), "platform/tools/devme");
        assert_eq!(
            origin.remote_path("~/development/projects/"),
            "~/development/projects/gitlab.example.com/platform/tools/devme"
        );
    }

    #[test]
    fn rejects_invalid_remotes() {
        for raw in [
            "",
            "not-a-url",
            "git@github.com:devme.git",
            "https://github.com",
        ] {
            assert!(parse_git_origin(raw).is_err(), "{raw} should be invalid");
        }
    }

    #[test]
    fn project_context_derives_name_path_and_configured_host() {
        let origin = parse_git_origin("git@github.com:devme-sh/devme.git").unwrap();
        let cfg = DevcloudConfig {
            host: Some("workbox".into()),
            root: Some("~/src".into()),
        };
        let ctx = ProjectContext {
            local_root: PathBuf::from("/tmp/devme"),
            remote_path: origin.remote_path(cfg.root_or_default()),
            host: cfg.host_or_default().to_string(),
            origin,
        };

        assert_eq!(ctx.name(), "devme");
        assert_eq!(ctx.host, "workbox");
        assert_eq!(ctx.remote_path, "~/src/github.com/devme-sh/devme");
    }

    #[test]
    fn resolves_temp_git_repo_with_origin() {
        let dir = git_repo_with_origin("git@github.com:devme-sh/devme.git");
        let cfg = DevcloudConfig {
            host: Some("workbox".into()),
            root: Some("~/src".into()),
        };

        let ctx = resolve_project(dir.path(), &cfg).unwrap();

        assert_eq!(ctx.name(), "devme");
        assert_eq!(ctx.identity(), "devme-sh/devme");
        assert_eq!(ctx.host, "workbox");
        assert_eq!(ctx.remote_path, "~/src/github.com/devme-sh/devme");
        assert_eq!(ctx.local_root, dir.path().canonicalize().unwrap());
    }

    #[test]
    fn name_and_path_commands_are_machine_readable() {
        let dir = git_repo_with_origin("https://gitlab.com/platform/tools/devme.git");
        let cfg = DevcloudConfig {
            host: None,
            root: Some("~/code".into()),
        };

        let name = run_command(DevcloudCommand::Name, dir.path(), &cfg, false).unwrap();
        let path = run_command(DevcloudCommand::Path, dir.path(), &cfg, false).unwrap();

        assert_eq!(name, "devme\n");
        assert_eq!(path, "~/code/gitlab.com/platform/tools/devme\n");
    }

    #[test]
    fn status_contains_identity_origin_host_and_path() {
        let dir = git_repo_with_origin("git@github.com:devme-sh/devme.git");
        let cfg = DevcloudConfig {
            host: Some("vps".into()),
            root: Some("~/projects".into()),
        };

        let status = run_command(DevcloudCommand::Status, dir.path(), &cfg, false).unwrap();

        assert!(status.contains("Project: devme-sh/devme"));
        assert!(status.contains("Origin: git@github.com:devme-sh/devme.git"));
        assert!(status.contains("Host: vps"));
        assert!(status.contains("Remote path: ~/projects/github.com/devme-sh/devme"));
    }

    #[test]
    fn doctor_fails_outside_git_repo() {
        let dir = TempDir::new().unwrap();
        let err = run_command(
            DevcloudCommand::Doctor,
            dir.path(),
            &DevcloudConfig::default(),
            false,
        )
        .unwrap_err();
        assert!(err.contains("not a Git repository"));
    }

    #[test]
    fn doctor_fails_without_origin() {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init"]);

        let err = run_command(
            DevcloudCommand::Doctor,
            dir.path(),
            &DevcloudConfig::default(),
            false,
        )
        .unwrap_err();

        assert!(err.contains("no origin remote configured"));
    }

    #[test]
    fn doctor_succeeds_with_local_identity_only() {
        let dir = git_repo_with_origin("git@github.com:devme-sh/devme.git");

        let doctor = run_command(
            DevcloudCommand::Doctor,
            dir.path(),
            &DevcloudConfig::default(),
            false,
        )
        .unwrap();

        assert!(doctor.contains("devcloud doctor: ok"));
        assert!(doctor.contains("Host: vps"));
        assert!(doctor.contains("Remote path: ~/development/projects/github.com/devme-sh/devme"));
    }

    #[test]
    fn json_flag_parses_globally() {
        let cli = DevcloudCli::parse_from(["devcloud", "--json", "status"]);
        assert!(cli.json);
        assert_eq!(cli.command, DevcloudCommand::Status);
    }

    #[test]
    fn status_json_has_schema_version_and_context() {
        let dir = git_repo_with_origin("git@github.com:devme-sh/devme.git");

        let json = run_command(
            DevcloudCommand::Status,
            dir.path(),
            &DevcloudConfig::default(),
            true,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["name"], "devme");
        assert_eq!(value["project"], "devme-sh/devme");
        assert_eq!(value["host"], "vps");
        assert_eq!(
            value["remote_path"],
            "~/development/projects/github.com/devme-sh/devme"
        );
    }

    fn git_repo_with_origin(origin: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        git(dir.path(), &["init"]);
        git(dir.path(), &["remote", "add", "origin", origin]);
        dir
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
