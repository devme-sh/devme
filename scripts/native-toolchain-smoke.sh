#!/usr/bin/env bash
# Exercise workspace task delegation through real xcodebuild and Gradle
# executables. The generated projects are intentionally minimal: Devme owns
# orchestration, while each native tool remains authoritative for its build.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEVME="${1:-$REPO_ROOT/target/release/devme}"
GRADLE="${GRADLE:-$(command -v gradle 2>/dev/null || true)}"

if [[ ! -x "$DEVME" ]]; then
  echo "devme binary not found at $DEVME" >&2
  exit 2
fi
command -v xcodebuild >/dev/null || { echo "xcodebuild required" >&2; exit 2; }
if [[ -z "$GRADLE" || ! -x "$GRADLE" ]]; then
  echo "Gradle required; set GRADLE to an executable Gradle distribution" >&2
  exit 2
fi

ROOT="$(mktemp -d /tmp/devme-native-toolchains.XXXXXX)"
PROJECT="$ROOT/project"
RUNTIME="$ROOT/runtime"
HOME_DIR="$ROOT/home"
trap 'rm -rf "$ROOT"' EXIT

mkdir -p "$PROJECT/apps/ios/Sources/NativeProbe" \
  "$PROJECT/apps/android/src/main/java/devme" "$RUNTIME" "$HOME_DIR"

cat > "$PROJECT/devme.toml" <<'EOF'
schema_version = 1

[workspace.members]
ios = "apps/ios"
android = "apps/android"

[task.check]
depends_on = ["ios::build", "android::build"]
EOF

cat > "$PROJECT/apps/ios/devme.toml" <<'EOF'
schema_version = 1

[task.build]
cmd = "xcodebuild -scheme NativeProbe -destination 'generic/platform=iOS Simulator' -derivedDataPath .derived build CODE_SIGNING_ALLOWED=NO"
timeout = 1200
EOF

cat > "$PROJECT/apps/ios/Package.swift" <<'EOF'
// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "NativeProbe",
    platforms: [.iOS(.v16)],
    products: [.library(name: "NativeProbe", targets: ["NativeProbe"])],
    targets: [.target(name: "NativeProbe")]
)
EOF

cat > "$PROJECT/apps/ios/Sources/NativeProbe/NativeProbe.swift" <<'EOF'
public struct NativeProbe {
    public init() {}
    public let ready = true
}
EOF

cat > "$PROJECT/apps/android/devme.toml" <<'EOF'
schema_version = 1

[task.build]
cmd = "./gradlew --no-daemon classes --project-cache-dir .gradle-project"
timeout = 1200
EOF

cat > "$PROJECT/apps/android/settings.gradle.kts" <<'EOF'
rootProject.name = "native-probe"
EOF

cat > "$PROJECT/apps/android/build.gradle.kts" <<'EOF'
plugins {
    java
}
EOF

cat > "$PROJECT/apps/android/src/main/java/devme/NativeProbe.java" <<'EOF'
package devme;

public final class NativeProbe {
    public static boolean ready() { return true; }
}
EOF

cat > "$PROJECT/apps/android/gradlew" <<EOF
#!/usr/bin/env bash
exec "$GRADLE" "\$@"
EOF
chmod +x "$PROJECT/apps/android/gradlew"

export HOME="$HOME_DIR" XDG_RUNTIME_DIR="$RUNTIME"
(cd "$PROJECT" && "$DEVME" --json config check >/dev/null)
(cd "$PROJECT/apps/ios" && "$DEVME" run build --output json >/dev/null)
(cd "$PROJECT/apps/android" && "$DEVME" run build --output json >/dev/null)
(cd "$PROJECT" && "$DEVME" run check --output json >/dev/null)

test -d "$PROJECT/apps/ios/.derived/Build/Products"
test -f "$PROJECT/apps/android/build/classes/java/main/devme/NativeProbe.class"
echo "real xcodebuild and Gradle workspace smoke passed"
