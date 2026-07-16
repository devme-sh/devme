//! Versioned project composition for `devme create` and `devme feature`.
//!
//! The composer owns only files recorded in `.devme/composition.lock`. A
//! feature may replace a managed file when its current digest still matches
//! the lock, but it never overwrites an app-owned or modified file.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const MANIFEST: &str = "devme-template.toml";
const LOCK: &str = ".devme/composition.lock";
const BACKUPS: &str = ".devme/composer/backups";
const JOURNAL: &str = ".devme/composer/journal";

#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub template: String,
    pub target: PathBuf,
    pub source: PathBuf,
    pub source_locator: Option<String>,
    pub features: Vec<String>,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct AddFeatureRequest {
    pub root: PathBuf,
    pub feature: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct RemoveFeatureRequest {
    pub root: PathBuf,
    pub feature: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
pub struct UpdateFeatureRequest {
    pub root: PathBuf,
    pub feature: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RecipeIdentity {
    pub name: String,
    pub version: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CompositionReport {
    pub schema_version: u8,
    pub operation: String,
    pub status: String,
    pub dry_run: bool,
    pub recipe: RecipeIdentity,
    pub features: Vec<String>,
    pub changed_files: Vec<PathBuf>,
    pub external_steps: Vec<ExternalStep>,
    pub next_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FeatureStatus {
    pub name: String,
    pub version: String,
    pub installed: bool,
    pub external_steps: Vec<ExternalStep>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ExternalStep {
    pub kind: &'static str,
    pub description: String,
    pub trusted: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FeatureListReport {
    pub schema_version: u8,
    pub template: String,
    pub recipe: RecipeIdentity,
    pub features: Vec<FeatureStatus>,
    pub next_commands: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ComposerError {
    #[error("composition conflict in {paths:?}")]
    Conflict { paths: Vec<PathBuf>, help: String },
    #[error("template or feature not found: {name}")]
    NotFound { name: String, help: String },
    #[error("invalid composition request: {message}")]
    Usage { message: String, help: String },
    #[error("invalid template recipe: {0}")]
    InvalidRecipe(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    TomlDeserialize(#[from] toml::de::Error),
    #[error(transparent)]
    TomlSerialize(#[from] toml::ser::Error),
}

impl ComposerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Conflict { .. } => "conflict",
            Self::NotFound { .. } => "not_found",
            Self::Usage { .. } => "invalid_arguments",
            Self::Io(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                "permission_denied"
            }
            Self::InvalidRecipe(_)
            | Self::Io(_)
            | Self::TomlDeserialize(_)
            | Self::TomlSerialize(_) => "internal",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Conflict { .. } => 5,
            Self::NotFound { .. } => 3,
            Self::Usage { .. } => 2,
            Self::Io(error) if error.kind() == io::ErrorKind::PermissionDenied => 4,
            Self::InvalidRecipe(_)
            | Self::Io(_)
            | Self::TomlDeserialize(_)
            | Self::TomlSerialize(_) => 1,
        }
    }

    pub fn help(&self) -> Option<&str> {
        match self {
            Self::Conflict { help, .. }
            | Self::NotFound { help, .. }
            | Self::Usage { help, .. } => Some(help),
            _ => None,
        }
    }

    pub fn conflict_paths(&self) -> &[PathBuf] {
        match self {
            Self::Conflict { paths, .. } => paths,
            _ => &[],
        }
    }
}

#[derive(Debug, Deserialize)]
struct Recipe {
    schema_version: u8,
    name: String,
    version: String,
    base: Payload,
    #[serde(default)]
    features: BTreeMap<String, FeatureRecipe>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Payload {
    Local {
        path: PathBuf,
    },
    Git {
        repository: String,
        revision: String,
        #[serde(default = "default_subdir")]
        subdir: PathBuf,
    },
}

#[derive(Debug, Deserialize)]
struct FeatureRecipe {
    version: String,
    path: PathBuf,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    conflicts: Vec<String>,
    #[serde(default)]
    external_steps: Vec<String>,
    #[serde(default)]
    remove_external_steps: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CompositionLock {
    schema_version: u8,
    template: String,
    recipe: LockedRecipe,
    managed_files: BTreeMap<PathBuf, String>,
    #[serde(default)]
    features: BTreeMap<String, LockedFeature>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LockedRecipe {
    name: String,
    version: String,
    digest: String,
    source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockedFeature {
    version: String,
    files: BTreeMap<PathBuf, LockedFeatureFile>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    external_steps: Vec<String>,
    #[serde(default)]
    remove_external_steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LockedFeatureFile {
    installed_digest: String,
    previous_digest: Option<String>,
    previous_existed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OperationJournal {
    schema_version: u8,
    operation: String,
    feature: Option<String>,
    backup_features: Vec<String>,
    entries: Vec<JournalEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEntry {
    path: PathBuf,
    before_digest: Option<String>,
    after_digest: Option<String>,
}

struct LoadedRecipe {
    recipe: Recipe,
    identity: RecipeIdentity,
    source: PathBuf,
    locator: String,
    _checkout: Option<tempfile::TempDir>,
}

#[derive(Debug, Clone)]
struct PayloadFile {
    contents: Vec<u8>,
    executable: bool,
}

#[derive(Debug, Default)]
pub struct Composer;

impl Composer {
    pub fn new() -> Self {
        Self
    }

    pub fn create(&self, request: CreateRequest) -> Result<CompositionReport, ComposerError> {
        let locator = request
            .source_locator
            .unwrap_or_else(|| request.source.display().to_string());
        let loaded = if request.source.exists() {
            load_recipe_path(&request.source, &locator, None)?
        } else {
            load_recipe(&locator)?
        };
        if loaded.recipe.name != request.template {
            return Err(ComposerError::NotFound {
                name: request.template,
                help: format!("Available template: {}", loaded.recipe.name),
            });
        }
        validate_new_target(&request.target)?;
        let mut base_files = collect_base_files(&loaded.source, &loaded.recipe.base)?;
        validate_payload_paths(&base_files, true)?;
        validate_base_payload_paths(&base_files)?;
        track_composition_state(&mut base_files);
        let mut planned = base_files.clone();
        let mut external_steps = Vec::new();
        let mut feature_order = Vec::new();
        let mut feature_owners = base_files
            .keys()
            .map(|path| (collision_key(path), (path.clone(), "base".to_string())))
            .collect::<BTreeMap<_, _>>();

        for feature in &request.features {
            let recipe = feature_recipe(&loaded.recipe, feature)?;
            validate_feature_dependencies(&feature_order, &loaded.recipe, feature, recipe)?;
            let files = collect_files(&payload_root(&loaded.source, &recipe.path)?)?;
            validate_payload_paths(&files, false)?;
            let overlaps = files
                .keys()
                .filter(|path| {
                    feature_owners
                        .get(&collision_key(path))
                        .is_some_and(|(existing, owner)| {
                            existing != *path
                                || !feature_can_overlay_owned_path(
                                    &loaded.recipe,
                                    feature,
                                    std::iter::once(owner.as_str()),
                                )
                        })
                })
                .cloned()
                .collect::<Vec<_>>();
            if !overlaps.is_empty() {
                return Err(ComposerError::Conflict {
                    paths: overlaps,
                    help: "Feature payloads must own separate files. Move shared configuration into the base template or generate it through one owning feature.".into(),
                });
            }
            for (path, file) in files {
                feature_owners.insert(collision_key(&path), (path.clone(), feature.clone()));
                planned.insert(path, file);
            }
            external_steps.extend(manual_steps(&recipe.external_steps));
            feature_order.push(feature.clone());
        }

        let changed_files = planned.keys().cloned().collect::<Vec<_>>();
        if request.dry_run {
            return Ok(report(
                "create",
                true,
                loaded.identity,
                feature_order,
                changed_files,
                external_steps,
            ));
        }

        fs::create_dir_all(&request.target)?;
        write_files(&request.target, &base_files)?;
        let mut lock = CompositionLock {
            schema_version: 1,
            template: loaded.recipe.name.clone(),
            recipe: LockedRecipe {
                name: loaded.identity.name.clone(),
                version: loaded.identity.version.clone(),
                digest: loaded.identity.digest.clone(),
                source: loaded.locator.clone(),
            },
            managed_files: digests(&base_files),
            features: BTreeMap::new(),
        };
        write_lock(&request.target, &lock)?;

        let mut all_changes = base_files.keys().cloned().collect::<BTreeSet<_>>();
        for feature in request.features {
            let added =
                self.add_loaded_feature(&request.target, &loaded, &mut lock, &feature, false)?;
            all_changes.extend(added.changed_files);
        }

        Ok(report(
            "create",
            false,
            loaded.identity,
            lock.features.keys().cloned().collect(),
            all_changes.into_iter().collect(),
            external_steps,
        ))
    }

    pub fn add_feature(
        &self,
        request: AddFeatureRequest,
    ) -> Result<CompositionReport, ComposerError> {
        let mut lock = read_lock(&request.root)?;
        let loaded = load_recipe(&lock.recipe.source)?;
        validate_recipe_transition(&lock, &loaded)?;
        self.add_loaded_feature(
            &request.root,
            &loaded,
            &mut lock,
            &request.feature,
            request.dry_run,
        )
    }

    pub fn remove_feature(
        &self,
        request: RemoveFeatureRequest,
    ) -> Result<CompositionReport, ComposerError> {
        let mut lock = read_lock(&request.root)?;
        let identity = lock_identity(&lock);
        let Some(feature) = lock.features.get(&request.feature).cloned() else {
            return Err(ComposerError::NotFound {
                name: request.feature,
                help: "Run `devme feature list` to inspect installed features.".into(),
            });
        };
        let dependents = lock
            .features
            .iter()
            .filter(|(name, installed)| {
                *name != &request.feature
                    && installed
                        .dependencies
                        .iter()
                        .any(|dependency| dependency == &request.feature)
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        if !dependents.is_empty() {
            return Err(ComposerError::Usage {
                message: format!(
                    "feature {:?} is required by {}",
                    request.feature,
                    dependents.join(", ")
                ),
                help: format!(
                    "Remove dependent features first: {}",
                    dependents
                        .iter()
                        .map(|name| format!("devme feature remove {name}"))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            });
        }
        let mut conflicts = Vec::new();
        for (path, file) in &feature.files {
            ensure_target_path_safe(&request.root, path)?;
            if file_digest_optional(&request.root.join(path))?.as_deref()
                != Some(file.installed_digest.as_str())
            {
                conflicts.push(path.clone());
            }
        }
        if !conflicts.is_empty() {
            return Err(ComposerError::Conflict {
                paths: conflicts,
                help: format!(
                    "Review with `devme feature remove {} --dry-run`, then restore or move the modified files before retrying.",
                    request.feature
                ),
            });
        }
        let changed_files = feature.files.keys().cloned().collect::<Vec<_>>();
        if !request.dry_run {
            let desired = feature
                .files
                .iter()
                .map(|(path, file)| {
                    (
                        path.clone(),
                        if file.previous_existed {
                            file.previous_digest.clone()
                        } else {
                            None
                        },
                    )
                })
                .collect();
            begin_journal(
                &request.root,
                "remove",
                Some(request.feature.clone()),
                desired,
                vec![request.feature.clone()],
            )?;
            for (path, file) in &feature.files {
                let target = request.root.join(path);
                if file.previous_existed {
                    let backup = backup_path(&request.root, &request.feature, path)?;
                    let backup_file = read_payload_file(&backup)?;
                    write_file(&target, &backup_file)?;
                    lock.managed_files.insert(
                        path.clone(),
                        file.previous_digest.clone().ok_or_else(|| {
                            ComposerError::InvalidRecipe(format!(
                                "missing previous digest for {}",
                                path.display()
                            ))
                        })?,
                    );
                } else {
                    remove_file_and_empty_parents(&request.root, &target)?;
                    lock.managed_files.remove(path);
                }
            }
            lock.features.remove(&request.feature);
            ensure_target_path_safe(&request.root, &Path::new(BACKUPS).join(&request.feature))?;
            let backup_root = request.root.join(BACKUPS).join(&request.feature);
            if backup_root.exists() {
                fs::remove_dir_all(backup_root)?;
            }
            write_lock(&request.root, &lock)?;
            clear_journal(&request.root)?;
        }
        let mut external_steps = manual_steps(&feature.remove_external_steps);
        external_steps.extend(manual_steps(&[
            "Source removal does not delete provider data, cancel subscriptions, revoke credentials, or remove store resources."
                .into(),
        ]));
        Ok(report(
            "feature_remove",
            request.dry_run,
            identity,
            lock.features
                .keys()
                .filter(|name| *name != &request.feature)
                .cloned()
                .collect(),
            changed_files,
            external_steps,
        ))
    }

    pub fn list_features(&self, root: &Path) -> Result<FeatureListReport, ComposerError> {
        let lock = read_lock(root)?;
        let loaded = load_recipe(&lock.recipe.source)?;
        validate_recipe_transition(&lock, &loaded)?;
        let names = loaded
            .recipe
            .features
            .keys()
            .chain(lock.features.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let features = names
            .iter()
            .map(|name| {
                if let Some(installed) = lock.features.get(name) {
                    FeatureStatus {
                        name: name.clone(),
                        version: installed.version.clone(),
                        installed: true,
                        external_steps: manual_steps(&installed.external_steps),
                    }
                } else {
                    let available = &loaded.recipe.features[name];
                    FeatureStatus {
                        name: name.clone(),
                        version: available.version.clone(),
                        installed: false,
                        external_steps: manual_steps(&available.external_steps),
                    }
                }
            })
            .collect::<Vec<_>>();
        let next_commands = features
            .iter()
            .filter(|feature| !feature.installed)
            .map(|feature| format!("devme feature add {} --dry-run --output toon", feature.name))
            .collect();
        Ok(FeatureListReport {
            schema_version: 1,
            template: lock.template.clone(),
            recipe: lock_identity(&lock),
            features,
            next_commands,
        })
    }

    pub fn recipe_locator(&self, root: &Path) -> Result<String, ComposerError> {
        Ok(read_lock(root)?.recipe.source)
    }

    pub fn update_feature(
        &self,
        request: UpdateFeatureRequest,
    ) -> Result<CompositionReport, ComposerError> {
        let mut lock = read_lock(&request.root)?;
        let loaded = load_recipe(&lock.recipe.source)?;
        validate_recipe_transition(&lock, &loaded)?;
        let feature = request.feature;
        if !lock.features.contains_key(&feature) {
            return Err(ComposerError::NotFound {
                name: feature,
                help: "Run `devme feature list` to inspect installed features.".into(),
            });
        }
        let selected = vec![feature];
        let mut changed_files = BTreeSet::new();
        let mut external_steps = Vec::new();

        for feature_name in &selected {
            let old = lock.features[feature_name].clone();
            let recipe = feature_recipe(&loaded.recipe, feature_name)?;
            validate_feature_dependencies(
                &lock
                    .features
                    .keys()
                    .filter(|name| *name != feature_name)
                    .cloned()
                    .collect::<Vec<_>>(),
                &loaded.recipe,
                feature_name,
                recipe,
            )?;
            let next_files = collect_files(&payload_root(&loaded.source, &recipe.path)?)?;
            validate_payload_paths(&next_files, false)?;
            let paths = old
                .files
                .keys()
                .chain(next_files.keys())
                .cloned()
                .collect::<BTreeSet<_>>();
            let mut conflicts = Vec::new();
            for path in &paths {
                ensure_target_path_safe(&request.root, path)?;
                let current = file_digest_optional(&request.root.join(path))?;
                if let Some(old_file) = old.files.get(path) {
                    if current.as_deref() != Some(old_file.installed_digest.as_str()) {
                        conflicts.push(path.clone());
                    }
                } else if current.is_some() && current.as_ref() != lock.managed_files.get(path) {
                    conflicts.push(path.clone());
                }
                if lock.features.iter().any(|(other_name, other)| {
                    other_name != feature_name && other.files.contains_key(path)
                }) {
                    conflicts.push(path.clone());
                }
                if has_case_collision(&lock, path) {
                    conflicts.push(path.clone());
                }
                if has_target_case_collision(&request.root, path)? {
                    conflicts.push(path.clone());
                }
            }
            conflicts.sort();
            conflicts.dedup();
            if !conflicts.is_empty() {
                return Err(ComposerError::Conflict {
                    paths: conflicts,
                    help: format!(
                        "Review with `devme feature update {feature_name} --dry-run`, then restore or move the modified files before retrying."
                    ),
                });
            }
            changed_files.extend(paths);
            external_steps.extend(manual_steps(&recipe.external_steps));
            if request.dry_run {
                continue;
            }
            let desired = old
                .files
                .keys()
                .chain(next_files.keys())
                .map(|path| {
                    let digest = next_files.get(path).map(file_digest).or_else(|| {
                        old.files.get(path).and_then(|old_file| {
                            if old_file.previous_existed {
                                old_file.previous_digest.clone()
                            } else {
                                None
                            }
                        })
                    });
                    (path.clone(), digest)
                })
                .collect();
            begin_journal(
                &request.root,
                "update",
                Some(feature_name.clone()),
                desired,
                vec![feature_name.clone()],
            )?;

            let mut updated_files = BTreeMap::new();
            for (path, old_file) in &old.files {
                let Some(next) = next_files.get(path) else {
                    restore_previous(&request.root, feature_name, path, old_file, &mut lock)?;
                    continue;
                };
                write_file(&request.root.join(path), next)?;
                lock.managed_files.insert(path.clone(), file_digest(next));
                let mut updated = old_file.clone();
                updated.installed_digest = file_digest(next);
                updated_files.insert(path.clone(), updated);
            }
            for (path, next) in &next_files {
                if old.files.contains_key(path) {
                    continue;
                }
                let target = request.root.join(path);
                let previous_digest = file_digest_optional(&target)?;
                let previous_existed = previous_digest.is_some();
                if previous_existed {
                    write_file(
                        &backup_path(&request.root, feature_name, path)?,
                        &read_payload_file(&target)?,
                    )?;
                }
                write_file(&target, next)?;
                let installed_digest = file_digest(next);
                lock.managed_files
                    .insert(path.clone(), installed_digest.clone());
                updated_files.insert(
                    path.clone(),
                    LockedFeatureFile {
                        installed_digest,
                        previous_digest,
                        previous_existed,
                    },
                );
            }
            lock.features.insert(
                feature_name.clone(),
                LockedFeature {
                    version: recipe.version.clone(),
                    files: updated_files,
                    dependencies: recipe.dependencies.clone(),
                    external_steps: recipe.external_steps.clone(),
                    remove_external_steps: recipe.remove_external_steps.clone(),
                },
            );
        }

        if !request.dry_run {
            lock.recipe.version = loaded.identity.version.clone();
            lock.recipe.digest = loaded.identity.digest.clone();
            write_lock(&request.root, &lock)?;
            clear_journal(&request.root)?;
        }
        Ok(report(
            "feature_update",
            request.dry_run,
            loaded.identity,
            lock.features.keys().cloned().collect(),
            changed_files.into_iter().collect(),
            external_steps,
        ))
    }

    pub fn abort_operation(&self, root: &Path) -> Result<CompositionReport, ComposerError> {
        let journal = abort_journal(root)?;
        let lock = read_lock(root)?;
        Ok(report(
            "feature_abort",
            false,
            lock_identity(&lock),
            lock.features.keys().cloned().collect(),
            journal
                .entries
                .into_iter()
                .map(|entry| entry.path)
                .collect(),
            manual_steps(&[
                "Source rollback does not change provider accounts, remote data, or subscriptions."
                    .into(),
            ]),
        ))
    }

    pub fn continue_operation(&self, root: &Path) -> Result<CompositionReport, ComposerError> {
        let journal = read_journal(root)?;
        let feature = journal.feature.clone().ok_or_else(|| {
            ComposerError::InvalidRecipe("pending operation does not name a feature".into())
        })?;
        abort_journal(root)?;
        match journal.operation.as_str() {
            "add" => self.add_feature(AddFeatureRequest {
                root: root.to_path_buf(),
                feature,
                dry_run: false,
            }),
            "remove" => self.remove_feature(RemoveFeatureRequest {
                root: root.to_path_buf(),
                feature,
                dry_run: false,
            }),
            "update" => self.update_feature(UpdateFeatureRequest {
                root: root.to_path_buf(),
                feature,
                dry_run: false,
            }),
            operation => Err(ComposerError::InvalidRecipe(format!(
                "unsupported pending operation {operation:?}"
            ))),
        }
    }

    fn add_loaded_feature(
        &self,
        root: &Path,
        loaded: &LoadedRecipe,
        lock: &mut CompositionLock,
        feature_name: &str,
        dry_run: bool,
    ) -> Result<CompositionReport, ComposerError> {
        if lock.features.contains_key(feature_name) {
            return Err(ComposerError::Usage {
                message: format!("feature {feature_name:?} is already installed"),
                help: format!("Use `devme feature update {feature_name}` to refresh it."),
            });
        }
        let feature = feature_recipe(&loaded.recipe, feature_name)?;
        validate_feature_dependencies(
            &lock.features.keys().cloned().collect::<Vec<_>>(),
            &loaded.recipe,
            feature_name,
            feature,
        )?;
        let files = collect_files(&payload_root(&loaded.source, &feature.path)?)?;
        validate_payload_paths(&files, false)?;
        let mut conflicts = Vec::new();
        for path in files.keys() {
            ensure_target_path_safe(root, path)?;
            let current = file_digest_optional(&root.join(path))?;
            let expected = lock.managed_files.get(path);
            if current.is_some() && current.as_ref() != expected {
                conflicts.push(path.clone());
            }
            let owners = lock
                .features
                .iter()
                .filter_map(|(name, installed)| installed.files.contains_key(path).then_some(name));
            if !feature_can_overlay_owned_path(
                &loaded.recipe,
                feature_name,
                owners.map(String::as_str),
            ) {
                conflicts.push(path.clone());
            }
            if has_case_collision(lock, path) {
                conflicts.push(path.clone());
            }
            if has_target_case_collision(root, path)? {
                conflicts.push(path.clone());
            }
        }
        conflicts.sort();
        conflicts.dedup();
        if !conflicts.is_empty() {
            return Err(ComposerError::Conflict {
                paths: conflicts,
                help: format!(
                    "Review with `devme feature add {feature_name} --dry-run`, then restore or move the modified files before retrying."
                ),
            });
        }

        let changed_files = files.keys().cloned().collect::<Vec<_>>();
        if !dry_run {
            begin_journal(
                root,
                "add",
                Some(feature_name.to_string()),
                files
                    .iter()
                    .map(|(path, file)| (path.clone(), Some(file_digest(file))))
                    .collect(),
                vec![feature_name.to_string()],
            )?;
            let mut locked_files = BTreeMap::new();
            for (path, file) in &files {
                let target = root.join(path);
                let previous_digest = file_digest_optional(&target)?;
                let previous_existed = previous_digest.is_some();
                if previous_existed {
                    let backup = backup_path(root, feature_name, path)?;
                    write_file(&backup, &read_payload_file(&target)?)?;
                }
                let installed_digest = file_digest(file);
                write_file(&target, file)?;
                lock.managed_files
                    .insert(path.clone(), installed_digest.clone());
                locked_files.insert(
                    path.clone(),
                    LockedFeatureFile {
                        installed_digest,
                        previous_digest,
                        previous_existed,
                    },
                );
            }
            lock.features.insert(
                feature_name.to_string(),
                LockedFeature {
                    version: feature.version.clone(),
                    files: locked_files,
                    dependencies: feature.dependencies.clone(),
                    external_steps: feature.external_steps.clone(),
                    remove_external_steps: feature.remove_external_steps.clone(),
                },
            );
            lock.recipe.version = loaded.identity.version.clone();
            lock.recipe.digest = loaded.identity.digest.clone();
            write_lock(root, lock)?;
            clear_journal(root)?;
        }

        let mut installed = lock.features.keys().cloned().collect::<Vec<_>>();
        if dry_run {
            installed.push(feature_name.to_string());
            installed.sort();
        }
        Ok(report(
            "feature_add",
            dry_run,
            loaded.identity.clone(),
            installed,
            changed_files,
            manual_steps(&feature.external_steps),
        ))
    }
}

fn load_recipe(locator: &str) -> Result<LoadedRecipe, ComposerError> {
    let local = Path::new(locator);
    let (source, checkout) = if local.exists() {
        (local.canonicalize()?, None)
    } else {
        let checkout = tempfile::tempdir()?;
        let output = Command::new("git")
            .args(["clone", "--quiet", "--depth", "1", locator])
            .arg(checkout.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ADVICE", "0")
            .output()?;
        if !output.status.success() {
            return Err(ComposerError::NotFound {
                name: locator.into(),
                help: format!(
                    "Could not resolve the recipe source: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        (checkout.path().to_path_buf(), Some(checkout))
    };
    load_recipe_path(&source, locator, checkout)
}

fn load_recipe_path(
    source: &Path,
    locator: &str,
    checkout: Option<tempfile::TempDir>,
) -> Result<LoadedRecipe, ComposerError> {
    let source = source.canonicalize()?;
    let bytes = fs::read(source.join(MANIFEST))?;
    let recipe: Recipe = toml::from_str(std::str::from_utf8(&bytes).map_err(|error| {
        ComposerError::InvalidRecipe(format!("{MANIFEST} is not UTF-8: {error}"))
    })?)?;
    if recipe.schema_version != 1 {
        return Err(ComposerError::InvalidRecipe(format!(
            "unsupported schema_version {}",
            recipe.schema_version
        )));
    }
    validate_git_revisions(&recipe)?;
    validate_recipe_identifiers(&recipe)?;
    let identity = RecipeIdentity {
        name: recipe.name.clone(),
        version: recipe.version.clone(),
        digest: recipe_digest(&source, &bytes, &recipe)?,
    };
    Ok(LoadedRecipe {
        recipe,
        identity,
        source,
        locator: locator.into(),
        _checkout: checkout,
    })
}

fn validate_recipe_transition(
    lock: &CompositionLock,
    loaded: &LoadedRecipe,
) -> Result<(), ComposerError> {
    if loaded.identity.name != lock.recipe.name || lock.recipe.name != lock.template {
        return Err(ComposerError::InvalidRecipe(format!(
            "recipe identity changed from {:?} to {:?}",
            lock.recipe.name, loaded.identity.name
        )));
    }
    if lock.recipe.version == loaded.identity.version
        && lock.recipe.digest != loaded.identity.digest
    {
        return Err(ComposerError::InvalidRecipe(format!(
            "recipe version {:?} changed without a version change; publish a new recipe version before changing its manifest or payloads",
            loaded.identity.version
        )));
    }
    Ok(())
}

fn validate_git_revisions(recipe: &Recipe) -> Result<(), ComposerError> {
    let Payload::Git { revision, .. } = &recipe.base else {
        return Ok(());
    };
    if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ComposerError::InvalidRecipe(format!(
            "Git base revision must be a full commit ID, got {revision:?}"
        )));
    }
    Ok(())
}

fn validate_recipe_identifiers(recipe: &Recipe) -> Result<(), ComposerError> {
    let mut names = BTreeMap::<String, &str>::new();
    for name in recipe.features.keys() {
        let path = Path::new(name);
        if name.is_empty()
            || path.components().count() != 1
            || !matches!(path.components().next(), Some(Component::Normal(_)))
        {
            return Err(ComposerError::InvalidRecipe(format!(
                "feature name must be one portable path component: {name:?}"
            )));
        }
        let normalized = name.to_lowercase();
        if let Some(existing) = names.insert(normalized, name) {
            return Err(ComposerError::InvalidRecipe(format!(
                "feature names collide on case-insensitive filesystems: {existing:?} and {name:?}"
            )));
        }
    }
    Ok(())
}

fn recipe_digest(source: &Path, manifest: &[u8], recipe: &Recipe) -> Result<String, ComposerError> {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"manifest", manifest);
    if let Payload::Local { path } = &recipe.base {
        let files = collect_files(&payload_root(source, path)?)?;
        hash_recipe_files(&mut hasher, "base", &files);
    }
    for (name, feature) in &recipe.features {
        let files = collect_files(&payload_root(source, &feature.path)?)?;
        hash_recipe_files(&mut hasher, &format!("feature:{name}"), &files);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn hash_recipe_files(hasher: &mut Sha256, owner: &str, files: &BTreeMap<PathBuf, PayloadFile>) {
    for (path, file) in files {
        hash_field(hasher, b"owner", owner.as_bytes());
        hash_field(hasher, b"path", path.as_os_str().as_encoded_bytes());
        hash_field(hasher, b"mode", &[u8::from(file.executable)]);
        hash_field(hasher, b"contents", &file.contents);
    }
}

fn hash_field(hasher: &mut Sha256, label: &[u8], value: &[u8]) {
    hasher.update((label.len() as u64).to_le_bytes());
    hasher.update(label);
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn feature_recipe<'a>(
    recipe: &'a Recipe,
    feature: &str,
) -> Result<&'a FeatureRecipe, ComposerError> {
    recipe
        .features
        .get(feature)
        .ok_or_else(|| ComposerError::NotFound {
            name: feature.to_string(),
            help: if recipe.features.is_empty() {
                "This recipe does not publish any optional features.".into()
            } else {
                format!(
                    "Available features: {}",
                    recipe
                        .features
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            },
        })
}

fn validate_feature_dependencies(
    installed: &[String],
    recipe: &Recipe,
    feature_name: &str,
    feature: &FeatureRecipe,
) -> Result<(), ComposerError> {
    let installed = installed
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let missing = feature
        .dependencies
        .iter()
        .filter(|dependency| !installed.contains(dependency.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(ComposerError::Usage {
            message: format!("feature {feature_name:?} requires {}", missing.join(", ")),
            help: format!(
                "Add dependencies first: {}",
                missing
                    .iter()
                    .map(|name| format!("devme feature add {name}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        });
    }
    let conflicts = feature
        .conflicts
        .iter()
        .filter(|conflict| installed.contains(conflict.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if !conflicts.is_empty() {
        return Err(ComposerError::Conflict {
            paths: Vec::new(),
            help: format!(
                "Remove conflicting features before adding {feature_name}: {}",
                conflicts.join(", ")
            ),
        });
    }
    for dependency in &feature.dependencies {
        if !recipe.features.contains_key(dependency) {
            return Err(ComposerError::InvalidRecipe(format!(
                "feature {feature_name:?} references unknown dependency {dependency:?}"
            )));
        }
    }
    Ok(())
}

fn feature_depends_on(recipe: &Recipe, feature: &str, dependency: &str) -> bool {
    let mut pending = vec![feature];
    let mut visited = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if !visited.insert(name) {
            continue;
        }
        let Some(candidate) = recipe.features.get(name) else {
            continue;
        };
        for parent in &candidate.dependencies {
            if parent == dependency {
                return true;
            }
            pending.push(parent);
        }
    }
    false
}

fn feature_can_overlay_owned_path<'a>(
    recipe: &Recipe,
    feature: &str,
    owners: impl IntoIterator<Item = &'a str>,
) -> bool {
    owners
        .into_iter()
        .all(|owner| owner == "base" || feature_depends_on(recipe, feature, owner))
}

fn validate_new_target(target: &Path) -> Result<(), ComposerError> {
    if !target.exists() {
        return Ok(());
    }
    if fs::symlink_metadata(target)?.file_type().is_symlink()
        || !target.is_dir()
        || fs::read_dir(target)?.next().is_some()
    {
        return Err(ComposerError::Conflict {
            paths: vec![target.to_path_buf()],
            help: "Choose an empty destination or move the existing files before retrying.".into(),
        });
    }
    Ok(())
}

fn ensure_target_path_safe(root: &Path, relative: &Path) -> Result<(), ComposerError> {
    validate_relative(relative)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ComposerError::Conflict {
                    paths: vec![relative.to_path_buf()],
                    help: "Replace the target-side symlink with a real path inside the project before retrying.".into(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn has_target_case_collision(root: &Path, relative: &Path) -> Result<bool, ComposerError> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let expected = component.as_os_str();
        if !current.is_dir() {
            return Ok(false);
        }
        for entry in fs::read_dir(&current)? {
            let actual = entry?.file_name();
            if actual != expected
                && actual
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&expected.to_string_lossy())
            {
                return Ok(true);
            }
        }
        current.push(expected);
    }
    Ok(false)
}

fn payload_root(source: &Path, relative: &Path) -> Result<PathBuf, ComposerError> {
    validate_relative(relative)?;
    let mut root = source.to_path_buf();
    for component in relative.components() {
        root.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&root).map_err(|error| {
            ComposerError::InvalidRecipe(format!(
                "payload directory is unavailable: {}: {error}",
                relative.display()
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ComposerError::InvalidRecipe(format!(
                "payload directory must use real directories inside the recipe: {}",
                relative.display()
            )));
        }
    }
    Ok(root)
}

fn validate_payload_paths(
    files: &BTreeMap<PathBuf, PayloadFile>,
    allow_gitignore: bool,
) -> Result<(), ComposerError> {
    let mut normalized = BTreeMap::<String, &Path>::new();
    for path in files.keys() {
        let first = path
            .components()
            .next()
            .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let normalized_path = collision_key(path);
        let invalid_gitignore = normalized_path == ".gitignore"
            && (!allow_gitignore || path != Path::new(".gitignore"));
        if matches!(first.as_str(), ".devme" | ".git") || invalid_gitignore {
            return Err(ComposerError::InvalidRecipe(format!(
                "payload path is reserved for project composition: {}",
                path.display()
            )));
        }
        let key = collision_key(path);
        if let Some(existing) = normalized.insert(key, path) {
            return Err(ComposerError::InvalidRecipe(format!(
                "payload paths collide on case-insensitive filesystems: {} and {}",
                existing.display(),
                path.display()
            )));
        }
    }
    Ok(())
}

fn validate_base_payload_paths(
    files: &BTreeMap<PathBuf, PayloadFile>,
) -> Result<(), ComposerError> {
    if let Some(path) = files.keys().find(|path| collision_key(path) == MANIFEST) {
        return Err(ComposerError::InvalidRecipe(format!(
            "base snapshot contains recipe authority file {}; pin a clean generated-project snapshot instead of the recipe authoring root",
            path.display()
        )));
    }
    Ok(())
}

fn collision_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

fn has_case_collision(lock: &CompositionLock, path: &Path) -> bool {
    let key = collision_key(path);
    lock.managed_files
        .keys()
        .any(|existing| existing != path && collision_key(existing) == key)
}

fn collect_base_files(
    recipe_source: &Path,
    payload: &Payload,
) -> Result<BTreeMap<PathBuf, PayloadFile>, ComposerError> {
    match payload {
        Payload::Local { path } => collect_files(&payload_root(recipe_source, path)?),
        Payload::Git {
            repository,
            revision,
            subdir,
        } => {
            let checkout = tempfile::tempdir()?;
            let output = Command::new("git")
                .args(["init", "--quiet"])
                .arg(checkout.path())
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_ADVICE", "0")
                .output()?;
            if !output.status.success() {
                return Err(ComposerError::InvalidRecipe(format!(
                    "could not initialize checkout for base revision {revision:?}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
            for args in [
                vec!["remote", "add", "origin", repository],
                vec!["fetch", "--quiet", "--depth", "1", "origin", revision],
                vec!["checkout", "--quiet", "--detach", "FETCH_HEAD"],
            ] {
                let output = Command::new("git")
                    .arg("-C")
                    .arg(checkout.path())
                    .args(&args)
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .env("GIT_ADVICE", "0")
                    .output()?;
                if !output.status.success() {
                    return Err(ComposerError::InvalidRecipe(format!(
                        "could not fetch base revision {revision:?} from {repository:?}: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )));
                }
            }
            let resolved = Command::new("git")
                .arg("-C")
                .arg(checkout.path())
                .args(["rev-parse", "HEAD"])
                .output()?;
            let resolved_ok = resolved.status.success();
            let resolved = String::from_utf8_lossy(&resolved.stdout);
            if !resolved_ok || !resolved.trim().eq_ignore_ascii_case(revision) {
                return Err(ComposerError::InvalidRecipe(format!(
                    "base revision resolved to {:?}, expected {revision:?}",
                    resolved.trim()
                )));
            }
            let root = if subdir == Path::new(".") {
                checkout.path().to_path_buf()
            } else {
                payload_root(checkout.path(), subdir)?
            };
            collect_files(&root)
        }
    }
}

fn default_subdir() -> PathBuf {
    PathBuf::from(".")
}

fn validate_relative(path: &Path) -> Result<(), ComposerError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ComposerError::InvalidRecipe(format!(
            "path must be a safe relative path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn collect_files(root: &Path) -> Result<BTreeMap<PathBuf, PayloadFile>, ComposerError> {
    fn visit(
        root: &Path,
        current: &Path,
        files: &mut BTreeMap<PathBuf, PayloadFile>,
    ) -> Result<(), ComposerError> {
        let mut entries = fs::read_dir(current)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let file_type = entry.file_type()?;
            let path = entry.path();
            if file_type.is_dir() && entry.file_name() == ".git" {
                continue;
            }
            if file_type.is_symlink() {
                return Err(ComposerError::InvalidRecipe(format!(
                    "payload symlinks are not supported: {}",
                    path.display()
                )));
            }
            if file_type.is_dir() {
                visit(root, &path, files)?;
            } else if file_type.is_file() {
                let relative = path.strip_prefix(root).map_err(|error| {
                    ComposerError::InvalidRecipe(format!("invalid payload path: {error}"))
                })?;
                validate_relative(relative)?;
                files.insert(relative.to_path_buf(), read_payload_file(&path)?);
            }
        }
        Ok(())
    }

    if fs::symlink_metadata(root)?.file_type().is_symlink() {
        return Err(ComposerError::InvalidRecipe(format!(
            "payload root cannot be a symlink: {}",
            root.display()
        )));
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

fn write_files(root: &Path, files: &BTreeMap<PathBuf, PayloadFile>) -> Result<(), ComposerError> {
    for (path, file) in files {
        write_file(&root.join(path), file)?;
    }
    Ok(())
}

fn write_file(path: &Path, file: &PayloadFile) -> Result<(), ComposerError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    temp.write_all(&file.contents)?;
    set_executable(temp.path(), file.executable)?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn read_lock(root: &Path) -> Result<CompositionLock, ComposerError> {
    ensure_target_path_safe(root, Path::new(LOCK))?;
    let path = root.join(LOCK);
    if !path.is_file() {
        return Err(ComposerError::NotFound {
            name: "composition lock".into(),
            help: "Run this command from a project created by `devme create`.".into(),
        });
    }
    Ok(toml::from_str(&fs::read_to_string(path)?)?)
}

fn write_lock(root: &Path, lock: &CompositionLock) -> Result<(), ComposerError> {
    ensure_target_path_safe(root, Path::new(LOCK))?;
    write_file(
        &root.join(LOCK),
        &PayloadFile {
            contents: toml::to_string_pretty(lock)?.into_bytes(),
            executable: false,
        },
    )
}

fn backup_path(root: &Path, feature: &str, path: &Path) -> Result<PathBuf, ComposerError> {
    validate_relative(Path::new(feature))?;
    validate_relative(path)?;
    let relative = Path::new(BACKUPS).join(feature).join(path);
    ensure_target_path_safe(root, &relative)?;
    Ok(root.join(relative))
}

fn remove_file_and_empty_parents(root: &Path, path: &Path) -> Result<(), ComposerError> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    let mut parent = path.parent();
    while let Some(directory) = parent {
        if directory == root || directory.starts_with(root.join(".devme")) {
            break;
        }
        if fs::read_dir(directory)?.next().is_none() {
            fs::remove_dir(directory)?;
            parent = directory.parent();
        } else {
            break;
        }
    }
    Ok(())
}

fn begin_journal(
    root: &Path,
    operation: &str,
    feature: Option<String>,
    desired: BTreeMap<PathBuf, Option<String>>,
    backup_features: Vec<String>,
) -> Result<(), ComposerError> {
    ensure_target_path_safe(root, Path::new(".devme/composer"))?;
    let journal_root = root.join(JOURNAL);
    if journal_root.exists() {
        return Err(ComposerError::Conflict {
            paths: vec![PathBuf::from(JOURNAL)],
            help: "Run `devme feature continue` or `devme feature abort` before starting another composition operation.".into(),
        });
    }
    let journal_parent = journal_root.parent().ok_or_else(|| {
        ComposerError::InvalidRecipe("journal path has no parent directory".into())
    })?;
    fs::create_dir_all(journal_parent)?;
    let stage = tempfile::Builder::new()
        .prefix("journal-stage-")
        .tempdir_in(journal_parent)?;
    let journal_stage = stage.path().to_path_buf();
    (|| {
        let mut entries = Vec::new();
        for (path, after_digest) in desired {
            validate_relative(&path)?;
            ensure_target_path_safe(root, &path)?;
            let source = root.join(&path);
            let before_digest = file_digest_optional(&source)?;
            if before_digest.is_some() {
                write_file(
                    &journal_stage.join("before").join(&path),
                    &read_payload_file(&source)?,
                )?;
            }
            entries.push(JournalEntry {
                path,
                before_digest,
                after_digest,
            });
        }
        write_file(
            &journal_stage.join("composition.lock"),
            &read_payload_file(&root.join(LOCK))?,
        )?;
        for name in &backup_features {
            validate_relative(Path::new(name))?;
            ensure_target_path_safe(root, &Path::new(BACKUPS).join(name))?;
            let source = root.join(BACKUPS).join(name);
            if source.is_dir() {
                copy_tree(&source, &journal_stage.join("backups").join(name))?;
            }
        }
        let journal = OperationJournal {
            schema_version: 1,
            operation: operation.into(),
            feature,
            backup_features,
            entries,
        };
        write_file(
            &journal_stage.join("operation.toml"),
            &PayloadFile {
                contents: toml::to_string_pretty(&journal)?.into_bytes(),
                executable: false,
            },
        )?;
        let staged = stage.keep();
        fs::rename(staged, &journal_root)?;
        Ok(())
    })()
}

fn read_journal(root: &Path) -> Result<OperationJournal, ComposerError> {
    ensure_target_path_safe(root, Path::new(JOURNAL).join("operation.toml").as_path())?;
    let path = root.join(JOURNAL).join("operation.toml");
    if !path.is_file() {
        return Err(ComposerError::NotFound {
            name: "pending composition operation".into(),
            help: "No interrupted operation was found in `.devme`.".into(),
        });
    }
    let journal: OperationJournal = toml::from_str(&fs::read_to_string(path)?)?;
    if journal.schema_version != 1 {
        return Err(ComposerError::InvalidRecipe(format!(
            "unsupported operation journal schema {}",
            journal.schema_version
        )));
    }
    Ok(journal)
}

fn abort_journal(root: &Path) -> Result<OperationJournal, ComposerError> {
    let journal = read_journal(root)?;
    let journal_root = root.join(JOURNAL);
    let mut conflicts = Vec::new();
    for entry in &journal.entries {
        ensure_target_path_safe(root, &entry.path)?;
        let current = file_digest_optional(&root.join(&entry.path))?;
        if current != entry.before_digest && current != entry.after_digest {
            conflicts.push(entry.path.clone());
        }
    }
    if !conflicts.is_empty() {
        return Err(ComposerError::Conflict {
            paths: conflicts,
            help: "Files changed after the interruption. Move those edits aside, then rerun `devme feature continue` or `devme feature abort`.".into(),
        });
    }
    for entry in &journal.entries {
        let target = root.join(&entry.path);
        if entry.before_digest.is_some() {
            write_file(
                &target,
                &read_payload_file(&journal_root.join("before").join(&entry.path))?,
            )?;
        } else {
            remove_file_and_empty_parents(root, &target)?;
        }
    }
    write_file(
        &root.join(LOCK),
        &read_payload_file(&journal_root.join("composition.lock"))?,
    )?;
    for name in &journal.backup_features {
        ensure_target_path_safe(root, &Path::new(BACKUPS).join(name))?;
        let target = root.join(BACKUPS).join(name);
        if target.exists() {
            fs::remove_dir_all(&target)?;
        }
        let snapshot = journal_root.join("backups").join(name);
        if snapshot.is_dir() {
            copy_tree(&snapshot, &target)?;
        }
    }
    clear_journal(root)?;
    Ok(journal)
}

fn clear_journal(root: &Path) -> Result<(), ComposerError> {
    ensure_target_path_safe(root, Path::new(JOURNAL))?;
    let path = root.join(JOURNAL);
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), ComposerError> {
    write_files(target, &collect_files(source)?)
}

fn restore_previous(
    root: &Path,
    feature: &str,
    path: &Path,
    file: &LockedFeatureFile,
    lock: &mut CompositionLock,
) -> Result<(), ComposerError> {
    let target = root.join(path);
    let backup = backup_path(root, feature, path)?;
    if file.previous_existed {
        write_file(&target, &read_payload_file(&backup)?)?;
        lock.managed_files.insert(
            path.to_path_buf(),
            file.previous_digest.clone().ok_or_else(|| {
                ComposerError::InvalidRecipe(format!(
                    "missing previous digest for {}",
                    path.display()
                ))
            })?,
        );
    } else {
        remove_file_and_empty_parents(root, &target)?;
        lock.managed_files.remove(path);
    }
    if backup.exists() {
        fs::remove_file(backup)?;
    }
    Ok(())
}

fn digests(files: &BTreeMap<PathBuf, PayloadFile>) -> BTreeMap<PathBuf, String> {
    files
        .iter()
        .map(|(path, file)| (path.clone(), file_digest(file)))
        .collect()
}

fn track_composition_state(files: &mut BTreeMap<PathBuf, PayloadFile>) {
    const MARKER: &str = "# Devme project composition state";
    const RULES: &str = "# Devme project composition state\n!.devme/\n.devme/*\n!.devme/composition.lock\n!.devme/composer/\n.devme/composer/*\n!.devme/composer/backups/\n!.devme/composer/backups/**\n";
    let path = PathBuf::from(".gitignore");
    let file = files.entry(path).or_insert_with(|| PayloadFile {
        contents: Vec::new(),
        executable: false,
    });
    if String::from_utf8_lossy(&file.contents).contains(MARKER) {
        return;
    }
    if !file.contents.is_empty() && !file.contents.ends_with(b"\n") {
        file.contents.push(b'\n');
    }
    if !file.contents.is_empty() {
        file.contents.push(b'\n');
    }
    file.contents.extend_from_slice(RULES.as_bytes());
}

fn file_digest_optional(path: &Path) -> Result<Option<String>, ComposerError> {
    if !path.exists() {
        return Ok(None);
    }
    if !path.is_file() {
        return Err(ComposerError::Conflict {
            paths: vec![path.to_path_buf()],
            help: "Move the non-file path before retrying.".into(),
        });
    }
    Ok(Some(file_digest(&read_payload_file(path)?)))
}

fn read_payload_file(path: &Path) -> Result<PayloadFile, ComposerError> {
    Ok(PayloadFile {
        contents: fs::read(path)?,
        executable: is_executable(path)?,
    })
}

fn file_digest(file: &PayloadFile) -> String {
    let mut bytes = Vec::with_capacity(file.contents.len() + 1);
    bytes.push(u8::from(file.executable));
    bytes.extend_from_slice(&file.contents);
    digest(&bytes)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> Result<bool, io::Error> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> Result<bool, io::Error> {
    Ok(false)
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<(), io::Error> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(if executable { 0o755 } else { 0o644 });
    fs::set_permissions(path, permissions)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> Result<(), io::Error> {
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn lock_identity(lock: &CompositionLock) -> RecipeIdentity {
    RecipeIdentity {
        name: lock.recipe.name.clone(),
        version: lock.recipe.version.clone(),
        digest: lock.recipe.digest.clone(),
    }
}

fn report(
    operation: &str,
    dry_run: bool,
    recipe: RecipeIdentity,
    features: Vec<String>,
    changed_files: Vec<PathBuf>,
    external_steps: Vec<ExternalStep>,
) -> CompositionReport {
    let next_commands = if dry_run {
        vec![operation_command(operation)]
    } else {
        vec!["devme feature list --output toon".into()]
    };
    CompositionReport {
        schema_version: 1,
        operation: operation.into(),
        status: if dry_run { "planned" } else { "applied" }.into(),
        dry_run,
        recipe,
        features,
        changed_files,
        external_steps,
        next_commands,
    }
}

fn manual_steps(steps: &[String]) -> Vec<ExternalStep> {
    steps
        .iter()
        .map(|description| ExternalStep {
            kind: "manual",
            description: description.clone(),
            trusted: false,
        })
        .collect()
}

fn operation_command(operation: &str) -> String {
    match operation {
        "create" => "Rerun the create command without --dry-run".into(),
        "feature_add" => "Rerun the feature add command without --dry-run".into(),
        "feature_remove" => "Rerun the feature remove command without --dry-run".into(),
        "feature_update" => "Rerun the feature update command without --dry-run".into(),
        _ => "Rerun without --dry-run".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_errors_have_the_semantic_cli_contract() {
        let error = ComposerError::Io(io::Error::from(io::ErrorKind::PermissionDenied));

        assert_eq!(error.code(), "permission_denied");
        assert_eq!(error.exit_code(), 4);
    }

    #[test]
    fn interrupted_feature_add_can_continue_without_overwriting_later_edits() {
        let recipe = tempfile::tempdir().unwrap();
        fs::create_dir_all(recipe.path().join("base")).unwrap();
        fs::create_dir_all(recipe.path().join("features/auth")).unwrap();
        fs::write(
            recipe.path().join(MANIFEST),
            r#"schema_version = 1
name = "native"
version = "1.0.0"

[base]
path = "base"

[features.auth]
version = "1.0.0"
path = "features/auth"
"#,
        )
        .unwrap();
        fs::write(recipe.path().join("base/app.txt"), "auth = false\n").unwrap();
        fs::write(recipe.path().join("features/auth/app.txt"), "auth = true\n").unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().join("app");
        let composer = Composer::new();
        composer
            .create(CreateRequest {
                template: "native".into(),
                target: root.clone(),
                source: recipe.path().to_path_buf(),
                source_locator: None,
                features: Vec::new(),
                dry_run: false,
            })
            .unwrap();
        let desired_file = read_payload_file(&recipe.path().join("features/auth/app.txt")).unwrap();
        begin_journal(
            &root,
            "add",
            Some("auth".into()),
            BTreeMap::from([(PathBuf::from("app.txt"), Some(file_digest(&desired_file)))]),
            vec!["auth".into()],
        )
        .unwrap();
        write_file(&root.join("app.txt"), &desired_file).unwrap();

        composer.continue_operation(&root).unwrap();

        assert_eq!(
            fs::read_to_string(root.join("app.txt")).unwrap(),
            "auth = true\n"
        );
        assert!(!root.join(JOURNAL).exists());

        let installed = read_payload_file(&root.join("app.txt")).unwrap();
        begin_journal(
            &root,
            "remove",
            Some("auth".into()),
            BTreeMap::from([(
                PathBuf::from("app.txt"),
                Some(file_digest(
                    &read_payload_file(&recipe.path().join("base/app.txt")).unwrap(),
                )),
            )]),
            vec!["auth".into()],
        )
        .unwrap();
        assert_eq!(
            file_digest(&installed),
            file_digest_optional(&root.join("app.txt"))
                .unwrap()
                .unwrap()
        );
        fs::write(root.join("app.txt"), "auth = edited later\n").unwrap();

        let error = composer.abort_operation(&root).unwrap_err();
        assert!(matches!(error, ComposerError::Conflict { .. }));
        assert_eq!(
            fs::read_to_string(root.join("app.txt")).unwrap(),
            "auth = edited later\n"
        );
    }
}
