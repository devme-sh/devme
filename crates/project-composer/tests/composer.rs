use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use devme_project_composer::{
    AddFeatureRequest, Composer, ComposerError, CreateRequest, RemoveFeatureRequest,
    UpdateFeatureRequest,
};
use tempfile::TempDir;

fn write(path: impl AsRef<Path>, contents: &str) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn recipe() -> TempDir {
    let source = TempDir::new().unwrap();
    write(
        source.path().join("devme-template.toml"),
        r#"schema_version = 1
name = "native"
version = "2026.7.15"

[base]
path = "base"

[features.auth]
version = "1.0.0"
path = "features/auth"
external_steps = ["Create provider credentials after finalizing app identifiers"]
"#,
    );
    write(source.path().join("base/README.md"), "# Native app\n");
    write(
        source.path().join("base/tooling/verify.sh"),
        "#!/bin/sh\nexit 0\n",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            source.path().join("base/tooling/verify.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    write(
        source.path().join("base/app/settings.txt"),
        "auth = false\n",
    );
    write(
        source.path().join("features/auth/app/settings.txt"),
        "auth = true\n",
    );
    write(
        source.path().join("features/auth/app/auth.txt"),
        "better-auth\n",
    );
    source
}

fn create_request(source: &Path, target: PathBuf) -> CreateRequest {
    CreateRequest {
        template: "native".into(),
        target,
        source: source.to_path_buf(),
        source_locator: None,
        features: Vec::new(),
        dry_run: false,
    }
}

#[test]
fn creates_a_pinned_project_and_composes_a_feature() {
    let source = recipe();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();

    let created = composer
        .create(create_request(source.path(), target.clone()))
        .unwrap();

    assert_eq!(created.operation, "create");
    assert_eq!(created.recipe.version, "2026.7.15");
    assert_eq!(
        fs::read_to_string(target.join("app/settings.txt")).unwrap(),
        "auth = false\n"
    );
    assert!(target.join(".devme/composition.lock").is_file());
    assert!(
        fs::read_to_string(target.join(".gitignore"))
            .unwrap()
            .contains("!.devme/composition.lock")
    );
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&target)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        Command::new("git")
            .args([
                "check-ignore",
                "--no-index",
                ".devme/composer/backups/auth/app/settings.txt",
            ])
            .current_dir(&target)
            .status()
            .unwrap()
            .code(),
        Some(1)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(target.join("tooling/verify.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(target.join("README.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }

    let added = composer
        .add_feature(AddFeatureRequest {
            root: target.clone(),
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap();

    assert_eq!(added.operation, "feature_add");
    assert_eq!(added.changed_files.len(), 2);
    assert_eq!(added.external_steps.len(), 1);
    assert_eq!(
        fs::read_to_string(target.join("app/settings.txt")).unwrap(),
        "auth = true\n"
    );
    assert_eq!(
        fs::read_to_string(target.join("app/auth.txt")).unwrap(),
        "better-auth\n"
    );

    let removed = composer
        .remove_feature(RemoveFeatureRequest {
            root: target.clone(),
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap();

    assert_eq!(removed.operation, "feature_remove");
    assert!(removed.external_steps.iter().any(|step| {
        !step.trusted && step.description.contains("does not delete provider data")
    }));
    assert_eq!(
        fs::read_to_string(target.join("app/settings.txt")).unwrap(),
        "auth = false\n"
    );
    assert!(!target.join("app/auth.txt").exists());
}

#[test]
fn refuses_to_overwrite_an_app_modified_managed_file() {
    let source = recipe();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();
    composer
        .create(create_request(source.path(), target.clone()))
        .unwrap();
    write(target.join("app/settings.txt"), "auth = custom\n");

    let error = composer
        .add_feature(AddFeatureRequest {
            root: target,
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap_err();

    let ComposerError::Conflict { paths, help } = error else {
        panic!("expected a conflict");
    };
    assert_eq!(paths, vec![PathBuf::from("app/settings.txt")]);
    assert!(help.contains("devme feature add auth --dry-run"));
}

#[test]
fn dry_run_is_read_only() {
    let source = recipe();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();
    let mut request = create_request(source.path(), target.clone());
    request.features.push("auth".into());
    request.dry_run = true;

    let report = composer.create(request).unwrap();

    assert_eq!(report.operation, "create");
    assert!(report.dry_run);
    assert!(!target.exists());
    assert!(
        report
            .changed_files
            .iter()
            .any(|path| path == Path::new("app/auth.txt"))
    );
}

#[test]
fn lock_keeps_the_portable_recipe_locator_instead_of_the_checkout_path() {
    let source = recipe();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("portable");
    let mut request = create_request(source.path(), target.clone());
    request.source_locator = Some("https://example.invalid/native-template.git".into());

    Composer::new().create(request).unwrap();

    assert_eq!(
        Composer::new().recipe_locator(&target).unwrap(),
        "https://example.invalid/native-template.git"
    );
}

#[test]
fn creates_from_a_git_pinned_base_payload() {
    let base = TempDir::new().unwrap();
    write(base.path().join("README.md"), "# Pinned base\n");
    let run = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(base.path())
                .status()
                .unwrap()
                .success()
        );
    };
    run(&["init", "-q"]);
    run(&["add", "README.md"]);
    run(&[
        "-c",
        "user.name=Devme Test",
        "-c",
        "user.email=devme@example.invalid",
        "commit",
        "-q",
        "-m",
        "base",
    ]);
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(base.path())
        .output()
        .unwrap();
    assert!(revision.status.success());
    let revision = String::from_utf8(revision.stdout).unwrap();
    let revision = revision.trim();

    let source = TempDir::new().unwrap();
    write(
        source.path().join("devme-template.toml"),
        &format!(
            r#"schema_version = 1
name = "native"
version = "2026.7.15"

[base]
repository = {:?}
revision = {:?}
subdir = "."
"#,
            base.path().display().to_string(),
            revision,
        ),
    );
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("pinned");

    Composer::new()
        .create(create_request(source.path(), target.clone()))
        .unwrap();

    assert_eq!(
        fs::read_to_string(target.join("README.md")).unwrap(),
        "# Pinned base\n"
    );
    assert!(!target.join(".git").exists());
}

#[test]
fn updates_an_installed_feature_without_losing_its_original_backup() {
    let source = recipe();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();
    let mut request = create_request(source.path(), target.clone());
    request.features.push("auth".into());
    composer.create(request).unwrap();

    write(
        source.path().join("devme-template.toml"),
        r#"schema_version = 1
name = "native"
version = "2026.7.16"

[base]
path = "base"

[features.auth]
version = "1.1.0"
path = "features/auth"
"#,
    );
    write(
        source.path().join("features/auth/app/settings.txt"),
        "auth = refreshed\n",
    );
    fs::remove_file(source.path().join("features/auth/app/auth.txt")).unwrap();
    write(
        source.path().join("features/auth/app/session.txt"),
        "session = secure\n",
    );

    let updated = composer
        .update_feature(UpdateFeatureRequest {
            root: target.clone(),
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap();

    assert_eq!(updated.operation, "feature_update");
    assert_eq!(updated.recipe.version, "2026.7.16");
    assert_eq!(
        fs::read_to_string(target.join("app/settings.txt")).unwrap(),
        "auth = refreshed\n"
    );
    assert!(!target.join("app/auth.txt").exists());
    assert_eq!(
        fs::read_to_string(target.join("app/session.txt")).unwrap(),
        "session = secure\n"
    );

    composer
        .remove_feature(RemoveFeatureRequest {
            root: target.clone(),
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap();
    assert_eq!(
        fs::read_to_string(target.join("app/settings.txt")).unwrap(),
        "auth = false\n"
    );
    assert!(!target.join("app/session.txt").exists());
}

#[test]
fn refuses_to_remove_a_feature_required_by_an_installed_feature() {
    let source = recipe();
    write(
        source.path().join("devme-template.toml"),
        r#"schema_version = 1
name = "native"
version = "2026.7.15"

[base]
path = "base"

[features.auth]
version = "1.0.0"
path = "features/auth"

[features.payments]
version = "1.0.0"
path = "features/payments"
dependencies = ["auth"]
"#,
    );
    write(
        source.path().join("features/payments/app/payments.txt"),
        "stripe\n",
    );
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();
    composer
        .create(create_request(source.path(), target.clone()))
        .unwrap();
    composer
        .add_feature(AddFeatureRequest {
            root: target.clone(),
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap();
    composer
        .add_feature(AddFeatureRequest {
            root: target.clone(),
            feature: "payments".into(),
            dry_run: false,
        })
        .unwrap();

    let error = composer
        .remove_feature(RemoveFeatureRequest {
            root: target,
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap_err();

    let ComposerError::Usage { message, help } = error else {
        panic!("expected a usage error");
    };
    assert!(message.contains("payments"));
    assert!(help.contains("devme feature remove payments"));
}

#[test]
fn dependent_feature_can_overlay_and_restore_dependency_owned_files() {
    let source = recipe();
    write(
        source.path().join("devme-template.toml"),
        r#"schema_version = 1
name = "native"
version = "2026.7.16"

[base]
path = "base"

[features.auth]
version = "1.0.0"
path = "features/auth"

[features.stripe]
version = "1.0.0"
path = "features/stripe"
dependencies = ["auth"]
"#,
    );
    write(
        source.path().join("features/stripe/app/settings.txt"),
        "auth = true\nstripe = true\n",
    );
    write(
        source.path().join("features/stripe/app/stripe.txt"),
        "stripe\n",
    );
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();
    composer
        .create(create_request(source.path(), target.clone()))
        .unwrap();

    composer
        .add_feature(AddFeatureRequest {
            root: target.clone(),
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap();
    composer
        .add_feature(AddFeatureRequest {
            root: target.clone(),
            feature: "stripe".into(),
            dry_run: false,
        })
        .unwrap();

    assert_eq!(
        fs::read_to_string(target.join("app/settings.txt")).unwrap(),
        "auth = true\nstripe = true\n"
    );
    composer
        .remove_feature(RemoveFeatureRequest {
            root: target.clone(),
            feature: "stripe".into(),
            dry_run: false,
        })
        .unwrap();
    assert_eq!(
        fs::read_to_string(target.join("app/settings.txt")).unwrap(),
        "auth = true\n"
    );
    assert!(!target.join("app/stripe.txt").exists());

    composer
        .remove_feature(RemoveFeatureRequest {
            root: target.clone(),
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap();
    assert_eq!(
        fs::read_to_string(target.join("app/settings.txt")).unwrap(),
        "auth = false\n"
    );
}

#[test]
fn modified_dependency_overlay_blocks_feature_removal() {
    let source = layered_recipe();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();
    let mut request = create_request(source.path(), target.clone());
    request.features = vec!["auth".into(), "stripe".into()];
    composer.create(request).unwrap();
    write(
        target.join("app/settings.txt"),
        "auth = true\nstripe = customized\n",
    );

    let error = composer
        .remove_feature(RemoveFeatureRequest {
            root: target,
            feature: "stripe".into(),
            dry_run: false,
        })
        .unwrap_err();

    let ComposerError::Conflict { paths, .. } = error else {
        panic!("expected a conflict");
    };
    assert_eq!(paths, vec![PathBuf::from("app/settings.txt")]);
}

#[test]
fn dependency_update_conflicts_while_dependent_overlay_is_installed() {
    let source = layered_recipe();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();
    let mut request = create_request(source.path(), target.clone());
    request.features = vec!["auth".into(), "stripe".into()];
    composer.create(request).unwrap();
    write(
        source.path().join("devme-template.toml"),
        r#"schema_version = 1
name = "native"
version = "2026.7.17"

[base]
path = "base"

[features.auth]
version = "1.1.0"
path = "features/auth"

[features.stripe]
version = "1.0.0"
path = "features/stripe"
dependencies = ["auth"]
"#,
    );
    write(
        source.path().join("features/auth/app/settings.txt"),
        "auth = refreshed\n",
    );

    let error = composer
        .update_feature(UpdateFeatureRequest {
            root: target,
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap_err();

    let ComposerError::Conflict { paths, help } = error else {
        panic!("expected a conflict");
    };
    assert_eq!(paths, vec![PathBuf::from("app/settings.txt")]);
    assert!(help.contains("devme feature update auth --dry-run"));
}

fn layered_recipe() -> TempDir {
    let source = recipe();
    write(
        source.path().join("devme-template.toml"),
        r#"schema_version = 1
name = "native"
version = "2026.7.16"

[base]
path = "base"

[features.auth]
version = "1.0.0"
path = "features/auth"

[features.stripe]
version = "1.0.0"
path = "features/stripe"
dependencies = ["auth"]
"#,
    );
    write(
        source.path().join("features/stripe/app/settings.txt"),
        "auth = true\nstripe = true\n",
    );
    source
}

#[test]
fn creation_can_compose_dependency_overlays_in_order() {
    let source = recipe();
    write(
        source.path().join("devme-template.toml"),
        r#"schema_version = 1
name = "native"
version = "2026.7.16"

[base]
path = "base"

[features.auth]
version = "1.0.0"
path = "features/auth"

[features.stripe]
version = "1.0.0"
path = "features/stripe"
dependencies = ["auth"]
"#,
    );
    write(
        source.path().join("features/stripe/app/settings.txt"),
        "auth = true\nstripe = true\n",
    );
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let mut request = create_request(source.path(), target.clone());
    request.features = vec!["auth".into(), "stripe".into()];

    Composer::new().create(request).unwrap();

    assert_eq!(
        fs::read_to_string(target.join("app/settings.txt")).unwrap(),
        "auth = true\nstripe = true\n"
    );
}

#[test]
fn refuses_overlapping_feature_payloads_before_creating_the_project() {
    let source = recipe();
    write(
        source.path().join("devme-template.toml"),
        r#"schema_version = 1
name = "native"
version = "2026.7.15"

[base]
path = "base"

[features.auth]
version = "1.0.0"
path = "features/auth"

[features.analytics]
version = "1.0.0"
path = "features/analytics"
"#,
    );
    write(
        source.path().join("features/analytics/App/Settings.txt"),
        "analytics = true\n",
    );
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();
    let mut request = create_request(source.path(), target.clone());
    request.features = vec!["auth".into(), "analytics".into()];

    let error = composer.create(request).unwrap_err();

    let ComposerError::Conflict { paths, help } = error else {
        panic!("expected a conflict");
    };
    assert_eq!(paths, vec![PathBuf::from("App/Settings.txt")]);
    assert!(help.contains("separate files"));
    assert!(!target.exists());
}

#[test]
fn rejects_payload_changes_without_a_recipe_version_change() {
    let source = recipe();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();
    composer
        .create(create_request(source.path(), target.clone()))
        .unwrap();
    write(
        source.path().join("features/auth/app/auth.txt"),
        "unexpected replacement\n",
    );

    let error = composer
        .add_feature(AddFeatureRequest {
            root: target,
            feature: "auth".into(),
            dry_run: true,
        })
        .unwrap_err();

    let ComposerError::InvalidRecipe(message) = error else {
        panic!("expected an invalid recipe");
    };
    assert!(message.contains("changed without a version change"));
}

#[cfg(unix)]
#[test]
fn refuses_target_symlinks_that_escape_the_project_root() {
    use std::os::unix::fs::symlink;

    let source = recipe();
    write(
        source.path().join("features/auth/escape/credential.txt"),
        "must stay inside project\n",
    );
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let outside = workspace.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let composer = Composer::new();
    composer
        .create(create_request(source.path(), target.clone()))
        .unwrap();
    symlink(&outside, target.join("escape")).unwrap();

    let error = composer
        .add_feature(AddFeatureRequest {
            root: target,
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap_err();

    let ComposerError::Conflict { paths, .. } = error else {
        panic!("expected a conflict");
    };
    assert!(paths.contains(&PathBuf::from("escape/credential.txt")));
    assert!(!outside.join("credential.txt").exists());
}

#[test]
fn refuses_to_adopt_an_identical_app_owned_file() {
    let source = recipe();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();
    composer
        .create(create_request(source.path(), target.clone()))
        .unwrap();
    write(target.join("app/auth.txt"), "better-auth\n");

    let error = composer
        .add_feature(AddFeatureRequest {
            root: target,
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap_err();

    let ComposerError::Conflict { paths, .. } = error else {
        panic!("expected a conflict");
    };
    assert!(paths.contains(&PathBuf::from("app/auth.txt")));
}

#[test]
fn rejects_payloads_that_claim_composer_authority_files() {
    let source = recipe();
    write(
        source.path().join("features/auth/.DEVME/composition.lock"),
        "untrusted lock\n",
    );
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let mut request = create_request(source.path(), target.clone());
    request.features.push("auth".into());

    let error = Composer::new().create(request).unwrap_err();

    let ComposerError::InvalidRecipe(message) = error else {
        panic!("expected an invalid recipe");
    };
    assert!(message.contains("reserved for project composition"));
    assert!(!target.exists());
}

#[test]
fn rejects_a_case_alias_of_the_base_gitignore() {
    let source = recipe();
    write(source.path().join("base/.GitIgnore"), "build/\n");
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");

    let error = Composer::new()
        .create(create_request(source.path(), target.clone()))
        .unwrap_err();

    assert!(matches!(error, ComposerError::InvalidRecipe(_)));
    assert!(!target.exists());
}

#[cfg(unix)]
#[test]
fn rejects_a_symlinked_payload_root() {
    use std::os::unix::fs::symlink;

    let source = recipe();
    let outside = TempDir::new().unwrap();
    write(outside.path().join("auth/secret.txt"), "outside\n");
    fs::remove_dir_all(source.path().join("features")).unwrap();
    symlink(outside.path(), source.path().join("features")).unwrap();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");

    let error = Composer::new()
        .create(create_request(source.path(), target))
        .unwrap_err();

    let ComposerError::InvalidRecipe(message) = error else {
        panic!("expected an invalid recipe");
    };
    assert!(message.contains("real directories inside the recipe"));
}

#[test]
fn rejects_a_case_only_base_feature_collision_before_creation() {
    let source = recipe();
    fs::remove_file(source.path().join("features/auth/app/settings.txt")).unwrap();
    write(
        source.path().join("features/auth/App/Settings.txt"),
        "auth = true\n",
    );
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let mut request = create_request(source.path(), target.clone());
    request.features.push("auth".into());

    let error = Composer::new().create(request).unwrap_err();

    assert!(matches!(error, ComposerError::Conflict { .. }));
    assert!(!target.exists());
}

#[test]
fn rejects_recipe_identity_changes() {
    let source = recipe();
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();
    composer
        .create(create_request(source.path(), target.clone()))
        .unwrap();
    let manifest = fs::read_to_string(source.path().join("devme-template.toml"))
        .unwrap()
        .replace("name = \"native\"", "name = \"other\"")
        .replace("2026.7.15", "2026.7.16");
    write(source.path().join("devme-template.toml"), &manifest);

    let error = composer.list_features(&target).unwrap_err();

    let ComposerError::InvalidRecipe(message) = error else {
        panic!("expected an invalid recipe");
    };
    assert!(message.contains("recipe identity changed"));
}

#[test]
fn rejects_nested_feature_names() {
    let source = recipe();
    let manifest = fs::read_to_string(source.path().join("devme-template.toml"))
        .unwrap()
        .replace("[features.auth]", "[features.\"auth/nested\"]");
    write(source.path().join("devme-template.toml"), &manifest);
    let workspace = TempDir::new().unwrap();

    let error = Composer::new()
        .create(create_request(
            source.path(),
            workspace.path().join("groceries"),
        ))
        .unwrap_err();

    let ComposerError::InvalidRecipe(message) = error else {
        panic!("expected an invalid recipe");
    };
    assert!(message.contains("one portable path component"));
}

#[test]
fn refuses_case_only_aliases_of_app_owned_files() {
    let source = recipe();
    fs::remove_file(source.path().join("features/auth/app/auth.txt")).unwrap();
    write(
        source.path().join("features/auth/app/AUTH.txt"),
        "better-auth\n",
    );
    let workspace = TempDir::new().unwrap();
    let target = workspace.path().join("groceries");
    let composer = Composer::new();
    composer
        .create(create_request(source.path(), target.clone()))
        .unwrap();
    write(target.join("app/auth.txt"), "app owned\n");

    let error = composer
        .add_feature(AddFeatureRequest {
            root: target,
            feature: "auth".into(),
            dry_run: false,
        })
        .unwrap_err();

    assert!(matches!(error, ComposerError::Conflict { .. }));
}
