default:
    @just --list

# Release-build the three binaries (cli, supervisor, tui).
build:
    cargo build --release -p devme -p devme-supervisor -p devme-tui

# Run the full workspace test suite.
test:
    cargo test --workspace

# Headless TUI smoke — drives the TUI in tmux + asserts the visible grid.
smoke: build
    scripts/tui-smoke.sh

# Symlink the release binary into ~/.local/bin so `devme` resolves anywhere.
link: build
    mkdir -p ~/.local/bin
    ln -sf "$(pwd)/target/release/devme" ~/.local/bin/devme
    @echo "linked → $(readlink ~/.local/bin/devme)"

# Live TUI dev loop — daemon + auto-restart TUI on src change.
tui-dev EXAMPLE="smoke":
    # Start (or reuse) a detached daemon for the example.
    cd examples/{{EXAMPLE}} && cargo run --release -p devme -- up -d
    @echo "rebuilding + restarting TUI on each change…"
    cd examples/{{EXAMPLE}} && cargo watch \
        -w ../../crates/tui/src \
        -w ../../crates/core/src \
        -x "run -p devme-tui"

# Stop the dev daemon started by `tui-dev`.
tui-dev-stop EXAMPLE="smoke":
    cd examples/{{EXAMPLE}} && cargo run --release -p devme -- down

# Watch render tests — fastest feedback loop for pure render tweaks.
render-watch:
    cargo watch -w crates/tui/src -x "test -p devme-tui render::"

# Copy examples/smoke into /tmp so you can poke at it without polluting the repo.
spawn-smoke:
    rm -rf /tmp/devme-smoke
    cp -r examples/smoke /tmp/devme-smoke
    @echo "ready: cd /tmp/devme-smoke && devme"

# Copy the shared-service demo into /tmp — see its README for current limitations.
spawn-shared:
    rm -rf /tmp/devme-shared-test
    cp -r examples/shared /tmp/devme-shared-test
    @echo "ready: see /tmp/devme-shared-test/README.md"

# Tear down any leftover devme socket or supervisor process under /tmp.
clean-tmp:
    -find /tmp -maxdepth 2 -name 'devme*.sock' -print -delete 2>/dev/null
    -pkill -f devme-supervisor 2>/dev/null
    @echo "ok"

# Pre-commit gate — fmt-check, clippy, tests.
check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
