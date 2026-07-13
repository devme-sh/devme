//! Conservative native-monorepo detection. Output is explicit configuration,
//! never a runtime-discovered build graph.

use std::path::Path;

use anyhow::{Result, bail};

pub fn detect(root: &Path) -> Result<String> {
    let files = walk(root, 3);
    let has = |suffix: &str| files.iter().any(|path| path.ends_with(suffix));
    let mut out = String::from("schema_version = 1\n\n[stack]\nname = \"native-monorepo\"\n");
    if has("Package.swift") {
        out.push_str("\n[step.swift-deps]\ncheck = \"swift package show-dependencies >/dev/null\"\nprovision = \"swift package resolve\"\ntrust = \"auto\"\n\n[task.swift-test]\ncmd = \"swift test\"\nsteps = [\"swift-deps\"]\n");
    }
    if files.iter().any(|path| {
        path.extension()
            .is_some_and(|ext| ext == "xcodeproj" || ext == "xcworkspace")
    }) {
        out.push_str("\n[resource.ios-simulator]\nscope = \"host\"\ncapacity = 1\nenv = \"SIMULATOR_SLOT\"\n\n[task.ios-build]\ncmd = \"xcodebuild build\"\nresources = [\"ios-simulator\"]\ntimeout = 1200\n");
    }
    if has("build.gradle.kts") || has("settings.gradle.kts") || has("gradlew") {
        out.push_str("\n[step.gradle-deps]\ncheck = \"test -x ./gradlew\"\nprovision = \"chmod +x ./gradlew\"\ntrust = \"auto\"\n\n[resource.android-emulator]\nscope = \"host\"\ncapacity = 1\nenv = \"EMULATOR_SLOT\"\n\n[task.android-test]\ncmd = \"./gradlew test\"\nsteps = [\"gradle-deps\"]\nresources = [\"android-emulator\"]\ntimeout = 1200\n");
    }
    let package = std::fs::read_to_string(root.join("package.json")).unwrap_or_default();
    let convex = has("convex.json") || has("convex/convex.config.ts") || package.contains("convex");
    let vite = has("vite.config.ts") || has("vite.config.js") || package.contains("vite-plus");
    if convex {
        out.push_str("\n[service.convex]\ncmd = \"bunx convex dev\"\nhealth = { shell = \"bunx convex function-spec >/dev/null\" }\nreadiness = { interval_ms = 1000, timeout_ms = 5000, retries = 60 }\n");
    }
    if vite {
        out.push_str("\n[service.web]\ncmd = \"bun run dev -- --port {port}\"\nport = { base = 5173, slot_offset = 10 }\nhealth = { http = \"http://localhost:{port}\" }\n");
    }
    if out.matches("[task.").count() == 0 && out.matches("[service.").count() == 0 {
        bail!("no supported Xcode, Swift, Gradle/Android, Convex, or Vite+ project markers found");
    }
    Ok(out)
}

fn walk(root: &Path, depth: usize) -> Vec<std::path::PathBuf> {
    if depth == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .is_some_and(|name| name == ".git" || name == "node_modules" || name == "target")
            {
                continue;
            }
            out.push(path.clone());
            if path.is_dir() {
                out.extend(walk(&path, depth - 1));
            }
        }
    }
    out
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
        ] {
            let path = dir.path().join(path);
            if path.extension().is_none() {
                std::fs::create_dir_all(path).unwrap();
            } else {
                std::fs::write(path, "").unwrap();
            }
        }
        let config = detect(dir.path()).unwrap();
        let stack = devme_config::Stack::parse(&config).unwrap();
        devme_config::validate(&stack).unwrap();
        assert!(stack.task.contains_key("ios-build"));
        assert!(stack.task.contains_key("android-test"));
        assert!(stack.service.contains_key("convex"));
        assert!(stack.service.contains_key("web"));
    }
}
