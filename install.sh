#!/bin/sh
# VIKRAM installer — Linux servers and macOS hosts.
#
# What it does, in order:
#   1. detects OS and architecture, refusing anything that is not Linux or macOS
#   2. finds a binary: --bin PATH, --from TARBALL_URL, ./target/release/vikram, ./vikram
#   3. installs it to <prefix>/bin/vikram
#   4. on Linux, as root: creates the `vikram` service user, installs the systemd units and
#      the tmpfiles entry, then reloads systemd (it does NOT enable or start anything)
#
# It never calls sudo itself. When it lacks the privilege for a step it prints the exact
# command to run instead and exits 0, leaving the binary installed.
#
# Re-running is safe: the binary is compared before being replaced, units are compared
# before being rewritten, and user/group/directory creation is conditional.
#
# POSIX sh only (no bashisms). `install` and `cmp` are used; both ship with Debian/Ubuntu/
# Fedora/Arch and with macOS.
#
# Usage: sh ./install.sh [options]      --help for the list.

set -eu

PROG=vikram
PREFIX=${VIKRAM_PREFIX:-/usr/local}
NO_SERVICE=0
DRY_RUN=0
FROM_URL=${VIKRAM_TARBALL_URL:-}
BIN_SRC=""
TMP=""
SRC=""
OS=""
ARCH=""
UNIT_CHANGED=0

say() { printf '%s\n' "$*"; }
step() { printf '\n==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<'EOF'
VIKRAM installer for Linux servers and macOS hosts.

Usage: sh ./install.sh [options]

Options:
  --prefix DIR      Install the binary into DIR/bin. Default /usr/local
                    (env: VIKRAM_PREFIX)
  --bin PATH        Install this binary instead of looking for a build.
  --from URL        Download a .tar.gz release tarball and install the `vikram`
                    binary inside it. {os} and {arch} are substituted, e.g.
                    --from "https://example.net/vikram-{os}-{arch}.tar.gz"
                    (env: VIKRAM_TARBALL_URL)
  --no-service      Install the binary only; do not touch systemd units.
  --dry-run         Print what would happen and change nothing.
  -h, --help        This text.

What it does NOT do:
  * It never runs sudo. Steps that need root are printed for you to run.
  * It does not enable or start any service. It installs units and reloads systemd;
    you choose when to turn a daemon on.
  * It does not install Rust or build VIKRAM. Build first, then run this script.

After installing on Linux:
  sudo systemctl enable --now vikram.service
  sudo journalctl -u vikram.service -f

Headless only: `vikram` with no subcommand starts the TUI, which needs a real terminal
(crates/vikram-cli/src/terminal.rs:66-69) and cannot run under systemd.
EOF
}

# ----------------------------------------------------------------- option parsing

while [ $# -gt 0 ]; do
    case $1 in
        --prefix)
            [ $# -ge 2 ] || die "--prefix needs a directory"
            PREFIX=$2
            shift 2
            ;;
        --prefix=*)
            PREFIX=${1#*=}
            shift
            ;;
        --bin)
            [ $# -ge 2 ] || die "--bin needs a path"
            BIN_SRC=$2
            shift 2
            ;;
        --bin=*)
            BIN_SRC=${1#*=}
            shift
            ;;
        --from)
            [ $# -ge 2 ] || die "--from needs a URL"
            FROM_URL=$2
            shift 2
            ;;
        --from=*)
            FROM_URL=${1#*=}
            shift
            ;;
        --no-service)
            NO_SERVICE=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1 (try --help)"
            ;;
    esac
done

case $PREFIX in
    /*) ;;
    *) die "--prefix must be an absolute path (got: $PREFIX)" ;;
esac
case $PREFIX in
    *'|'*|*'&'*|*'\'*)
        die "refusing a prefix containing | & or backslash: unit paths are rewritten with sed"
        ;;
esac

# Where this script lives, so ./target/release and packaging/systemd can be found no matter
# what the current directory is.
SCRIPT_PATH=$0
case $SCRIPT_PATH in
    */*) SCRIPT_DIR=$(cd "$(dirname "$SCRIPT_PATH")" && pwd) ;;
    *) SCRIPT_DIR=$(pwd) ;;
esac

BINDIR=$PREFIX/bin
VIKRAM_BIN=$BINDIR/vikram

# ----------------------------------------------------------------- platform

step "Detecting platform"

UNAME_S=$(uname -s 2>/dev/null || echo unknown)
case $UNAME_S in
    Linux)
        OS=linux
        ;;
    Darwin)
        OS=darwin
        ;;
    MINGW*|MSYS*|CYGWIN*|Windows_NT|windows*)
        say ""
        die "Windows is not supported by this script.
VIKRAM builds on Windows with MSVC, but it is installed differently there:
PowerShell install steps are in docs/install.md. Come back to this script on a
Linux or macOS host (including WSL, which reports Linux)."
        ;;
    *)
        die "unsupported operating system: $UNAME_S
This script targets Linux (systemd) and macOS. See docs/install.md for other platforms."
        ;;
esac

UNAME_M=$(uname -m 2>/dev/null || echo unknown)
case $UNAME_M in
    x86_64|amd64)
        ARCH=x86_64
        ;;
    aarch64|arm64)
        ARCH=aarch64
        ;;
    armv7l|armv7|armv6l)
        ARCH=armv7
        ;;
    *)
        ARCH=$UNAME_M
        warn "unrecognised architecture '$UNAME_M'; using it verbatim"
        ;;
esac

say "    os:   $OS"
say "    arch: $ARCH"
say "    prefix: $PREFIX"
if [ "$(id -u)" = 0 ]; then
    say "    user: root"
else
    say "    user: $(id -un) uid $(id -u) — privileged steps will be printed, not run"
fi
if [ "$DRY_RUN" = 1 ]; then
    say "    mode: dry-run (nothing will be changed)"
fi

TMP=$(mktemp -d "${TMPDIR:-/tmp}/vikram-install.XXXXXX")
trap 'rm -rf "$TMP"' EXIT HUP INT TERM

# ----------------------------------------------------------------- helpers

# run CMD... — perform a mutating command, or only print it under --dry-run.
run() {
    if [ "$DRY_RUN" = 1 ]; then
        printf '    would run: %s\n' "$*"
    else
        "$@"
    fi
}

# install_file SRC DST MODE — copy only when the content differs, so re-runs are quiet and
# systemd is not reloaded for nothing. Sets UNIT_CHANGED when it actually wrote something.
install_file() {
    if_src=$1
    if_dst=$2
    if_mode=$3

    if [ -f "$if_dst" ] && cmp -s "$if_src" "$if_dst"; then
        say "    unchanged: $if_dst"
        return 0
    fi
    if [ "$DRY_RUN" = 1 ]; then
        printf '    would install: %s -> %s (%s)\n' "$if_src" "$if_dst" "$if_mode"
        UNIT_CHANGED=1
        return 0
    fi
    install -m "$if_mode" "$if_src" "$if_dst"
    say "    installed: $if_dst"
    UNIT_CHANGED=1
}

# ----------------------------------------------------------------- find a binary

step "Locating the vikram binary"

fetch_tarball() {
    fetch_url=$(printf '%s' "$FROM_URL" | sed -e "s|{os}|$OS|g" -e "s|{arch}|$ARCH|g")
    say "    source: $fetch_url"
    case $fetch_url in
        *.tar.gz|*.tgz) ;;
        *) die "--from must point at a .tar.gz or .tgz (got: $fetch_url)" ;;
    esac

    if [ "$DRY_RUN" = 1 ]; then
        printf '    would download: %s\n' "$fetch_url"
        SRC=$TMP/vikram
        printf '    would extract it and install %s\n' "$VIKRAM_BIN"
        return 0
    fi

    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$TMP/vikram.tar.gz" "$fetch_url" ||
            die "download failed: $fetch_url"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$TMP/vikram.tar.gz" "$fetch_url" ||
            die "download failed: $fetch_url"
    else
        die "need curl or wget to download $fetch_url"
    fi

    mkdir -p "$TMP/x"
    tar -xzf "$TMP/vikram.tar.gz" -C "$TMP/x" ||
        die "could not extract $TMP/vikram.tar.gz"
    SRC=$(find "$TMP/x" -type f -name "$PROG" | head -n 1)
    [ -n "$SRC" ] || die "tarball contains no file named '$PROG'"
    chmod +x "$SRC"
    say "    extracted: $SRC"
}

if [ -n "$BIN_SRC" ]; then
    [ -f "$BIN_SRC" ] || die "--bin does not exist: $BIN_SRC"
    SRC=$BIN_SRC
    say "    --bin: $SRC"
elif [ -n "$FROM_URL" ]; then
    fetch_tarball
else
    for candidate in "$SCRIPT_DIR/target/release/$PROG" "$SCRIPT_DIR/$PROG" "$SCRIPT_DIR/bin/$PROG"; do
        if [ -f "$candidate" ]; then
            SRC=$candidate
            say "    found: $SRC"
            break
        fi
    done
    if [ -z "$SRC" ]; then
        die "no $PROG binary to install.

Looked in:
    $SCRIPT_DIR/target/release/$PROG
    $SCRIPT_DIR/$PROG
    $SCRIPT_DIR/bin/$PROG

No prebuilt release archives are published for VIKRAM yet (see docs/install.md), so
choose one:
    build here, then re-run   cargo build --release --locked -p vikram-cli && sh ./install.sh
    install a local binary    sh ./install.sh --bin /path/to/vikram
    install a tarball         sh ./install.sh --from https://host/vikram-{os}-{arch}.tar.gz"
    fi
fi

# ----------------------------------------------------------------- install binary

step "Installing the binary"

run mkdir -p "$BINDIR"

if [ "$DRY_RUN" = 0 ] && [ -f "$VIKRAM_BIN" ] && cmp -s "$SRC" "$VIKRAM_BIN"; then
    say "    already installed and identical: $VIKRAM_BIN"
else
    install_file "$SRC" "$VIKRAM_BIN" 0755
fi

if [ "$DRY_RUN" = 0 ]; then
    "$VIKRAM_BIN" --version || die "$VIKRAM_BIN --version failed — is this the right binary?"
fi

# ----------------------------------------------------------------- systemd units

print_manual_units() {
    say ""
    say "This step needs root. Nothing was changed. Run:"
    say ""
    if [ "$PREFIX" = /usr/local ]; then
        printf '    sudo groupadd --system %s\n' "$PROG"
        printf '    sudo useradd --system --gid %s --home-dir /var/lib/%s --no-create-home \\\n' "$PROG" "$PROG"
        printf '        --shell /usr/sbin/nologin %s\n' "$PROG"
        printf '    sudo install -m 0644 %s/packaging/systemd/vikram.service /etc/systemd/system/vikram.service\n' "$SCRIPT_DIR"
        printf '    sudo install -m 0644 %s/packaging/systemd/vikram-gateway.service /etc/systemd/system/vikram-gateway.service\n' "$SCRIPT_DIR"
        printf '    sudo install -m 0644 %s/packaging/systemd/vikram.tmpfiles /etc/tmpfiles.d/vikram.conf\n' "$SCRIPT_DIR"
        printf '    sudo systemd-tmpfiles --create /etc/tmpfiles.d/vikram.conf\n'
        printf '    sudo systemctl daemon-reload\n'
        printf '    sudo systemctl enable --now vikram.service\n'
    else
        say "    sudo sh $SCRIPT_PATH --prefix $PREFIX"
        say ""
        say "    (re-running as root is required here: the unit files hardcode the binary path"
        say "     and must be rewritten for $VIKRAM_BIN)"
    fi
    say ""
}

if [ "$OS" != linux ]; then
    step "Service installation"
    say "    macOS: this script installs no service."
    say "    Use 'vikram daemon install' as the user that should own the daemon; it writes"
    say "    ~/Library/LaunchAgents/dev.vikram.daemon.plist"
    say "    (crates/vikram-daemon/src/service.rs)."
    say ""
    say "Done. Binary: $VIKRAM_BIN"
    exit 0
fi

if [ "$NO_SERVICE" = 1 ]; then
    step "Service installation"
    say "    --no-service: skipped. Unit files live in $SCRIPT_DIR/packaging/systemd/"
    say ""
    say "Done. Binary: $VIKRAM_BIN"
    exit 0
fi

step "Service user and systemd units"

[ -d "$SCRIPT_DIR/packaging/systemd" ] ||
    die "packaging/systemd not found next to this script ($SCRIPT_DIR/packaging/systemd)"

if [ "$(id -u)" != 0 ]; then
    print_manual_units
    say "Done. Binary: $VIKRAM_BIN"
    exit 0
fi

# Service account. --no-create-home: /var/lib/vikram is created by tmpfiles with the mode
# the units expect, so useradd must not seed it with a home skeleton.
if grep -q "^$PROG:" /etc/group 2>/dev/null; then
    say "    group $PROG: exists"
else
    run groupadd --system "$PROG"
fi

if grep -q "^$PROG:" /etc/passwd 2>/dev/null; then
    say "    user $PROG: exists"
else
    NOLOGIN=$(command -v nologin || echo /usr/sbin/nologin)
    run useradd --system --gid "$PROG" --home-dir "/var/lib/$PROG" \
        --no-create-home --shell "$NOLOGIN" "$PROG"
fi

# Render + install the units. A non-default prefix is substituted into every ExecStart.
render_unit() {
    ru_src=$1
    ru_dst=$2
    if [ "$PREFIX" = /usr/local ]; then
        install_file "$ru_src" "$ru_dst" 0644
    else
        ru_tmp=$TMP/$(basename "$ru_src")
        sed "s|/usr/local/bin/vikram|$VIKRAM_BIN|g" "$ru_src" > "$ru_tmp"
        install_file "$ru_tmp" "$ru_dst" 0644
    fi
}

run mkdir -p /etc/systemd/system /etc/tmpfiles.d
render_unit "$SCRIPT_DIR/packaging/systemd/vikram.service" /etc/systemd/system/vikram.service
render_unit "$SCRIPT_DIR/packaging/systemd/vikram-gateway.service" \
    /etc/systemd/system/vikram-gateway.service
install_file "$SCRIPT_DIR/packaging/systemd/vikram.tmpfiles" /etc/tmpfiles.d/vikram.conf 0644

# Creates /var/lib/vikram and /etc/vikram plus the empty env files if they are missing.
# Safe to repeat: the 'f' entries never truncate an existing env file.
if command -v systemd-tmpfiles >/dev/null 2>&1; then
    run systemd-tmpfiles --create /etc/tmpfiles.d/vikram.conf
else
    warn "systemd-tmpfiles not found; create /var/lib/vikram (0700 vikram:vikram) by hand"
fi

if command -v systemctl >/dev/null 2>&1; then
    if [ "$UNIT_CHANGED" = 1 ]; then
        run systemctl daemon-reload
    else
        say "    units unchanged; daemon-reload skipped"
    fi
else
    warn "systemctl not found (container?); units installed but systemd cannot reload them"
fi

step "Done"
say "    binary:      $VIKRAM_BIN"
say "    state:       /var/lib/vikram   (VIKRAM_HOME)"
say "    config:      /etc/vikram/vikram.env   (provider keys)"
say "    gateway env: /etc/vikram/gateway.env  (GATEWAY_WEBHOOK_BIND, platform tokens)"
say ""
say "    Nothing was enabled or started. When ready:"
say ""
say "      sudo systemctl enable --now vikram.service"
say "      sudo systemctl status vikram.service"
say "      sudo journalctl -u vikram.service -f"
say ""
say "    Add an API key first, or the daemon starts but every turn fails:"
say ""
say "      sudoedit /etc/vikram/vikram.env"
say ""
say "    The TUI is not a service: run 'vikram' on a real terminal instead."
say "    docs/vm.md covers the rest of the server story."
