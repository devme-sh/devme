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

# Live TUI dev loop — detached daemon + cargo-watch restart of TUI. `devme down` in the example dir when done.
tui-dev EXAMPLE="smoke":
    @cargo watch --version >/dev/null 2>&1 || cargo install cargo-watch
    cd examples/{{EXAMPLE}} && cargo run --release -p devme -- up -d
    cargo watch \
        -w {{justfile_directory()}}/crates/tui/src \
        -w {{justfile_directory()}}/crates/core/src \
        -s 'cd {{justfile_directory()}}/examples/{{EXAMPLE}} && cargo run -p devme-tui'
