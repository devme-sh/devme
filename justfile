default:
    @just --list

# Build and install all binaries to ~/.cargo/bin.
build:
    cargo install --path crates/cli
    cargo install --path crates/supervisor
    cargo install --path crates/shared-supervisor

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

# Live TUI dev loop — detached daemon + cargo-watch restart of TUI.
# The TUI runs with --no-shutdown so quitting doesn't kill the daemon;
# cargo-watch can restart it cleanly. Use Ctrl-C on cargo-watch to stop,
# then `cd examples/<name> && devme down` to tear down services.
tui-dev EXAMPLE="smoke":
    @cargo watch --version >/dev/null 2>&1 || cargo install cargo-watch
    cargo build --release -p devme -p devme-supervisor
    cd examples/{{EXAMPLE}} && cargo run --release -p devme -- up -d
    cargo watch \
        -w {{justfile_directory()}}/crates/tui/src \
        -w {{justfile_directory()}}/crates/core/src \
        -s 'cd {{justfile_directory()}}/examples/{{EXAMPLE}} && cargo run -p devme-tui -- --no-shutdown'
