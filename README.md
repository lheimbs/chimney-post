<p align="center">
  <img src="chimney-post_banner.png" alt="Chimney Post -- emails enter a chimney and emerge as encrypted Matrix messages" width="800">
</p>

<h1 align="center">Chimney Post</h1>

<p align="center">
  A local-only SMTP server that forwards incoming email to Matrix with end-to-end encryption.
</p>

<p align="center">
  <a href="https://github.com/lheimbs/chimney-post/actions/workflows/ci.yml"><img src="https://github.com/lheimbs/chimney-post/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/lheimbs/chimney-post/releases"><img src="https://img.shields.io/github/v/release/lheimbs/chimney-post" alt="Latest Release"></a>
  <a href="https://github.com/lheimbs/chimney-post/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/rust-1.88%2B-orange" alt="Minimum Rust version: 1.88">
</p>

---

**Warning**: This project was created with AI assistance and is in an early prototype stage. Use at your own risk.

---

## About

Chimney Post sits on your local machine, accepts emails over SMTP, and delivers them as end-to-end encrypted messages to a Matrix room. It is designed for forwarding automated notifications on your server - be it nextcloud, rkhunter for rootkit hunting or apticron for automated upgrades - into one encrypted Matrix chat.

The SMTP server binds to `127.0.0.1` by default, so it accepts no connections from the network. (The one exception is running it [as a container](#running-with-docker), where it must bind `0.0.0.0` *inside* the container and the container boundary limits reachability instead.) Matrix messages are encrypted by default using the `matrix-sdk` E2EE implementation, and the encryption store is persisted locally in SQLite so device keys survive restarts.

This is essentially a super narrow version of [mailrise](https://github.com/YoRyan/mailrise) but intended for a single server and its services and only forwarding to matrix.

## Features

- **Local SMTP server** -- Listens on localhost only; never exposed to the network.
- **End-to-end encrypted Matrix delivery** -- All messages are sent through E2EE. Optionally enforce that the target room is encrypted before sending.
- **Password or access-token authentication** -- Connect to any Matrix homeserver with either method.
- **Configurable message templates** -- Format forwarded emails with MiniJinja templates (subject, body, sender, recipient are all available as variables).
- **Durable, persistent queue** -- Accepted emails are written to a local SQLite outbox *before* the SMTP `250 OK` and only removed once delivered, so messages survive process restarts and crashes. Delivery is *at-least-once*: each send carries a stable Matrix transaction id so that an in-session retry after a lost response is deduplicated by the homeserver; a crash *between* a successful send and the queue removal will, however, redeliver on the next start as a genuine duplicate (the restart is a new session, so the transaction id no longer dedupes it). The outbox is size-bounded (`queue.max_len`); when full, new mail is refused with a temporary `451` so senders retry.
- **Retry with exponential backoff** -- Failed Matrix deliveries are retried automatically (configurable attempts and interval). A single failing message backs off independently and never blocks delivery of the rest of the queue. A message that exhausts all retries is moved to a `dead_letter` table rather than dropped.
- **Resilient startup** -- The SMTP server starts accepting and queuing mail immediately; the Matrix connection is established in the background and retried, so a homeserver outage at boot neither refuses incoming mail nor crash-loops the service. Queued mail is delivered once the connection is up.
- **Async, low-resource** -- Built on Tokio; idles at minimal CPU and memory.
- **Structured logging** -- JSON or human-readable log output via `tracing`.
- **systemd-ready** -- Ships with a hardened service unit file.
- **Graceful shutdown** -- Handles SIGINT and SIGTERM cleanly.

## How It Works

```txt
 Service / Script                Chimney Post                 Matrix Homeserver
 ──────────────────     ─────────────────────────────     ────────────────────────
  sends email  ───────> SMTP server (127.0.0.1:2525)
                        parses headers & body
                        persists to SQLite outbox ──────> Matrix client sends
                        (then acks 250 OK)                E2EE message to room
                        on success: delete from outbox
                        on failure: reschedule w/ backoff
```

1. An application connects to the SMTP server on localhost and sends an email (standard SMTP commands: EHLO, MAIL FROM, RCPT TO, DATA).
2. Chimney Post parses the email headers (From, To, Subject) and body.
3. The message is written to a persistent SQLite outbox. Only after it is durably stored does the server reply `250 OK` (a storage failure yields `451` so the sender retries).
4. A background worker picks up the earliest *due* message, formats it through the configured MiniJinja template, and sends it as an encrypted Matrix message. On success the message is deleted from the outbox.
5. If delivery fails, the message is rescheduled with exponential backoff up to the configured retry limit; the worker moves on to the next due message so one failure never blocks the queue. Messages still pending are retained across restarts and retried on the next start. A message that exhausts all retries is moved to a `dead_letter` table in the same SQLite database (with its last error) rather than silently dropped, so a permanently undeliverable alert can be inspected and recovered.

## Getting Started

### Prerequisites

- Rust 1.88 or later (install via [rustup](https://rustup.rs/))
- A Matrix account for the bot
- A Matrix room where the bot should post (invite the bot user to the room)

### Quick Install (Linux)

The install script downloads the latest signed release for your architecture, verifies its checksum and cosign/sigstore signature, installs the binary, writes a template `config.toml`, and installs the systemd unit (see [Running as a systemd Service](#running-as-a-systemd-service)). It also offers to set up `msmtp` (see [Sending Mail from Local Tools](#sending-mail-from-local-tools-mailx-cron-apticron-)) if no MTA is detected.

Run it as a regular user, not with `sudo`: the script calls `sudo` itself for the specific steps that need root (installing the binary, config, and systemd unit) and will prompt for your password when it gets there.

Review [`install.sh`](install.sh) before running it, as with any script piped into a shell:

```bash
curl -fsSL https://raw.githubusercontent.com/lheimbs/chimney-post/main/install.sh | bash
```

Requires [cosign](https://docs.sigstore.dev/system_config/installation/) v3+ to be installed for signature verification (recommended -- see [Pre-built Binaries](#pre-built-binaries) below for why). The script is configurable via environment variables; see the comment header of `install.sh` for the full list, e.g.:

```bash
# Install a specific version, skip the systemd unit, and skip the MTA prompt
curl -fsSL https://raw.githubusercontent.com/lheimbs/chimney-post/main/install.sh \
  | CHIMNEY_VERSION=v0.1.0 CHIMNEY_SKIP_SYSTEMD=1 CHIMNEY_MTA=no bash
```

It never overwrites an existing `config.toml` or `/etc/msmtprc`, and is safe to re-run to upgrade the binary and unit file in place.

On Arch, accepting the `msmtp` offer runs a full `pacman -Syu`, which upgrades every package on the system — partial upgrades are unsupported there. Set `CHIMNEY_MTA=no` if you would rather install it yourself.

### Pre-built Binaries

Release binaries for `x86_64` and `aarch64` Linux are published on the [Releases page](https://github.com/lheimbs/chimney-post/releases). This section documents the manual steps that `install.sh` above automates, useful if you want to inspect each step yourself.
Each release tarball is signed with a cosign keyless signature (sigstore) and carries SLSA build provenance attested via GitHub Actions OIDC.

Binaries are built on Ubuntu 24.04 and dynamically link glibc 2.39, so they run on Ubuntu 24.04+, Debian 13+, and anything else with glibc 2.39 or newer. On older distributions — including Debian 12 (glibc 2.36) and Ubuntu 22.04 (2.35) — build from source instead.

Each tarball is built reproducibly: member order, timestamps and ownership are normalised, so rebuilding the same commit yields a byte-identical archive and therefore the same digest as the one that was signed and attested.

In the commands below, replace `<version>` with the release tag (e.g. `v0.1.0`) and `<target>` with `x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu`.

Download a tarball and its `.bundle` sidecar, then verify the signature. This needs **cosign v3.0 or newer** — the `.bundle` files are standard Sigstore bundles, which cosign v2.6.x can only read if you add `--new-bundle-format`, and cosign v2.5 and older cannot read at all:

```sh
cosign verify-blob chimney-post-<version>-<target>.tar.gz \
  --bundle chimney-post-<version>-<target>.tar.gz.bundle \
  --certificate-identity "https://github.com/lheimbs/chimney-post/.github/workflows/release.yml@refs/tags/<version>" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  --certificate-github-workflow-repository "lheimbs/chimney-post"
```

The `--certificate-identity` value pins the exact repository, workflow file, and tag that produced the signature. If you script this across releases and need a pattern, anchor it — `--certificate-identity-regexp` is matched unanchored, so an unanchored pattern also accepts signatures from other workflows and other similarly-named repositories:

```sh
--certificate-identity-regexp '^https://github\.com/lheimbs/chimney-post/\.github/workflows/release\.yml@refs/tags/v[0-9]+\.[0-9]+\.[0-9]+$'
```

To verify build provenance (requires the [GitHub CLI](https://cli.github.com/)):

```sh
gh attestation verify chimney-post-<version>-<target>.tar.gz \
  --repo lheimbs/chimney-post \
  --signer-workflow lheimbs/chimney-post/.github/workflows/release.yml
```

Checksums for all artifacts are in `SHA256SUMS`, which is signed the same way. Verifying it once is the cheaper path if you downloaded several assets:

```sh
cosign verify-blob SHA256SUMS \
  --bundle SHA256SUMS.bundle \
  --certificate-identity "https://github.com/lheimbs/chimney-post/.github/workflows/release.yml@refs/tags/<version>" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  --certificate-github-workflow-repository "lheimbs/chimney-post"

sha256sum -c SHA256SUMS --ignore-missing
```

### Build

```bash
git clone https://github.com/lheimbs/chimney-post.git
cd chimney-post
cargo build --release
```

The binary is written to `target/release/chimney-post`.

### Configure

Copy the example configuration and edit it:

```bash
cp config.example.toml config.toml
```

At minimum, fill in the `[matrix]` section with your homeserver URL, bot user ID, and target room ID. Provide credentials through environment variables:

```bash
# Password-based authentication
export MATRIX_PASSWORD="your-matrix-password"

# -- or -- access-token authentication
export MATRIX_ACCESS_TOKEN="syt_..."
export MATRIX_DEVICE_ID="ABCDEFGHIJ"
```

The config file references these variables with `${MATRIX_PASSWORD}` syntax; Chimney Post substitutes them at startup.

### Run

```bash
# Uses config.toml in the current directory by default
cargo run --release

# Or point to a specific config file
CHIMNEY_CONFIG=/etc/chimney-post/config.toml ./target/release/chimney-post
```

## Configuration Reference

All settings live in a single TOML file. See `config.example.toml` for a fully annotated copy.

### `[smtp]`

| Key                   | Default          | Description                                                                          |
|-----------------------|------------------|--------------------------------------------------------------------------------------|
| `bind`                | `127.0.0.1:2525` | Address and port the SMTP server listens on.                                         |
| `max_message_size`    | `10485760`       | Maximum email size in bytes (10 MB).                                                  |
| `timeout`             | `30`             | Per-read timeout in seconds (wait for the next line).                                 |
| `max_connections`     | `100`            | Maximum simultaneous connections; excess are rejected with `421`.                    |
| `max_session_seconds` | `300`            | Maximum lifetime of one SMTP session, bounding slow/stuck connections.               |

### `[matrix]`

| Key                | Default          | Description                                        |
|--------------------|------------------|----------------------------------------------------|
| `homeserver`       | --               | Matrix homeserver URL (e.g. `https://matrix.org`). |
| `user_id`          | --               | Full Matrix user ID (`@user:server.com`).          |
| `device_name`      | `chimney-post`   | Display name for the Matrix device.                |
| `room_id`          | --               | Default/catch-all room ID (`!room:server.com`). Used for emails that match no routing rule. |
| `store_path`       | --               | Directory for the E2EE key store (SQLite).         |
| `require_e2ee`     | `true`           | Refuse to send if the room is not encrypted.       |
| `message_template` | *(built-in)*     | MiniJinja template for formatting messages.        |
| `routes`           | *(none)*         | Optional room-routing rules; see [Room routing](#room-routing). |

TLS to `homeserver` is handled by [rustls](https://github.com/rustls/rustls),
not OpenSSL. Both the bundled Mozilla root set and your OS's trust store are
checked, so a homeserver with a certificate from a private/corporate CA that's
trusted by the machine's OS will work. Two things rustls is stricter about
than OpenSSL and won't accept from *either* store: TLS 1.0/1.1, and
certificates that carry only a CN with no SAN.

### Room routing

By default every email is delivered to `room_id`. To fan notifications out to
different rooms, add `[[matrix.routes]]` rules that select on the email's
recipient (`to`, matched against any SMTP `RCPT TO`) and/or sender (`from`,
matched against the SMTP `MAIL FROM`) -- similar to how
[mailrise](https://github.com/YoRyan/mailrise) routes on address, but targeting
Matrix rooms:

```toml
[matrix]
room_id = "!fallback:example.com"   # catch-all for unmatched mail

[[matrix.routes]]
to = "alerts@chimney"               # route by recipient
room_id = "!alerts:example.com"

[[matrix.routes]]
from = "nextcloud@server.example.com"   # route by sender
room_id = "!nextcloud:example.com"

[[matrix.routes]]
to = "ops@chimney"                  # both set => both must match
from = "root@server.example.com"
room_id = "!ops:example.com"
```

Rules are evaluated top to bottom and the **first match wins**. Each rule must
set `room_id` and at least one of `to`/`from`; when both are set, both must
match (logical AND). Matching is case-insensitive. Any email that matches no
rule falls back to `room_id`, so mail is never dropped for lack of a route.

### `[matrix.credentials]`

Provide **either** the password **or** the access_token + device_id pair:

| Key             | Description                                                                          |
|-----------------|-------------------------------------------------------------------------------------|
| `password`      | Matrix password (use `${MATRIX_PASSWORD}`).                                          |
| `access_token`  | Matrix access token (use `${MATRIX_ACCESS_TOKEN}`).                                  |
| `device_id`     | Required with `access_token`; **strongly recommended with `password`** (see below).  |

> **Pin `device_id` for password auth.** A password login with no `device_id`
> makes the homeserver mint a **new Matrix device on every start**. Because the
> local E2EE crypto store is bound to a single device, the next restart
> mismatches it — orphaning devices on your account and forcing a crypto-store
> reset each time. Set a stable `device_id` (any string, e.g. `chimney-post`) so
> every login reuses the same device. (Switching to `access_token` + `device_id`
> avoids re-logging in entirely and is the preferred setup for an unattended
> bot — see `docs/session-persistence.md` for a fully self-managing alternative.)

### `[logging]`

| Key      | Default | Description                                               |
|----------|---------|-----------------------------------------------------------|
| `level`  | `info`  | Log verbosity: `trace`, `debug`, `info`, `warn`, `error`. |
| `format` | `json`  | Output format: `json` or `pretty`.                        |

### `[queue]`

| Key             | Default                          | Description                                                              |
|-----------------|----------------------------------|--------------------------------------------------------------------------|
| `max_retries`   | `5`                              | Maximum retries after the initial attempt before a message is moved to the dead-letter table. |
| `retry_backoff` | `60`                             | Base backoff interval in seconds (doubles each retry, capped at 900s).   |
| `db_path`       | `/var/lib/chimney-post/queue.db` | Path to the persistent SQLite outbox; must be writable by the service.   |
| `max_len`       | `10000`                          | Max queued messages before new mail is refused with `451`. `0` = unlimited. |

## Message Templates

Chimney Post formats each email using a [MiniJinja](https://docs.rs/minijinja) template before sending it to Matrix. Four variables are available: `from`, `to`, `subject`, and `body` (all strings).

The built-in default template renders a full email view:

```jinja
{%- if from %}From: {{ from }}
{% endif -%}
{%- if to %}To: {{ to }}
{% endif -%}
{%- if subject %}Subject: {{ subject }}{% else %}Subject: (none){% endif %}

{%- if body and body is string and body | trim %}
{{ body }}
{%- else %}
(empty message body)
{%- endif %}
```

Override it in `config.toml` to use a custom format:

```toml
[matrix]
message_template = "[{{ subject }}] {{ body }}"
```

## Running as a systemd Service

A service unit file is included at `systemd/chimney-post.service`. To install:

```bash
# Copy the binary
sudo cp target/release/chimney-post /usr/local/bin/

# Create the config directory and copy your config
sudo mkdir -p /etc/chimney-post
sudo cp config.toml /etc/chimney-post/config.toml
sudo chmod 600 /etc/chimney-post/config.toml

# Install the service unit
sudo cp systemd/chimney-post.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now chimney-post
```

The unit passes `config.toml` to the service with `LoadCredential=` rather than
reading it from `/etc` directly. `DynamicUser=yes` means the process runs under a
transient non-root UID that cannot open a `0600` root-owned file, so systemd reads
it as root at start and hands the service a private read-only copy under `%d`
(`/run/credentials/chimney-post.service`). That is why `chmod 600` above is both
safe and sufficient. If you move `config.toml` elsewhere, update the
`LoadCredential=` path in the unit, not `CHIMNEY_CONFIG`.

The service unit runs with `DynamicUser=yes` and an extensive sandbox: read-only root, private `/tmp` and `/dev`, dropped capabilities, kernel/`/proc` protections, a `@system-service` syscall allow-list, and a restricted set of address families (`systemd-analyze security` rates it ~1.3 "OK"). State data (the E2EE key store and the SQLite queue) is kept under `/var/lib/chimney-post`.

The unit uses `Type=notify`: the service reports **ready** as soon as the SMTP listener is up (so `systemctl start` doesn't block on Matrix), publishes a live `STATUS=` line you can see with `systemctl status` (Matrix connection state + queue/dead-letter depth), and pings the systemd watchdog (`WatchdogSec=120`) so a hung runtime is auto-restarted. It also logs a `warn` when it is queuing mail without a Matrix connection and when messages reach the dead-letter table, so a silent delivery outage is visible in `journalctl`. To be alerted, wire those to your monitoring, e.g. a journald match or an `OnFailure=` mailer unit.

Two directives are the most likely to need adjustment for your host and are commented as such in the unit: `MemoryDenyWriteExecute=yes` and `PrivateUsers=yes` — if the service fails to start, remove these first and check `journalctl -u chimney-post` for "Operation not permitted". If you bind to a port below 1024, you must also grant `CAP_NET_BIND_SERVICE` (see the comment in the unit). `IPAddressDeny` is intentionally not set because the service needs egress to an arbitrary homeserver IP and ingress from your LAN — restrict the SMTP port with a host firewall instead.

Set secrets in a systemd environment file or drop-in override:

```bash
sudo systemctl edit chimney-post
```

```ini
[Service]
Environment=MATRIX_PASSWORD=your-secret
```

## Running with Docker

A multi-arch (amd64/arm64) image is published to GHCR.

**Set `bind = "0.0.0.0:2525"` in the config you mount.** This is the one setting
that must differ from the default `config.toml`: inside a container, `127.0.0.1`
is the *container's own* loopback, which neither a published port nor another
container can ever reach. Leaving the default produces a confusing failure rather
than a clean one -- the connection is accepted by Docker's port forwarder and then
closed with no data, so senders report "connection reset" or "server does not
speak SMTP", and the image has no shell to debug from. The container boundary, not
the bind address, is what limits reachability here; the `-p` flag below is what
keeps it on loopback.

```bash
docker run -d --name chimney-post \
  -v "$PWD/config.toml:/etc/chimney-post/config.toml:ro" \
  -v chimney-post-data:/var/lib/chimney-post \
  -e MATRIX_PASSWORD=your-secret \
  -p 127.0.0.1:2525:2525 \
  ghcr.io/lheimbs/chimney-post:latest
```

- `config.toml` is bind-mounted read-only; `CHIMNEY_CONFIG` already points at
  `/etc/chimney-post/config.toml` in the image, so no extra env var is needed unless
  you mount it somewhere else. Secrets referenced as `${MATRIX_PASSWORD}` /
  `${MATRIX_ACCESS_TOKEN}` in the config come from `-e`/`--env-file`, same as the
  systemd unit -- never bake them into the image or the config file itself.
- The mounted config must be **readable by uid 65532**, the non-root user the
  container runs as. `0644` is fine here and is not the same compromise as it would
  be for the systemd install: the config holds only `${MATRIX_PASSWORD}`-style
  placeholders, never the secret itself. (A `0600` root-owned file, as the systemd
  section instructs, is unreadable inside the container -- there is no
  `LoadCredential=` equivalent for a bind mount, and the container exits with a
  single "Permission denied" line.)
- `/var/lib/chimney-post` holds the SQLite outbox and the Matrix E2EE key store and
  must be a persistent volume; without it, mail queued between restarts and the
  encryption identity are both lost.
- The image is built `FROM` [`gcr.io/distroless/cc-debian12:nonroot`](https://github.com/GoogleContainerTools/distroless)
  (see `Dockerfile`) -- no shell, no package manager, runs as a fixed non-root UID
  (`65532:65532`).
- **Unlike the systemd install, which binds `127.0.0.1` at the OS level and can never
  be reached over the network, publishing the container's port is entirely your
  call.** The SMTP listener has no authentication -- anything that can reach it can
  inject Matrix messages. `-p 127.0.0.1:2525:2525` above keeps the same loopback-only
  guarantee; if you instead attach the container to a docker network so other
  containers can reach it directly (`chimney-post:2525`, no published port at all),
  make sure that network only contains senders you trust.

Verify the image the same way as the release tarballs (cosign signature + SLSA
provenance) -- see the "Container Image" section of each release's notes for the
exact commands.

## Sending Mail from Local Tools (`mailx`, cron, apticron, ...)

Most tools that "send mail" -- `mailx`, `cron`, `apticron`, `rkhunter`, `logwatch`, etc. -- do not speak SMTP. They shell out to the `/usr/sbin/sendmail` binary interface and expect a local MTA to be installed. Chimney Post is an SMTP server, not a `sendmail` provider, so you bridge the two with a tiny send-only MTA that accepts mail on the `sendmail` interface and relays it over SMTP to Chimney Post's localhost listener.

```txt
mailx / cron  ──>  /usr/sbin/sendmail  ──>  SMTP 127.0.0.1:2525  ──>  Chimney Post  ──>  Matrix
                   (msmtp)
```

### Recommended: `msmtp`

`msmtp` is the lightest option: no daemon, no spool, a single binary. `install.sh` (see [Quick Install](#quick-install-linux)) automates this step across Debian/Ubuntu, Fedora, RHEL-family (via EPEL), openSUSE, Arch, and (best-effort) Nix -- run it with `CHIMNEY_MTA=yes` to set msmtp up without the interactive prompt. The commands below are the manual equivalent, package names vary by distro:

```bash
# Debian/Ubuntu -- msmtp-mta installs the /usr/sbin/sendmail symlink mailx/cron expect
sudo apt install msmtp msmtp-mta bsd-mailx

# Fedora -- msmtp itself ships /usr/bin/sendmail directly
sudo dnf install msmtp s-nail

# RHEL/CentOS/Rocky/Alma -- msmtp is in EPEL and registers itself via `alternatives`
sudo dnf install epel-release && sudo dnf install msmtp s-nail

# openSUSE
sudo zypper install msmtp msmtp-mta mailx

# Arch -- msmtp ships no sendmail-compatible symlink; create one yourself
sudo pacman -S msmtp s-nail
sudo ln -sf "$(command -v msmtp)" /usr/local/bin/sendmail
```

`/etc/msmtprc`:

```ini
defaults
auth   off
tls    off
syslog on

account chimney
host    127.0.0.1
port    2525
# e.g. root@myserver -- Chimney Post reads this as the From header
from    %U@%H

account default : chimney
```

`auth off` and `tls off` are correct here: Chimney Post's SMTP listener is plaintext, unauthenticated, and bound to localhost only. Adjust `port` if you changed `[smtp].bind`. Test it:

```bash
echo "test body" | mail -s "test subject" alerts@localhost
```

Without any `[[matrix.routes]]` rules the recipient address is just a placeholder -- Chimney Post forwards everything to your one configured `room_id`. Once you add routing rules, the recipient (and/or sender) address selects the destination room; see [Room routing](#room-routing).

### Trade-off: synchronous send vs. local queue

`msmtp` sends synchronously and does **not** queue. If Chimney Post is momentarily unreachable (e.g. mid-restart) when a job fires, that submission fails and the local tool sees an error. In practice this is rare: Chimney Post binds localhost and starts accepting and queuing mail immediately on boot -- before the Matrix connection is even up -- so the listener is almost always reachable, and its own durable SQLite outbox owns the reliability that actually matters (the flaky Matrix hop).

If you want a second layer of durability across Chimney Post restarts, use a queuing relay instead of `msmtp`:

| Option         | Daemon | Local queue | Notes                                            |
|----------------|--------|-------------|--------------------------------------------------|
| **msmtp**      | no     | no          | Simplest; fine given Chimney Post's own queue.   |
| **nullmailer** | yes    | yes         | Tiny; spools locally and retries to the relay.   |
| **dma**        | no     | yes         | Queues; flushes on submission and via cron.      |

Start with `msmtp`; reach for `nullmailer` only if you actually observe mail lost during restarts.

## Development

### Build and Test

```bash
cargo fmt --check    # Check formatting
cargo clippy         # Lint (warnings treated as errors in CI)
cargo test           # Run unit and integration tests
cargo build          # Debug build
```
