#!/usr/bin/env bash
# Chimney Post installer.
#
#   curl -fsSL https://raw.githubusercontent.com/lheimbs/chimney-post/main/install.sh | bash
#
# Downloads the latest (or pinned) signed release tarball, verifies it against
# its published checksum and cosign/sigstore signature, installs the binary,
# and optionally sets up the systemd service and an msmtp MTA. Every step is
# safe to re-run: it upgrades the binary and unit file in place but never
# overwrites an existing config.toml or msmtprc.
#
# Run as a regular user -- installing the binary, config, and systemd unit
# needs root, so the script calls `sudo` itself (only for the specific
# commands that need it) and may prompt for your password.
#
# Configuration is via environment variables (all optional):
#   CHIMNEY_VERSION         Release tag to install, e.g. "v0.1.0" (default: latest)
#   CHIMNEY_INSTALL_PREFIX  Directory for the binary (default: /usr/local/bin)
#   CHIMNEY_CONFIG_DIR      Directory for config.toml (default: /etc/chimney-post)
#   CHIMNEY_SKIP_SYSTEMD    1 to skip installing/enabling the systemd unit
#   CHIMNEY_SKIP_VERIFY     1 to skip cosign signature verification (checksum
#                           verification still runs; NOT recommended)
#   CHIMNEY_MTA             "ask" (default), "yes", or "no" -- whether to set
#                           up msmtp as a sendmail-compatible MTA
#   CHIMNEY_NONINTERACTIVE  1 to never prompt (CHIMNEY_MTA=ask then behaves as "no")
set -euo pipefail

REPO="lheimbs/chimney-post"

VERSION="${CHIMNEY_VERSION:-}"
INSTALL_PREFIX="${CHIMNEY_INSTALL_PREFIX:-/usr/local/bin}"
CONFIG_DIR="${CHIMNEY_CONFIG_DIR:-/etc/chimney-post}"
SKIP_SYSTEMD="${CHIMNEY_SKIP_SYSTEMD:-0}"
SKIP_VERIFY="${CHIMNEY_SKIP_VERIFY:-0}"
MTA_MODE="${CHIMNEY_MTA:-ask}"
NONINTERACTIVE="${CHIMNEY_NONINTERACTIVE:-0}"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*" >&2; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || err "required command '$1' not found -- please install it and re-run"
}

case "$MTA_MODE" in
  ask|yes|no) ;;
  *) err "CHIMNEY_MTA must be one of: ask, yes, no (got '$MTA_MODE')" ;;
esac

need_cmd curl
need_cmd tar
need_cmd sha256sum
need_cmd install

# Installing to $INSTALL_PREFIX, $CONFIG_DIR, and /etc/systemd/system needs
# root. Run privileged commands through $SUDO rather than requiring the whole
# script to run as root, so `curl | bash` (no `sudo`) works: only the specific
# commands that need it prompt, via sudo's own /dev/tty password prompt.
if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
else
  need_cmd sudo
  log "Some steps need root -- sudo may prompt for your password."
  sudo -v || err "sudo authentication failed"
  SUDO="sudo"
fi

case "$(uname -s)" in
  Linux) ;;
  *) err "only Linux is supported by the prebuilt binaries; build from source instead (see README)" ;;
esac

case "$(uname -m)" in
  x86_64|amd64)   TARGET="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64)  TARGET="aarch64-unknown-linux-gnu" ;;
  *) err "unsupported architecture '$(uname -m)' -- build from source instead (see README)" ;;
esac

if [ -z "$VERSION" ]; then
  log "Resolving latest release..."
  # Following the /releases/latest redirect avoids the GitHub API's stricter
  # rate limit and needs no JSON parsing: it 302s straight to .../tag/<tag>.
  latest_url=$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest")
  VERSION="${latest_url##*/}"
  [ -n "$VERSION" ] || err "could not resolve the latest release tag"
fi
log "Installing chimney-post ${VERSION} (${TARGET})"

# A local WORKDIR (not TMPDIR) so this never shadows the environment
# variable of that name mktemp/tar/etc. themselves consult. Verified non-empty
# and a real directory before the trap is armed, so the trap can never fire
# `rm -rf` against an empty string or something we didn't create ourselves.
WORKDIR=$(mktemp -d) || err "failed to create a temporary working directory"
[ -n "$WORKDIR" ] && [ -d "$WORKDIR" ] || err "mktemp did not return a usable directory"
trap '[ -n "${WORKDIR:-}" ] && [ -d "$WORKDIR" ] && rm -rf -- "$WORKDIR"' EXIT

DL_BASE="https://github.com/${REPO}/releases/download/${VERSION}"
TARBALL="chimney-post-${VERSION}-${TARGET}.tar.gz"

fetch() {
  # $1 = URL, $2 = destination filename in $WORKDIR
  curl -fsSL "$1" -o "$WORKDIR/$2" || err "failed to download $1"
}

log "Downloading release assets..."
fetch "$DL_BASE/$TARBALL" "$TARBALL"
fetch "$DL_BASE/$TARBALL.bundle" "$TARBALL.bundle"
fetch "$DL_BASE/SHA256SUMS" "SHA256SUMS"

log "Verifying checksum..."
( cd "$WORKDIR" && sha256sum -c SHA256SUMS --ignore-missing ) \
  || err "checksum verification failed -- downloaded file does not match SHA256SUMS"

if [ "$SKIP_VERIFY" = "1" ]; then
  warn "CHIMNEY_SKIP_VERIFY=1 -- skipping cosign signature verification. Only the checksum was checked, which proves the download wasn't corrupted, NOT that it came from the real release workflow."
elif command -v cosign >/dev/null 2>&1; then
  log "Verifying cosign signature..."
  cosign verify-blob "$WORKDIR/$TARBALL" \
    --bundle "$WORKDIR/$TARBALL.bundle" \
    --certificate-identity "https://github.com/${REPO}/.github/workflows/release.yml@refs/tags/${VERSION}" \
    --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
    --certificate-github-workflow-repository "${REPO}" \
    || err "cosign signature verification failed -- refusing to install a tarball that doesn't verify against ${REPO}'s release workflow"
else
  err "cosign is required to verify the release signature but was not found. Install it from https://docs.sigstore.dev/system_config/installation/ and re-run, or set CHIMNEY_SKIP_VERIFY=1 to install with checksum-only verification (not recommended)."
fi

log "Installing binary to ${INSTALL_PREFIX}/chimney-post..."
tar -xzf "$WORKDIR/$TARBALL" -C "$WORKDIR" chimney-post
$SUDO install -d -m 0755 "$INSTALL_PREFIX"
$SUDO install -m 0755 -o root -g root "$WORKDIR/chimney-post" "$INSTALL_PREFIX/chimney-post"

RAW_BASE="https://raw.githubusercontent.com/${REPO}/${VERSION}"

# $CONFIG_DIR is root-owned, so an existing config.toml is only checked
# for/written to via $SUDO -- a plain `[ -e ]` as a non-root user would still
# work (0755 dirs are traversable/readable by anyone), but $SUDO keeps this
# correct even if the directory's permissions are tightened. Config.toml is
# needed regardless of init system, so this runs even under
# CHIMNEY_SKIP_SYSTEMD=1 -- that flag only controls the systemd unit below.
$SUDO install -d -m 0755 "$CONFIG_DIR"

if $SUDO test -e "$CONFIG_DIR/config.toml"; then
  log "Leaving existing $CONFIG_DIR/config.toml in place."
else
  log "Writing template config to ${CONFIG_DIR}/config.toml..."
  fetch_config_url="$RAW_BASE/config.example.toml"
  curl -fsSL "$fetch_config_url" -o "$WORKDIR/config.toml" \
    || err "failed to download $fetch_config_url"
  $SUDO install -m 0600 "$WORKDIR/config.toml" "$CONFIG_DIR/config.toml"
fi

if [ "$SKIP_SYSTEMD" = "1" ]; then
  log "CHIMNEY_SKIP_SYSTEMD=1 -- skipping systemd unit installation."
elif command -v systemctl >/dev/null 2>&1; then
  log "Installing systemd unit..."
  curl -fsSL "$RAW_BASE/systemd/chimney-post.service" -o "$WORKDIR/chimney-post.service" \
    || err "failed to download $RAW_BASE/systemd/chimney-post.service"
  # The unit hardcodes /usr/local/bin; keep it in sync if the binary was
  # installed somewhere else.
  if [ "$INSTALL_PREFIX" != "/usr/local/bin" ]; then
    sed -i "s|^ExecStart=/usr/local/bin/chimney-post|ExecStart=${INSTALL_PREFIX}/chimney-post|" \
      "$WORKDIR/chimney-post.service"
  fi
  $SUDO install -m 0644 "$WORKDIR/chimney-post.service" /etc/systemd/system/chimney-post.service
  $SUDO systemctl daemon-reload
  log "Service unit installed (not started -- fill in ${CONFIG_DIR}/config.toml first, see 'Next steps' below)."
else
  warn "systemctl not found -- skipping systemd unit installation."
fi

# --- Optional MTA setup (msmtp) ---------------------------------------------
setup_msmtp() {
  if ! command -v apt-get >/dev/null 2>&1; then
    warn "msmtp setup is currently only automated for Debian/Ubuntu (apt-get not found). See README for manual instructions for your distro."
    return
  fi
  log "Installing msmtp, msmtp-mta, bsd-mailx..."
  $SUDO apt-get update -qq || err "apt-get update failed"
  $SUDO apt-get install -y msmtp msmtp-mta bsd-mailx || err "failed to install msmtp packages"

  if $SUDO test -e /etc/msmtprc; then
    log "Leaving existing /etc/msmtprc in place."
    return
  fi

  # Matches the smtp.bind port in config.example.toml (2525); adjust
  # /etc/msmtprc yourself if you changed [smtp].bind.
  log "Writing /etc/msmtprc..."
  cat > "$WORKDIR/msmtprc" <<'EOF'
defaults
auth   off
tls    off
syslog on

account chimney
host    127.0.0.1
port    2525
from    %U@%H

account default : chimney
EOF
  $SUDO install -m 0644 "$WORKDIR/msmtprc" /etc/msmtprc
}

mta_detected=0
if command -v sendmail >/dev/null 2>&1; then
  mta_detected=1
fi

case "$MTA_MODE" in
  no)
    : # explicitly opted out
    ;;
  yes)
    setup_msmtp
    ;;
  ask)
    if [ "$mta_detected" = "1" ]; then
      log "An MTA (sendmail-compatible) is already installed -- skipping msmtp setup."
    elif [ "$NONINTERACTIVE" = "1" ]; then
      log "No MTA detected. CHIMNEY_NONINTERACTIVE=1 -- skipping msmtp setup (set CHIMNEY_MTA=yes to install it automatically)."
    # stdin is the piped script when run as `curl | bash`, so prompt on the
    # controlling terminal directly rather than reading stdin. /dev/tty can
    # exist as a device node with no controlling terminal behind it (e.g. in
    # CI) -- opening it then fails with ENXIO -- so the open+read is the
    # condition itself rather than a separate existence check beforehand.
    elif { printf 'No sendmail-compatible MTA detected. Install msmtp as one now? [y/N] ' > /dev/tty \
        && read -r reply < /dev/tty; } 2>/dev/null; then
      case "$reply" in
        [yY]|[yY][eE][sS]) setup_msmtp ;;
        *) log "Skipping msmtp setup." ;;
      esac
    else
      log "No MTA detected. Not running interactively -- skipping msmtp setup (set CHIMNEY_MTA=yes to install it automatically)."
    fi
    ;;
esac

log "Done."
cat >&2 <<EOF

Next steps:
  1. Edit ${CONFIG_DIR}/config.toml with your homeserver, user_id, and room_id.
  2. Provide credentials, e.g.:
       sudo systemctl edit chimney-post
     and add:
       [Service]
       Environment=MATRIX_PASSWORD=your-secret
  3. Start the service:
       sudo systemctl enable --now chimney-post
EOF
