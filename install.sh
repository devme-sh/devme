#!/bin/sh
# devme installer — curl -fsSL https://devme.sh/install | sh
#
# Detects OS/arch, downloads the right binary from GitHub Releases,
# and drops it into ~/.local/bin (or BIN_DIR if set).
#
# Environment variables:
#   DEVME_VERSION  - specific version to install (e.g. "0.2.0"), default: latest
#   BIN_DIR        - override install directory, default: ~/.local/bin

set -eu

REPO="devme-sh/devme"
GITHUB_URL="https://github.com/${REPO}"

# --- helpers ----------------------------------------------------------------

info() {
    printf '\033[1;34m%s\033[0m %s\n' ">>" "$*"
}

warn() {
    printf '\033[1;33m%s\033[0m %s\n' "warn:" "$*" >&2
}

error() {
    printf '\033[1;31m%s\033[0m %s\n' "error:" "$*" >&2
    exit 1
}

need() {
    command -v "$1" > /dev/null 2>&1 || error "need '$1' (command not found)"
}

# --- detect OS & arch -------------------------------------------------------

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)  os="unknown-linux-gnu" ;;
        Darwin) os="apple-darwin"      ;;
        *)      error "unsupported OS: $os" ;;
    esac

    case "$arch" in
        x86_64|amd64)             arch="x86_64"  ;;
        aarch64|arm64)            arch="aarch64"  ;;
        *)                        error "unsupported architecture: $arch" ;;
    esac

    # macOS: detect if running under Rosetta 2
    if [ "$os" = "apple-darwin" ] && [ "$arch" = "x86_64" ]; then
        if sysctl -n sysctl.proc_translated 2>/dev/null | grep -q 1; then
            arch="aarch64"
            info "detected Rosetta 2 — using native arm64 binary"
        fi
    fi

    TARGET="${arch}-${os}"
}

# --- resolve version --------------------------------------------------------

resolve_version() {
    if [ -n "${DEVME_VERSION:-}" ]; then
        VERSION="$DEVME_VERSION"
    else
        need curl
        VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
            | grep '"tag_name"' \
            | sed -E 's/.*"v?([^"]+)".*/\1/')"
        [ -n "$VERSION" ] || error "could not determine latest version"
    fi
}

# --- download & install -----------------------------------------------------

download_and_install() {
    archive="devme-${TARGET}.tar.gz"
    url="${GITHUB_URL}/releases/download/v${VERSION}/${archive}"

    bin_dir="${BIN_DIR:-$HOME/.local/bin}"
    mkdir -p "$bin_dir"

    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' EXIT

    info "downloading devme v${VERSION} for ${TARGET}"
    info "  ${url}"

    need curl
    need tar

    if ! curl -fSL --progress-bar -o "${tmpdir}/${archive}" "$url"; then
        error "download failed — check that v${VERSION} has a release for ${TARGET}"
    fi

    tar xzf "${tmpdir}/${archive}" -C "$tmpdir"

    # The archive contains binaries at the top level
    for bin in devme devme-tui devme-supervisor devme-shared-supervisor; do
        if [ -f "${tmpdir}/${bin}" ]; then
            install -m 755 "${tmpdir}/${bin}" "${bin_dir}/${bin}"
        fi
    done

    info "installed to ${bin_dir}/devme"
}

# --- PATH advice ------------------------------------------------------------

ensure_path() {
    bin_dir="${BIN_DIR:-$HOME/.local/bin}"

    case ":${PATH}:" in
        *":${bin_dir}:"*) return ;;
    esac

    warn "${bin_dir} is not in your PATH"

    shell_name="$(basename "${SHELL:-/bin/sh}")"
    case "$shell_name" in
        fish)
            rc="$HOME/.config/fish/config.fish"
            line="fish_add_path ${bin_dir}"
            ;;
        zsh)
            rc="$HOME/.zshrc"
            line="export PATH=\"${bin_dir}:\$PATH\""
            ;;
        bash)
            if [ -f "$HOME/.bash_profile" ]; then
                rc="$HOME/.bash_profile"
            else
                rc="$HOME/.bashrc"
            fi
            line="export PATH=\"${bin_dir}:\$PATH\""
            ;;
        *)
            printf '  add %s to your PATH\n' "$bin_dir"
            return
            ;;
    esac

    info "to add it, run:"
    printf '\n  echo '\''%s'\'' >> %s && source %s\n\n' "$line" "$rc" "$rc"
}

# --- main -------------------------------------------------------------------

main() {
    info "devme installer"
    echo

    detect_target
    resolve_version
    download_and_install
    ensure_path

    echo
    info "done! run 'devme --help' to get started"
}

main
