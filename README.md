<p align="center">
  <img src="chimney-post_banner.png" alt="Chimney Post -- emails enter a chimney and emerge as encrypted Matrix messages" width="800">
</p>

<h1 align="center">Chimney Post</h1>

<p align="center">
  A local-only SMTP server that forwards incoming email to Matrix with end-to-end encryption.
</p>

<p align="center">
  <a href="https://github.com/lheimbs/chimney-post/actions/workflows/ci.yml"><img src="https://github.com/lheimbs/chimney-post/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/lheimbs/chimney-post/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/rust-1.74%2B-orange" alt="Minimum Rust version: 1.74">
</p>

---

**Warning**: This project was created with AI assistance and is in an early prototype stage. Use at your own risk.

---

## About

Chimney Post sits on your local machine, accepts emails over SMTP, and delivers them as end-to-end encrypted messages to a Matrix room. It is designed for forwarding automated notifications on your server - be it nextcloud, rkhunter for rootkit hunting or apticron for automated upgrades - into one encrypted Matrix chat.

The SMTP server binds exclusively to `127.0.0.1`, so it never accepts connections from the network. Matrix messages are encrypted by default using the `matrix-sdk` E2EE implementation, and the encryption store is persisted locally in SQLite so device keys survive restarts.

This is essentially a super narrow version of [mailrise](https://github.com/YoRyan/mailrise) but intended for a single server and its services and only forwarding to matrix.

## Features

- **Local SMTP server** -- Listens on localhost only; never exposed to the network.
- **End-to-end encrypted Matrix delivery** -- All messages are sent through E2EE. Optionally enforce that the target room is encrypted before sending.
- **Password or access-token authentication** -- Connect to any Matrix homeserver with either method.
- **Configurable message templates** -- Format forwarded emails with MiniJinja templates (subject, body, sender, recipient are all available as variables).
- **Durable, persistent queue** -- Accepted emails are written to a local SQLite outbox *before* the SMTP `250 OK` and only removed once delivered, so messages survive restarts and crashes.
- **Retry with exponential backoff** -- Failed Matrix deliveries are retried automatically (configurable attempts and interval). A single failing message backs off independently and never blocks delivery of the rest of the queue.
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
5. If delivery fails, the message is rescheduled with exponential backoff up to the configured retry limit; the worker moves on to the next due message so one failure never blocks the queue. Messages still pending are retained across restarts and retried on the next start.

## Getting Started

### Prerequisites

- Rust 1.74 or later (install via [rustup](https://rustup.rs/))
- A Matrix account for the bot
- A Matrix room where the bot should post (invite the bot user to the room)

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

| Key                | Default          | Description                                                               |
|--------------------|------------------|---------------------------------------------------------------------------|
| `bind`             | `127.0.0.1:2525` | Address and port the SMTP server listens on. Must be a localhost address. |
| `max_message_size` | `10485760`       | Maximum email size in bytes (10 MB).                                      |
| `timeout`          | `30`             | Connection timeout in seconds.                                            |

### `[matrix]`

| Key                | Default          | Description                                        |
|--------------------|------------------|----------------------------------------------------|
| `homeserver`       | --               | Matrix homeserver URL (e.g. `https://matrix.org`). |
| `user_id`          | --               | Full Matrix user ID (`@user:server.com`).          |
| `device_name`      | `chimney-post`   | Display name for the Matrix device.                |
| `room_id`          | --               | Target room ID (`!room:server.com`).               |
| `store_path`       | --               | Directory for the E2EE key store (SQLite).         |
| `require_e2ee`     | `true`           | Refuse to send if the room is not encrypted.       |
| `message_template` | *(built-in)*     | MiniJinja template for formatting messages.        |

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
| `max_retries`   | `5`                              | Maximum retries after the initial attempt before a message is dropped.   |
| `retry_backoff` | `60`                             | Base backoff interval in seconds (doubles each retry, capped at 900s).   |
| `db_path`       | `/var/lib/chimney-post/queue.db` | Path to the persistent SQLite outbox; must be writable by the service.   |

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

The service unit runs with `DynamicUser=yes` and strict filesystem protections (read-only root, private `/tmp`, no new privileges). State data (E2EE key store) is kept under `/var/lib/chimney-post`.

Set secrets in a systemd environment file or drop-in override:

```bash
sudo systemctl edit chimney-post
```

```ini
[Service]
Environment=MATRIX_PASSWORD=your-secret
```

## Development

### Build and Test

```bash
cargo fmt --check    # Check formatting
cargo clippy         # Lint (warnings treated as errors in CI)
cargo test           # Run unit and integration tests
cargo build          # Debug build
```
