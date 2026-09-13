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
  # A real command, not `sudo -v`: some sudoers configs (e.g. openSUSE's
  # `targetpw` default) make validate-only `-v` prompt for a password even
  # under a matching NOPASSWD rule, while an actual command correctly honors
  # it.
  sudo true || err "sudo authentication failed"
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
  # Sanity-check the result rather than just testing it for emptiness. With no
  # published release the redirect lands on the releases index instead of
  # .../tag/<tag>, so VERSION becomes the literal string "releases" -- non-empty,
  # and it would otherwise sail on into a 404 download URL and a nonsense cosign
  # --certificate-identity.
  case "$VERSION" in
    v[0-9]*) ;;
    *) err "could not resolve the latest release tag (got '$VERSION' from ${latest_url}) -- set CHIMNEY_VERSION to a release tag such as v0.1.0" ;;
  esac
fi
log "Installing chimney-post ${VERSION} (${TARGET})"

# A local WORKDIR (not TMPDIR) so this never shadows the environment
# variable of that name mktemp/tar/etc. themselves consult. Verified non-empty
# and a real directory before the trap is armed, so the trap can never fire
# `rm -rf` against an empty string or something we didn't create ourselves.
WORKDIR=$(mktemp -d) || err "failed to create a temporary working directory"
if [ -z "$WORKDIR" ] || [ ! -d "$WORKDIR" ]; then
  err "mktemp did not return a usable directory"
fi
trap 'if [ -n "${WORKDIR:-}" ] && [ -d "$WORKDIR" ]; then rm -rf -- "$WORKDIR"; fi' EXIT

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
  # The unit hardcodes both /usr/local/bin and /etc/chimney-post; keep each in
  # sync if the binary or the config went somewhere else. Every rewrite is
  # checked afterwards rather than trusted: sed reports success when it matched
  # nothing, and silently installing a unit that still points at the default
  # paths would fail later, at `systemctl start`, with a much worse error.
  if [ "$INSTALL_PREFIX" != "/usr/local/bin" ]; then
    sed -i "s|^ExecStart=/usr/local/bin/chimney-post|ExecStart=${INSTALL_PREFIX}/chimney-post|" \
      "$WORKDIR/chimney-post.service"
    grep -qF "ExecStart=${INSTALL_PREFIX}/chimney-post" "$WORKDIR/chimney-post.service" \
      || err "could not point the unit's ExecStart at ${INSTALL_PREFIX} -- install /etc/systemd/system/chimney-post.service by hand (see README)"
  fi
  if [ "$CONFIG_DIR" != "/etc/chimney-post" ]; then
    # Two spellings, because install.sh is fetched from main but the unit from
    # the release tag: current units pass the config as a systemd credential,
    # units from releases before that set CHIMNEY_CONFIG directly.
    sed -i \
      -e "s|^LoadCredential=config:/etc/chimney-post/config.toml|LoadCredential=config:${CONFIG_DIR}/config.toml|" \
      -e "s|^Environment=CHIMNEY_CONFIG=/etc/chimney-post/config.toml|Environment=CHIMNEY_CONFIG=${CONFIG_DIR}/config.toml|" \
      "$WORKDIR/chimney-post.service"
    if ! grep -qF "LoadCredential=config:${CONFIG_DIR}/config.toml" "$WORKDIR/chimney-post.service" \
       && ! grep -qF "Environment=CHIMNEY_CONFIG=${CONFIG_DIR}/config.toml" "$WORKDIR/chimney-post.service"; then
      err "could not point the unit at ${CONFIG_DIR}/config.toml -- install /etc/systemd/system/chimney-post.service by hand (see README)"
    fi
  fi
  $SUDO install -m 0644 "$WORKDIR/chimney-post.service" /etc/systemd/system/chimney-post.service
  # The unit file is worth installing wherever systemctl exists -- including
  # container images being built to run systemd later -- but the reload is only
  # meaningful when systemd is the running init. WSL without systemd enabled,
  # LXC/chroots and many container images ship the binaries without PID 1, and
  # there `daemon-reload` fails with "System has not been booted with systemd";
  # under `set -e` that aborted the installer after everything was already in
  # place, swallowing the "Next steps" block. /run/systemd/system exists only
  # when systemd really is init, so gate on it -- and warn rather than exit if
  # the reload fails anyway.
  if [ -d /run/systemd/system ]; then
    $SUDO systemctl daemon-reload \
      || warn "systemctl daemon-reload failed -- run it yourself before starting the service"
  else
    warn "systemd is not the running init here -- unit installed, but not reloaded. Run 'systemctl daemon-reload' once you boot with systemd."
  fi
  log "Service unit installed (not started -- fill in ${CONFIG_DIR}/config.toml first, see 'Next steps' below)."
else
  warn "systemctl not found -- skipping systemd unit installation."
fi

# --- Optional MTA setup (msmtp) ---------------------------------------------
# Package names/behavior below are each verified against the real repos (not
# just guessed from memory):
#   - apt (Debian/Ubuntu): msmtp-mta installs the /usr/sbin/sendmail symlink.
#   - dnf/yum (Fedora): msmtp itself ships /usr/bin/sendmail directly.
#   - dnf/yum (RHEL/CentOS/Rocky/Alma, via EPEL): msmtp's postinstall
#     registers itself via `alternatives` as the mta automatically.
#   - zypper (openSUSE): msmtp-mta installs the /usr/sbin/sendmail symlink,
#     same division of labor as Debian.
#   - pacman (Arch): msmtp ships no sendmail-compatible symlink at all (see
#     its own usr/share/doc/msmtp/set_sendmail/ -- upstream expects you to do
#     this yourself), so this creates one.
#   - nix: nixpkgs' msmtp ships its own sendmail wrapper inside the package,
#     just not necessarily somewhere already on root's PATH.
# `s-nail`/`mailx` (the interactive mail-reading/testing client from the
# "Sending Mail from Local Tools" section of the README) is a convenience,
# not required for chimney-post itself, so a failure to install it only
# warns -- it never blocks getting msmtp/sendmail working.
# `sendmail` lives in /usr/sbin, which is not on a non-root user's PATH on
# Debian <=12 or openSUSE (the install-script-mta job below hits the same quirk
# from the other direction). A bare `command -v sendmail` there reports "no MTA"
# on a host that already has one -- and getting this wrong is destructive rather
# than merely noisy: the apt branch installs msmtp-mta, which both Provides and
# Conflicts mail-transport-agent, so apt removes the running postfix/exim to
# make room for it. Check the standard locations explicitly.
have_sendmail() {
  if command -v sendmail >/dev/null 2>&1; then
    return 0
  fi
  local candidate
  for candidate in /usr/sbin/sendmail /sbin/sendmail /usr/lib/sendmail; do
    if [ -x "$candidate" ]; then
      return 0
    fi
  done
  return 1
}

detect_pkg_manager() {
  if command -v apt-get >/dev/null 2>&1; then echo apt
  elif command -v dnf >/dev/null 2>&1; then echo dnf
  elif command -v yum >/dev/null 2>&1; then echo yum
  elif command -v zypper >/dev/null 2>&1; then echo zypper
  elif command -v pacman >/dev/null 2>&1; then echo pacman
  elif command -v nix >/dev/null 2>&1; then echo nix
  else echo none
  fi
}

is_fedora() {
  # Fedora ships msmtp directly and has no epel-release package; RHEL-family
  # (Rocky/Alma/CentOS/RHEL itself) needs EPEL enabled first. Subshell so
  # sourcing os-release doesn't leak its variables into the rest of the script.
  # shellcheck disable=SC1091 # dynamic path, deliberately not followed
  ( . /etc/os-release 2>/dev/null && [ "${ID:-}" = "fedora" ] )
}

setup_msmtp_dnf() {
  local pkg_mgr="$1"
  if ! is_fedora; then
    $SUDO "$pkg_mgr" install -y epel-release \
      || warn "could not enable EPEL -- msmtp install may fail on RHEL-family without it"
  fi
  log "Installing msmtp ($pkg_mgr)..."
  $SUDO "$pkg_mgr" install -y msmtp || err "failed to install msmtp"
  log "Installing s-nail (mailx-compatible, $pkg_mgr)..."
  $SUDO "$pkg_mgr" install -y s-nail \
    || warn "failed to install s-nail (mailx) -- msmtp/sendmail are installed regardless"
}

setup_msmtp_nix() {
  if ! $SUDO sh -c 'command -v nix' >/dev/null 2>&1; then
    warn "nix is available for your user but not for root -- msmtp setup for nix isn't automated in that case. See README for manual steps."
    return 1
  fi
  log "Installing msmtp via nix (best-effort)..."
  if ! $SUDO nix --extra-experimental-features 'nix-command flakes' profile install nixpkgs#msmtp; then
    warn "'nix profile install nixpkgs#msmtp' failed -- skipping msmtp setup. See README for manual steps."
    return 1
  fi
  local sendmail_path candidate
  sendmail_path=$($SUDO sh -c 'command -v sendmail' 2>/dev/null || true)
  if [ -z "$sendmail_path" ]; then
    for candidate in /root/.nix-profile/bin/sendmail /nix/var/nix/profiles/default/bin/sendmail; do
      if $SUDO test -e "$candidate"; then
        sendmail_path="$candidate"
        break
      fi
    done
  fi
  if [ -z "$sendmail_path" ]; then
    warn "installed msmtp via nix but couldn't locate its sendmail wrapper -- see README for manual steps."
    return 1
  fi
  # /usr/local/bin is on PATH essentially everywhere, unlike root's nix
  # profile bin dir, which may not be without extra shell setup.
  $SUDO ln -sf "$sendmail_path" /usr/local/bin/sendmail
}

setup_msmtp() {
  case "$(detect_pkg_manager)" in
    apt)
      log "Installing msmtp, msmtp-mta (apt)..."
      $SUDO apt-get update -qq || err "apt-get update failed"
      $SUDO apt-get install -y msmtp msmtp-mta || err "failed to install msmtp packages"
      # Separate transaction, and only a warning: bsd-mailx conflicts with
      # mailutils, so bundling it in above turned an already-installed mail
      # reader into a hard installer failure.
      log "Installing bsd-mailx (apt)..."
      $SUDO apt-get install -y bsd-mailx \
        || warn "failed to install bsd-mailx (mailx) -- msmtp/sendmail are installed regardless"
      ;;
    dnf) setup_msmtp_dnf dnf ;;
    yum) setup_msmtp_dnf yum ;;
    zypper)
      log "Installing msmtp, msmtp-mta (zypper)..."
      $SUDO zypper --non-interactive install msmtp msmtp-mta \
        || err "failed to install msmtp packages"
      log "Installing mailx (zypper)..."
      $SUDO zypper --non-interactive install mailx \
        || warn "failed to install mailx -- msmtp/sendmail are installed regardless"
      ;;
    pacman)
      log "Installing msmtp (pacman)..."
      # Full -Syu, not just -Sy: partial upgrades (syncing the database
      # without upgrading already-installed packages) are unsupported on
      # Arch and can break the system.
      $SUDO pacman -Syu --noconfirm --needed msmtp \
        || err "failed to install msmtp packages"
      # Plain -S: the database was just synced and the system just upgraded,
      # so this is not a partial upgrade.
      log "Installing s-nail (mailx-compatible, pacman)..."
      $SUDO pacman -S --noconfirm --needed s-nail \
        || warn "failed to install s-nail (mailx) -- msmtp/sendmail are installed regardless"
      if ! have_sendmail; then
        $SUDO ln -sf "$(command -v msmtp)" /usr/local/bin/sendmail
      fi
      ;;
    nix)
      # `return 0`, not a bare `return`: setup_msmtp_nix has already warned and
      # returns 1, and propagating that status makes setup_msmtp fail, which
      # under `set -e` kills the whole installer -- after the binary, config and
      # unit are all in place, and before "Next steps" is printed. A skipped
      # optional MTA is not a failed install.
      setup_msmtp_nix || return 0
      ;;
    none)
      warn "msmtp setup isn't automated for this OS (no apt-get/dnf/yum/zypper/pacman/nix found). See README for manual instructions."
      return 0
      ;;
  esac

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
if have_sendmail; then
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
