//! Resolution of an explicit one-level Devme workspace into one runtime graph.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use devme_core::{Dependency, HealthCheck};
use indexmap::IndexMap;
use thiserror::Error;

use crate::{Provision, Stack, validate};

/// Which config owns the invocation directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Root,
    Member(String),
}

/// Source location for a flattened node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub config: PathBuf,
    pub local_name: String,
}

/// A workspace root and its members composed into one namespaced stack.
#[derive(Debug, Clone)]
pub struct ResolvedWorkspace {
    root: PathBuf,
    focus: Focus,
    stack: Stack,
    origins: HashMap<String, Origin>,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("no devme.toml found from {0} or any parent directory")]
    Missing(PathBuf),
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error(
        "workspace member {member:?} has invalid name; use letters, digits, '-' or '_', and do not use 'root' or '::'"
    )]
    InvalidMemberName { member: String },
    #[error(
        "workspace member {member:?} path {path:?} must be a relative directory within the workspace root"
    )]
    InvalidMemberPath { member: String, path: String },
    #[error("workspace member {member:?} is missing {path}")]
    MissingMemberConfig { member: String, path: PathBuf },
    #[error(
        "workspace member {member:?} declares another [workspace]; nested workspaces are not supported"
    )]
    NestedWorkspace { member: String },
    #[error("workspace members {first:?} and {second:?} use overlapping directories")]
    OverlappingMembers { first: String, second: String },
    #[error("nested config {path} is not an explicitly listed workspace member")]
    UnclaimedConfig { path: PathBuf },
    #[error("{kind} name {name:?} in {path} contains reserved separator '::'")]
    InvalidNodeName {
        kind: &'static str,
        name: String,
        path: PathBuf,
    },
    #[error(
        "child config {path} declares [logs]; shared retention and redaction policy belongs in the workspace root"
    )]
    ChildLogs { path: PathBuf },
    #[error(
        "child config {path} declares [stack].{field}; workspace-wide metadata belongs in the root config"
    )]
    ChildStackField { path: PathBuf, field: &'static str },
    #[error("environment variable {name:?} is declared differently by {first} and {second}")]
    ConflictingEnv {
        name: String,
        first: PathBuf,
        second: PathBuf,
    },
    #[error("resolved workspace is invalid: {0}")]
    Invalid(String),
}

impl ResolvedWorkspace {
    /// Resolve the nearest standalone config or an owning workspace root.
    pub fn resolve(invocation: &Path) -> Result<Self, WorkspaceError> {
        let invocation = absolute(invocation)?;
        let start = if invocation.is_file() {
            invocation.parent().unwrap_or(&invocation)
        } else {
            invocation.as_path()
        };
        let candidates = ancestor_configs(start);
        if candidates.is_empty() {
            return Err(WorkspaceError::Missing(invocation));
        }

        let mut parsed = Vec::new();
        for path in &candidates {
            parsed.push((path.clone(), load(path)?));
        }

        // The outermost explicit workspace owns its listed children. A plain
        // nested config remains a standalone project for compatibility.
        let selected = parsed
            .iter()
            .rev()
            .find(|(_, stack)| stack.workspace.is_some())
            .or_else(|| parsed.first())
            .expect("candidates is non-empty");
        let root_config = &selected.0;
        let root = root_config
            .parent()
            .expect("devme.toml always has a parent")
            .to_path_buf();
        let mut stack = selected.1.clone();
        let mut origins = root_origins(root_config, &stack);
        let mut focus = Focus::Root;

        let Some(workspace) = stack.workspace.clone() else {
            stack.workspace = None;
            validate_resolved(&stack)?;
            return Ok(Self {
                root,
                focus,
                stack,
                origins,
            });
        };

        let members = validate_members(&root, &workspace.members)?;
        let expected_configs = members
            .iter()
            .map(|(_, relative)| root.join(relative).join("devme.toml"))
            .collect::<HashSet<_>>();
        if let Some(path) = candidates.iter().find(|path| {
            path.as_path() != root_config
                && path.starts_with(&root)
                && !expected_configs.contains(path.as_path())
        }) {
            return Err(WorkspaceError::UnclaimedConfig { path: path.clone() });
        }
        validate_node_names(&stack, root_config)?;
        stack.workspace = None;
        let mut env_origins: HashMap<String, PathBuf> = stack
            .env
            .keys()
            .map(|name| (name.clone(), root_config.clone()))
            .collect();

        for (member, relative) in members {
            let member_root = root.join(&relative);
            if start.starts_with(&member_root) {
                focus = Focus::Member(member.clone());
            }
            let config = member_root.join("devme.toml");
            if !config.is_file() {
                return Err(WorkspaceError::MissingMemberConfig {
                    member,
                    path: config,
                });
            }
            let child = load(&config)?;
            if child.workspace.is_some() {
                return Err(WorkspaceError::NestedWorkspace { member });
            }
            validate_node_names(&child, &config)?;
            merge_child(
                &mut stack,
                child,
                &member,
                &relative,
                &config,
                &mut origins,
                &mut env_origins,
            )?;
        }

        validate_resolved(&stack)?;
        Ok(Self {
            root,
            focus,
            stack,
            origins,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn focus(&self) -> &Focus {
        &self.focus
    }

    pub fn stack(&self) -> &Stack {
        &self.stack
    }

    pub fn into_stack(self) -> Stack {
        self.stack
    }

    pub fn origin(&self, name: &str) -> Option<&Origin> {
        self.origins.get(name)
    }

    /// Interpret an unqualified CLI name relative to the invoking member.
    pub fn focus_name(&self, name: &str) -> String {
        if let Some(root_name) = name.strip_prefix("root::") {
            root_name.to_string()
        } else if name.contains("::") {
            name.to_string()
        } else {
            match &self.focus {
                Focus::Root => name.to_string(),
                Focus::Member(member) => format!("{member}::{name}"),
            }
        }
    }

    /// Service targets selected by a member-scoped bare invocation. Root
    /// focus returns `None`, meaning the complete stack.
    pub fn focus_services(&self) -> Option<Vec<String>> {
        let Focus::Member(member) = &self.focus else {
            return None;
        };
        let prefix = format!("{member}::");
        Some(
            self.stack
                .service
                .keys()
                .filter(|name| name.starts_with(&prefix))
                .cloned()
                .collect(),
        )
    }

    /// Sessions declared by the invoking member. Root focus is explicit and
    /// therefore returns no implicit session candidates.
    pub fn focus_sessions(&self) -> Vec<String> {
        let Focus::Member(member) = &self.focus else {
            return Vec::new();
        };
        let prefix = format!("{member}::");
        self.stack
            .session
            .keys()
            .filter(|name| name.starts_with(&prefix))
            .cloned()
            .collect()
    }
}

fn absolute(path: &Path) -> Result<PathBuf, WorkspaceError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|source| WorkspaceError::Read {
                path: path.to_path_buf(),
                source,
            })
    }
}

fn ancestor_configs(start: &Path) -> Vec<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join("devme.toml"))
        .filter(|path| path.is_file())
        .collect()
}

fn load(path: &Path) -> Result<Stack, WorkspaceError> {
    let text = fs::read_to_string(path).map_err(|source| WorkspaceError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    Stack::parse(&text).map_err(|source| WorkspaceError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn root_origins(config: &Path, stack: &Stack) -> HashMap<String, Origin> {
    stack
        .step
        .keys()
        .chain(stack.service.keys())
        .chain(stack.task.keys())
        .chain(stack.resource.keys())
        .chain(stack.session.keys())
        .map(|name| {
            (
                name.clone(),
                Origin {
                    config: config.to_path_buf(),
                    local_name: name.clone(),
                },
            )
        })
        .collect()
}

fn validate_members(
    root: &Path,
    declared: &IndexMap<String, String>,
) -> Result<Vec<(String, PathBuf)>, WorkspaceError> {
    let mut result: Vec<(String, PathBuf)> = Vec::new();
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    for (name, raw) in declared {
        if name == "root"
            || name.contains("::")
            || name.is_empty()
            || !name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return Err(WorkspaceError::InvalidMemberName {
                member: name.clone(),
            });
        }
        let path = Path::new(raw);
        if path.is_absolute()
            || path.as_os_str().is_empty()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(WorkspaceError::InvalidMemberPath {
                member: name.clone(),
                path: raw.clone(),
            });
        }
        let normalized = path.to_path_buf();
        let joined = root.join(&normalized);
        let canonical_joined =
            fs::canonicalize(&joined).unwrap_or_else(|_| canonical_root.join(&normalized));
        if !canonical_joined.starts_with(&canonical_root) {
            return Err(WorkspaceError::InvalidMemberPath {
                member: name.clone(),
                path: raw.clone(),
            });
        }
        for (other, other_path) in &result {
            if normalized.starts_with(other_path) || other_path.starts_with(&normalized) {
                return Err(WorkspaceError::OverlappingMembers {
                    first: other.clone(),
                    second: name.clone(),
                });
            }
        }
        result.push((name.clone(), normalized));
    }
    Ok(result)
}

fn validate_node_names(stack: &Stack, path: &Path) -> Result<(), WorkspaceError> {
    for (kind, names) in [
        ("step", stack.step.keys().collect::<Vec<_>>()),
        ("service", stack.service.keys().collect::<Vec<_>>()),
        ("task", stack.task.keys().collect::<Vec<_>>()),
        ("resource", stack.resource.keys().collect::<Vec<_>>()),
        ("session", stack.session.keys().collect::<Vec<_>>()),
    ] {
        if let Some(name) = names.into_iter().find(|name| name.contains("::")) {
            return Err(WorkspaceError::InvalidNodeName {
                kind,
                name: name.clone(),
                path: path.to_path_buf(),
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn merge_child(
    root: &mut Stack,
    mut child: Stack,
    member: &str,
    relative: &Path,
    config: &Path,
    origins: &mut HashMap<String, Origin>,
    env_origins: &mut HashMap<String, PathBuf>,
) -> Result<(), WorkspaceError> {
    if child.logs.is_some() {
        return Err(WorkspaceError::ChildLogs {
            path: config.to_path_buf(),
        });
    }
    if let Some(meta) = &child.stack {
        for (field, set) in [
            ("default_trust", meta.default_trust.is_some()),
            ("default_restart", meta.default_restart.is_some()),
            ("env_file", meta.env_file.is_some()),
            ("on_create", meta.on_create.is_some()),
            ("on_destroy", meta.on_destroy.is_some()),
        ] {
            if set {
                return Err(WorkspaceError::ChildStackField {
                    path: config.to_path_buf(),
                    field,
                });
            }
        }
    }

    for (name, value) in child.env.drain(..) {
        if let Some(existing) = root.env.get(&name) {
            if existing != &value {
                return Err(WorkspaceError::ConflictingEnv {
                    name: name.clone(),
                    first: env_origins[&name].clone(),
                    second: config.to_path_buf(),
                });
            }
        } else {
            root.env.insert(name.clone(), value);
            env_origins.insert(name, config.to_path_buf());
        }
    }

    let prefix = shell_cd(relative);
    for (local_name, mut step) in child.step.drain(..) {
        rewrite_deps(&mut step.depends_on, member);
        step.check = format!("{prefix}{}", step.check);
        step.provision = step.provision.map(|provision| match provision {
            Provision::Shell(command) => Provision::Shell(format!("{prefix}{command}")),
            Provision::Wizard { wizard } => Provision::Wizard {
                wizard: relative.join(wizard).to_string_lossy().into_owned(),
            },
        });
        insert_node(&mut root.step, origins, member, local_name, step, config);
    }
    for (local_name, mut service) in child.service.drain(..) {
        rewrite_deps(&mut service.depends_on, member);
        service.cwd = Some(rebase_cwd(relative, service.cwd.as_deref()));
        if let Some(path) = &service.log_tail {
            service.log_tail = Some(relative.join(path).to_string_lossy().into_owned());
        }
        if let Some(HealthCheck::Shell { shell }) = &mut service.health {
            *shell = format!("{prefix}{shell}");
        }
        insert_node(
            &mut root.service,
            origins,
            member,
            local_name,
            service,
            config,
        );
    }
    for (local_name, mut task) in child.task.drain(..) {
        rewrite_names(&mut task.depends_on, member);
        rewrite_names(&mut task.steps, member);
        rewrite_names(&mut task.services, member);
        rewrite_names(&mut task.resources, member);
        if task.cmd.is_some() {
            task.cwd = Some(rebase_cwd(relative, task.cwd.as_deref()));
        }
        insert_node(&mut root.task, origins, member, local_name, task, config);
    }
    for (local_name, resource) in child.resource.drain(..) {
        insert_node(
            &mut root.resource,
            origins,
            member,
            local_name,
            resource,
            config,
        );
    }
    for (local_name, mut session) in child.session.drain(..) {
        rewrite_names(&mut session.needs, member);
        rewrite_names(&mut session.resources, member);
        if let Some(task) = &mut session.run {
            *task = qualify(task, member);
        }
        insert_node(
            &mut root.session,
            origins,
            member,
            local_name,
            session,
            config,
        );
    }
    Ok(())
}

fn insert_node<T>(
    map: &mut IndexMap<String, T>,
    origins: &mut HashMap<String, Origin>,
    member: &str,
    local_name: String,
    value: T,
    config: &Path,
) {
    let qualified = format!("{member}::{local_name}");
    map.insert(qualified.clone(), value);
    origins.insert(
        qualified,
        Origin {
            config: config.to_path_buf(),
            local_name,
        },
    );
}

fn rewrite_deps(dependencies: &mut [Dependency], member: &str) {
    for dependency in dependencies {
        dependency.name = qualify(&dependency.name, member);
    }
}

fn rewrite_names(names: &mut [String], member: &str) {
    for name in names {
        *name = qualify(name, member);
    }
}

fn qualify(name: &str, member: &str) -> String {
    if let Some(root_name) = name.strip_prefix("root::") {
        root_name.to_string()
    } else if name.contains("::") {
        name.to_string()
    } else {
        format!("{member}::{name}")
    }
}

fn rebase_cwd(relative: &Path, cwd: Option<&str>) -> String {
    cwd.map_or_else(
        || relative.to_string_lossy().into_owned(),
        |cwd| relative.join(cwd).to_string_lossy().into_owned(),
    )
}

fn shell_cd(relative: &Path) -> String {
    let raw = relative.to_string_lossy();
    let quoted = raw.replace('\'', "'\\''");
    format!("cd '{quoted}' && ")
}

fn validate_resolved(stack: &Stack) -> Result<(), WorkspaceError> {
    validate(stack).map_err(|errors| {
        WorkspaceError::Invalid(
            errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        )
    })
}
