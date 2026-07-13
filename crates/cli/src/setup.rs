//! Conservative native-monorepo detection. Output is explicit configuration,
//! never a runtime-discovered build graph.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// A file produced by split setup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedConfig {
    pub path: PathBuf,
    pub contents: String,
}

/// An ordered, write-safe split setup plan. The root config is always first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupPlan {
    pub files: Vec<GeneratedConfig>,
}

impl SetupPlan {
    pub fn file(&self, path: &Path) -> Option<&str> {
        self.files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.contents.as_str())
    }

    /// Write every generated config, but only after proving none would be
    /// overwritten. This keeps a partial split from corrupting an existing
    /// workspace layout.
    pub fn write(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let destinations = self
            .files
            .iter()
            .map(|file| root.join(&file.path))
            .collect::<Vec<_>>();
        if let Some(path) = destinations.iter().find(|path| path.exists()) {
            bail!("{} already exists; refusing to overwrite", path.display());
        }
        for (file, destination) in self.files.iter().zip(&destinations) {
            let parent = destination.parent().expect("generated file has a parent");
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
            std::fs::write(destination, &file.contents)
                .with_context(|| format!("cannot write {}", destination.display()))?;
        }
        Ok(destinations)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Markers {
    swift: bool,
    xcode: bool,
    gradle: bool,
    android: bool,
    convex: bool,
    vite: bool,
    gradle_wrapper: bool,
    gradle_settings: bool,
    package_json: bool,
    dev_script: bool,
}

impl Markers {
    fn any(self) -> bool {
        self.swift || self.xcode || self.gradle || self.convex || (self.vite && self.dev_script)
    }
}

/// Produce the compatible single-file setup preview. Commands are emitted
/// with explicit `cwd` values when their marker belongs to a nested app.
pub fn detect(root: &Path) -> Result<String> {
    let mut projects = discover(root)?;
    coalesce_gradle_projects(&mut projects);
    let names = project_names(projects.keys());
    let mut out = header();
    for (relative, markers) in &projects {
        let prefix = if relative.as_os_str().is_empty() {
            String::new()
        } else {
            format!("{}-", names[relative])
        };
        let gradle_owner = gradle_owner(&projects, relative);
        append_project(
            &mut out,
            *markers,
            &prefix,
            relative,
            &gradle_owner,
            projects[&gradle_owner].gradle_wrapper,
        );
    }
    ensure_useful(&out)?;
    Ok(out)
}

/// Produce a one-level explicit workspace. Each non-root marker-owning
/// directory gets a local child config; Devme still does not infer edges
/// between tools or reproduce their build graphs.
pub fn detect_split(root: &Path) -> Result<SetupPlan> {
    let mut projects = discover(root)?;
    coalesce_gradle_projects(&mut projects);
    reject_overlapping_projects(projects.keys())?;
    let names = project_names(projects.keys());
    let members = projects
        .keys()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|path| (names[path].clone(), path.clone()))
        .collect::<Vec<_>>();

    let mut root_config = header();
    if !members.is_empty() {
        root_config.push_str("\n[workspace.members]\n");
        for (name, path) in &members {
            root_config.push_str(&format!(
                "{} = {}\n",
                toml_key(name),
                toml_string(&path.to_string_lossy())
            ));
        }
    }
    if let Some(markers) = projects.get(Path::new("")) {
        let gradle_owner = gradle_owner(&projects, Path::new(""));
        append_project(
            &mut root_config,
            *markers,
            "",
            Path::new(""),
            &gradle_owner,
            projects[&gradle_owner].gradle_wrapper,
        );
    }

    let mut files = vec![GeneratedConfig {
        path: PathBuf::from("devme.toml"),
        contents: root_config,
    }];
    for (_, path) in members {
        let gradle_owner = gradle_owner(&projects, &path);
        let gradle_cwd = path_from_member_to_ancestor(&path, &gradle_owner);
        let mut contents = header();
        append_project(
            &mut contents,
            projects[&path],
            "",
            Path::new(""),
            &gradle_cwd,
            projects[&gradle_owner].gradle_wrapper,
        );
        files.push(GeneratedConfig {
            path: path.join("devme.toml"),
            contents,
        });
    }
    Ok(SetupPlan { files })
}

fn header() -> String {
    "schema_version = 1\n\n[stack]\nname = \"native-monorepo\"\n".to_string()
}

fn discover(root: &Path) -> Result<BTreeMap<PathBuf, Markers>> {
    let mut projects = BTreeMap::<PathBuf, Markers>::new();
    for path in walk(root, 6) {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let owner = path.parent().unwrap_or(root);
        let relative = owner.strip_prefix(root).unwrap_or(owner).to_path_buf();
        let marker = projects.entry(relative).or_default();
        match name {
            "Package.swift" => marker.swift = true,
            "settings.gradle.kts" | "settings.gradle" => {
                marker.gradle = true;
                marker.gradle_settings = true;
                let contents = std::fs::read_to_string(&path).unwrap_or_default();
                marker.android |= contents.contains("com.android.")
                    || contents.contains("android {")
                    || contents.contains("android{");
            }
            "build.gradle.kts" | "build.gradle" => {
                marker.gradle = true;
                let contents = std::fs::read_to_string(&path).unwrap_or_default();
                marker.android |= contents.contains("com.android.")
                    || contents.contains("android {")
                    || contents.contains("android{");
            }
            "gradlew" => {
                marker.gradle = true;
                marker.gradle_wrapper = true;
            }
            "convex.json" => marker.convex = true,
            "vite.config.ts" | "vite.config.js" | "vite.config.mts" | "vite.config.mjs" => {
                marker.vite = true;
            }
            "package.json" => {
                let package = std::fs::read_to_string(&path).unwrap_or_default();
                marker.package_json = true;
                marker.convex |= package.contains("\"convex\"");
                marker.vite |= package.contains("\"vite\"") || package.contains("vite-plus");
                marker.dev_script |= serde_json::from_str::<serde_json::Value>(&package)
                    .ok()
                    .and_then(|value| value.pointer("/scripts/dev")?.as_str().map(str::to_owned))
                    .is_some_and(|command| !command.trim().is_empty());
            }
            "convex.config.ts" if owner.file_name().is_some_and(|name| name == "convex") => {
                let project = owner.parent().unwrap_or(root);
                let relative = project.strip_prefix(root).unwrap_or(project).to_path_buf();
                projects.entry(relative).or_default().convex = true;
            }
            _ if name.ends_with(".xcodeproj") || name.ends_with(".xcworkspace") => {
                marker.xcode = true;
            }
            _ => {}
        }
    }
    projects.retain(|_, marker| marker.any());
    if projects.is_empty() {
        bail!("no supported Xcode, Swift, Gradle/Android, Convex, or Vite+ project markers found");
    }
    Ok(projects)
}

fn walk(root: &Path, depth: usize) -> Vec<PathBuf> {
    if depth == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = entry.file_type().ok();
            if path.file_name().is_some_and(|name| {
                name == ".git"
                    || name == ".devme"
                    || name == "node_modules"
                    || name == "target"
                    || name == "DerivedData"
                    || name == "build"
            }) {
                continue;
            }
            out.push(path.clone());
            if file_type.is_some_and(|file_type| file_type.is_dir() && !file_type.is_symlink())
                && !path
                    .extension()
                    .is_some_and(|extension| extension == "xcodeproj" || extension == "xcworkspace")
            {
                out.extend(walk(&path, depth - 1));
            }
        }
    }
    out
}

fn project_names<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> HashMap<PathBuf, String> {
    let paths = paths.cloned().collect::<Vec<_>>();
    let mut base_counts = HashMap::<String, usize>::new();
    for path in paths.iter().filter(|path| !path.as_os_str().is_empty()) {
        *base_counts
            .entry(slug(path.file_name().unwrap()))
            .or_default() += 1;
    }
    let mut result = HashMap::new();
    let mut used = HashSet::new();
    for path in paths
        .into_iter()
        .filter(|path| !path.as_os_str().is_empty())
    {
        let base = slug(path.file_name().unwrap());
        let candidate = if base_counts[&base] == 1 {
            base
        } else {
            slug(path.as_os_str())
        };
        let mut name = candidate.clone();
        let mut suffix = 2;
        while !used.insert(name.clone()) {
            name = format!("{candidate}-{suffix}");
            suffix += 1;
        }
        result.insert(path, name);
    }
    result
}

fn slug(value: &std::ffi::OsStr) -> String {
    let mut out = String::new();
    let mut separator = false;
    for character in value.to_string_lossy().chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            out.push(character.to_ascii_lowercase());
            separator = false;
        } else if !separator && !out.is_empty() {
            out.push('-');
            separator = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "app".to_string()
    } else {
        out
    }
}

fn reject_overlapping_projects<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> Result<()> {
    let paths = paths
        .filter(|path| !path.as_os_str().is_empty())
        .collect::<Vec<_>>();
    for (index, first) in paths.iter().enumerate() {
        for second in paths.iter().skip(index + 1) {
            if first.starts_with(second) || second.starts_with(first) {
                bail!(
                    "cannot split nested project directories {} and {}; keep the single-file config or choose non-overlapping workspace members",
                    first.display(),
                    second.display()
                );
            }
        }
    }
    Ok(())
}

/// Treat modules owned by one Gradle settings file as one detected project.
/// Gradle remains authoritative for the module graph, while Devme sees only
/// the explicit build root that can safely become a workspace member.
fn coalesce_gradle_projects(projects: &mut BTreeMap<PathBuf, Markers>) {
    let paths = projects.keys().cloned().collect::<Vec<_>>();
    for path in paths {
        let Some(markers) = projects.get(&path).copied() else {
            continue;
        };
        if !markers.gradle {
            continue;
        }
        let owner = projects
            .iter()
            .filter(|(candidate, candidate_markers)| {
                candidate_markers.gradle_settings
                    && !candidate.as_os_str().is_empty()
                    && path.starts_with(candidate)
                    && candidate.as_path() != path
            })
            .max_by_key(|(candidate, _)| candidate.components().count())
            .map(|(candidate, _)| candidate.clone());
        let Some(owner) = owner else {
            continue;
        };

        if let Some(owner_markers) = projects.get_mut(&owner) {
            owner_markers.android |= markers.android;
        }
        if let Some(module_markers) = projects.get_mut(&path) {
            module_markers.gradle = false;
            module_markers.android = false;
            module_markers.gradle_wrapper = false;
            module_markers.gradle_settings = false;
        }
    }
    projects.retain(|_, markers| markers.any());
}

fn gradle_owner(projects: &BTreeMap<PathBuf, Markers>, project: &Path) -> PathBuf {
    let settings_owner = projects
        .iter()
        .filter(|(candidate, markers)| markers.gradle_settings && project.starts_with(candidate))
        .max_by_key(|(candidate, _)| candidate.components().count());
    if let Some((candidate, markers)) = settings_owner
        && markers.gradle_wrapper
    {
        return candidate.clone();
    }
    project.to_path_buf()
}

fn path_from_member_to_ancestor(member: &Path, ancestor: &Path) -> PathBuf {
    let depth = member
        .strip_prefix(ancestor)
        .expect("Gradle wrapper owner is an ancestor of its project")
        .components()
        .count();
    std::iter::repeat_n("..", depth).collect()
}

fn cwd_fragments(cwd: &Path) -> (String, String) {
    let cwd_line = if cwd.as_os_str().is_empty() {
        String::new()
    } else {
        format!("cwd = {}\n", toml_string(&cwd.to_string_lossy()))
    };
    let shell_prefix = if cwd.as_os_str().is_empty() {
        String::new()
    } else {
        let quoted = cwd.to_string_lossy().replace('\'', "'\\''");
        format!("cd '{quoted}' && ")
    };
    (cwd_line, shell_prefix)
}

fn append_project(
    out: &mut String,
    markers: Markers,
    prefix: &str,
    cwd: &Path,
    gradle_cwd: &Path,
    gradle_wrapper: bool,
) {
    let (cwd_line, shell_prefix) = cwd_fragments(cwd);

    if markers.convex || (markers.vite && markers.dev_script) {
        out.push_str(&format!(
            "\n[step.{prefix}bun]\ncheck = \"command -v bun >/dev/null || test -x \\\"$HOME/.bun/bin/bun\\\"\"\nprovision = \"curl -fsSL https://bun.sh/install | bash\"\n"
        ));
    }

    if markers.package_json && (markers.convex || (markers.vite && markers.dev_script)) {
        let check = toml_string(&format!("{shell_prefix}test -d node_modules"));
        let provision = toml_string(&format!("{shell_prefix}{}", bun_command("install")));
        out.push_str(&format!(
            "\n[step.{prefix}dependencies]\ncheck = {check}\nprovision = {provision}\ntrust = \"auto\"\ndepends_on = [\"{prefix}bun\"]\n"
        ));
    }

    if markers.swift {
        let check = toml_string(&format!(
            "{shell_prefix}swift package show-dependencies >/dev/null"
        ));
        let provision = toml_string(&format!("{shell_prefix}swift package resolve"));
        out.push_str(&format!(
            "\n[step.{prefix}swift-deps]\ncheck = {check}\nprovision = {provision}\ntrust = \"auto\"\n\n[task.{prefix}swift-test]\ncmd = \"swift test\"\n{cwd_line}steps = [\"{prefix}swift-deps\"]\n"
        ));
    }
    if markers.xcode {
        out.push_str(&format!(
            "\n[resource.{prefix}ios-simulator]\nscope = \"host\"\ncapacity = 1\nenv = \"SIMULATOR_SLOT\"\n\n[task.{prefix}ios-build]\ncmd = \"xcodebuild build\"\n{cwd_line}resources = [\"{prefix}ios-simulator\"]\ntimeout = 1200\n"
        ));
    }
    if markers.gradle {
        let (gradle_cwd_line, gradle_shell_prefix) = cwd_fragments(gradle_cwd);
        let executable = if gradle_wrapper {
            "./gradlew"
        } else {
            "gradle"
        };
        let step = if gradle_wrapper {
            let check = toml_string(&format!("{gradle_shell_prefix}test -x ./gradlew"));
            let provision = toml_string(&format!("{gradle_shell_prefix}chmod +x ./gradlew"));
            out.push_str(&format!(
                "\n[step.{prefix}gradle-deps]\ncheck = {check}\nprovision = {provision}\ntrust = \"auto\"\n"
            ));
            format!("steps = [\"{prefix}gradle-deps\"]\n")
        } else {
            String::new()
        };
        if markers.android {
            out.push_str(&format!(
                "\n[resource.{prefix}android-emulator]\nscope = \"host\"\ncapacity = 1\nenv = \"EMULATOR_SLOT\"\n\n[task.{prefix}android-test]\ncmd = \"{executable} --no-daemon test\"\n{gradle_cwd_line}{step}resources = [\"{prefix}android-emulator\"]\ntimeout = 1200\n"
            ));
        } else {
            out.push_str(&format!(
                "\n[task.{prefix}gradle-test]\ncmd = \"{executable} --no-daemon test\"\n{gradle_cwd_line}{step}timeout = 1200\n"
            ));
        }
    }
    if markers.convex {
        let health = toml_string(&format!(
            "{shell_prefix}{} >/dev/null",
            bun_command("x convex function-spec")
        ));
        let command = toml_string(&bun_command("x convex dev"));
        let dependencies = if markers.package_json {
            format!("depends_on = [\"{prefix}dependencies\"]\n")
        } else {
            format!("depends_on = [\"{prefix}bun\"]\n")
        };
        out.push_str(&format!(
            "\n[service.{prefix}convex]\ncmd = {command}\n{cwd_line}{dependencies}health = {{ shell = {health} }}\nreadiness = {{ interval_ms = 1000, timeout_ms = 5000, retries = 60 }}\n"
        ));
    }
    if markers.vite && markers.dev_script {
        let command = toml_string(&bun_command("run dev -- --port {port}"));
        let dependencies = if markers.package_json {
            format!("depends_on = [\"{prefix}dependencies\"]\n")
        } else {
            format!("depends_on = [\"{prefix}bun\"]\n")
        };
        out.push_str(&format!(
            "\n[service.{prefix}web]\ncmd = {command}\n{cwd_line}{dependencies}port = {{ base = 5173, slot_offset = 10 }}\nhealth = {{ http = \"http://localhost:{{port}}\" }}\n"
        ));
    }
}

fn bun_command(arguments: &str) -> String {
    format!(
        "BUN_BIN=$(command -v bun 2>/dev/null || printf '%s' \"$HOME/.bun/bin/bun\"); \"$BUN_BIN\" {arguments}"
    )
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

fn toml_key(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
    {
        value.to_string()
    } else {
        toml_string(value)
    }
}

fn ensure_useful(config: &str) -> Result<()> {
    if config.matches("[task.").count() == 0 && config.matches("[service.").count() == 0 {
        bail!("no runnable tasks or services could be generated from the detected markers");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn detects_native_backend_and_web_without_build_graph_inference() {
        let dir = TempDir::new().unwrap();
        for path in [
            "App.xcworkspace",
            "Package.swift",
            "settings.gradle.kts",
            "convex.json",
            "vite.config.ts",
            "package.json",
        ] {
            let path = dir.path().join(path);
            if path.extension().is_none() {
                std::fs::create_dir_all(path).unwrap();
            } else {
                std::fs::write(path, "").unwrap();
            }
        }
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"dev":"vite"},"dependencies":{"convex":"1.0.0"},"devDependencies":{"vite":"7.0.0"}}"#,
        )
        .unwrap();
        let config = detect(dir.path()).unwrap();
        let stack = devme_config::Stack::parse(&config).unwrap();
        devme_config::validate(&stack).unwrap();
        assert!(stack.task.contains_key("ios-build"));
        assert!(stack.task.contains_key("gradle-test"));
        assert!(stack.service.contains_key("convex"));
        assert!(stack.service.contains_key("web"));
        assert!(stack.step.contains_key("bun"));
        assert!(stack.step.contains_key("dependencies"));
        assert_eq!(stack.service["convex"].depends_on[0].name, "dependencies");
        assert_eq!(stack.step["dependencies"].depends_on[0].name, "bun");
    }

    #[test]
    fn single_file_detection_keeps_commands_in_marker_owning_directories() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/ios")).unwrap();
        std::fs::create_dir_all(dir.path().join("backend")).unwrap();
        std::fs::write(dir.path().join("apps/ios/App.xcworkspace"), "").unwrap();
        std::fs::write(dir.path().join("apps/ios/Package.swift"), "").unwrap();
        std::fs::write(dir.path().join("backend/convex.json"), "").unwrap();

        let config = detect(dir.path()).unwrap();
        let stack = devme_config::Stack::parse(&config).unwrap();
        devme_config::validate(&stack).unwrap();

        assert_eq!(stack.task["ios-ios-build"].cwd.as_deref(), Some("apps/ios"));
        assert_eq!(
            stack.service["backend-convex"].cwd.as_deref(),
            Some("backend")
        );
        assert!(
            stack.step["ios-swift-deps"]
                .check
                .starts_with("cd 'apps/ios' && ")
        );
        let devme_core::HealthCheck::Shell { shell } =
            stack.service["backend-convex"].health.as_ref().unwrap()
        else {
            panic!("Convex must use a command readiness probe");
        };
        assert!(shell.starts_with("cd 'backend' && "));
        assert!(stack.workspace.is_none());
    }

    #[test]
    fn split_detection_emits_explicit_members_and_local_child_configs() {
        let dir = TempDir::new().unwrap();
        for directory in ["apps/ios", "apps/android", "backend", "web"] {
            std::fs::create_dir_all(dir.path().join(directory)).unwrap();
        }
        std::fs::write(dir.path().join("apps/ios/App.xcodeproj"), "").unwrap();
        std::fs::write(dir.path().join("apps/android/settings.gradle.kts"), "").unwrap();
        std::fs::write(
            dir.path().join("apps/android/build.gradle.kts"),
            "plugins { id(\"com.android.application\") }",
        )
        .unwrap();
        std::fs::write(dir.path().join("backend/convex.json"), "").unwrap();
        std::fs::write(dir.path().join("web/vite.config.ts"), "").unwrap();
        std::fs::write(
            dir.path().join("web/package.json"),
            r#"{"scripts":{"dev":"vite"},"devDependencies":{"vite":"7.0.0"}}"#,
        )
        .unwrap();

        let plan = detect_split(dir.path()).unwrap();
        let root = plan.file(Path::new("devme.toml")).unwrap();
        let root_stack = devme_config::Stack::parse(root).unwrap();
        let members = &root_stack.workspace.unwrap().members;
        assert_eq!(members["ios"], "apps/ios");
        assert_eq!(members["android"], "apps/android");
        assert_eq!(members["backend"], "backend");
        assert_eq!(members["web"], "web");

        let ios = devme_config::Stack::parse(plan.file(Path::new("apps/ios/devme.toml")).unwrap())
            .unwrap();
        assert!(ios.task.contains_key("ios-build"));
        assert_eq!(ios.task["ios-build"].cwd, None);

        let android =
            devme_config::Stack::parse(plan.file(Path::new("apps/android/devme.toml")).unwrap())
                .unwrap();
        assert!(android.task.contains_key("android-test"));
        assert!(android.resource.contains_key("android-emulator"));

        let resolved = devme_config::ResolvedWorkspace::resolve(dir.path());
        assert!(resolved.is_err(), "dry-run must not write any files");
    }

    #[test]
    fn split_write_preflights_all_destinations_before_writing() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("ios")).unwrap();
        std::fs::write(dir.path().join("ios/App.xcodeproj"), "").unwrap();
        std::fs::write(dir.path().join("ios/devme.toml"), "owned=true\n").unwrap();
        let plan = detect_split(dir.path()).unwrap();

        let error = plan.write(dir.path()).unwrap_err().to_string();
        assert!(error.contains("already exists"));
        assert!(!dir.path().join("devme.toml").exists());
    }
}
