# AGENTS.md

Guidance for AI coding agents (and human contributors) working in this repository.

## Project Overview

**chimney-post** is a local-only SMTP server, written in Rust, that forwards
incoming email to a Matrix room as end-to-end encrypted messages. It's built
for forwarding automated notifications (cron, apticron, rkhunter, Nextcloud,
...) from a single server into one encrypted Matrix chat — a narrow,
single-server alternative to [mailrise](https://github.com/YoRyan/mailrise).

Key properties that shape most design decisions:

- The SMTP listener binds to `127.0.0.1` only — it must never accept
  connections from the network.
- Matrix delivery is E2EE via `matrix-sdk`, with the crypto store persisted
  to SQLite so device identity survives restarts.
- Accepted mail is durably queued (SQLite outbox) *before* the SMTP `250 OK`
  is sent, and only removed once Matrix delivery succeeds. Delivery is
  at-least-once with retry/backoff and a dead-letter table for messages that
  exhaust retries.
- The SMTP listener and the Matrix delivery worker are decoupled: SMTP starts
  accepting and queuing mail immediately, independent of whether the Matrix
  connection is up yet.

See `README.md` for the full feature list, configuration reference, and
`docs/session-persistence.md` / `docs/testing.md` for deeper topic guides —
don't duplicate that material here; read it when you need it.

## Architecture

```
 Service / Script          Chimney Post                    Matrix Homeserver
 ─────────────────    ─────────────────────────────    ────────────────────────
  sends email  ──────> SMTP server (127.0.0.1:2525)
                       parses headers & body
                       persists to SQLite outbox ─────> Matrix client sends
                       (then acks 250 OK)                E2EE message to room
                       on success: delete from outbox
                       on failure: reschedule w/ backoff
```

Module layout (`src/`):

| Module | File(s) | Responsibility |
|---|---|---|
| `config` | `config.rs` | TOML config loading, env-var substitution, validation (e.g. rejecting non-loopback SMTP binds, invalid Matrix IDs). |
| `error` | `error.rs` | Single `ChimneyError` enum (`thiserror`) and the crate-wide `Result<T>` alias. |
| `smtp` | `smtp/server.rs`, `smtp/parser.rs` | Async SMTP session handling and email parsing (headers, folded-header unfolding, body extraction). |
| `matrix` | `matrix/client.rs`, `matrix/formatter.rs`, `matrix/routing.rs` | Matrix login/connect (password or access-token), MiniJinja message formatting, and recipient/sender-based room routing. |
| `queue` | `queue/store.rs`, `queue/worker.rs` | SQLite-backed durable outbox (`MessageStore`) and the delivery worker (retry with exponential backoff, dead-lettering, reconnect-on-failure). |
| `main` | `main.rs` | Orchestration: binds SMTP first (readiness point), spawns the SMTP server and delivery worker as independent tasks, handles graceful shutdown (drain queue, systemd notify/watchdog). |

`lib.rs` just re-exports the modules — treat it as the public surface when
adding new modules.

Data flow: SMTP session → parse → `MessageStore` (SQLite, ack `250` only
after durable write) → delivery worker picks up due messages → route to a
room → render via MiniJinja template → send E2EE via `matrix-sdk` → delete
from store on success, else reschedule with backoff or dead-letter after
`queue.max_retries`.

## Rust Coding Guidelines

- **Idiomatic, minimal-dependency Rust.** Prefer the standard library and
  the crates already in `Cargo.toml`; think twice before adding a new
  dependency, especially one with a deep dependency tree.
- **Research before adding any new dependency** — a Rust crate, a GitHub
  Actions step in `.github/workflows/`, or anything else. Look up its
  current latest version rather than guessing or relying on training data,
  and check it's actually compatible with this project (crate: matches
  `edition = "2021"` and `rust-version = "1.88"` in `Cargo.toml`, and the
  1.93 toolchain pinned in CI — see the comment in `ci.yml` about the
  clippy/matrix-sdk regression on 1.94+; Action: matches the runner and
  any adjacent pinned actions) before pinning a version.
- **No `unwrap()`/`expect()` on paths reachable from production code.**
  Propagate errors through `Result<T, ChimneyError>` (`error.rs`). `unwrap`
  is fine in tests and in cases that are genuinely infallible (document why
  if it's not obvious).
- **Errors go through `ChimneyError`.** Add a new variant (with a `#[error]`
  message) rather than stringly-typed ad hoc errors; use `#[from]` for
  straightforward conversions.
- **Async correctness.** This is a Tokio codebase — never block the runtime
  with synchronous I/O or `std::thread::sleep`; use `tokio::time::sleep`,
  `tokio::fs`, etc.
- **Security invariants are non-negotiable:** SMTP must stay bound to
  loopback only, Matrix messages must stay E2EE by default, credentials
  come from environment variables (never hardcoded or logged), and logs
  must not leak message bodies, passwords, or tokens.
- **Doc comments on public APIs.** Use `///` with a `# Errors` section for
  fallible public functions, matching the existing style in `matrix/`,
  `queue/`, and `smtp/`.
- **Tests accompany behavior changes.** Unit tests live next to the code
  they cover; cross-cutting behavior goes in `tests/integration_test.rs`.
  See `docs/testing.md` for the full testing guide (including manual E2EE
  checks that can't be automated).
- **Keep changes small and reviewable.** Match existing style and structure
  rather than introducing new patterns; don't refactor unrelated code while
  fixing something else.

## Validation Pipeline

Run this before every commit and before opening or updating a PR — CI
enforces the same checks (`.github/workflows/ci.yml`) and will fail the
build otherwise:

```bash
cargo fmt --check       # formatting must be clean
cargo clippy -- -D warnings   # zero warnings allowed
cargo test               # unit + integration tests must pass
```

`cargo fmt` (without `--check`) to actually apply formatting locally. CI
also runs an MSRV check (`cargo check --all-targets` on the Rust version
pinned in `Cargo.toml`'s `rust-version`) and `cargo audit` — keep both in
mind for dependency or edition changes, but they don't need to be run for
every trivial change.

## Agent Workflow

### Implementing a new feature

When instructed to work on a new feature or fix:

1. Create a local branch off `main` (e.g. `git checkout -b feature/short-name`).
2. Implement the change, following the guidelines above.
3. Run the full validation pipeline (`cargo fmt --check`, `cargo clippy -- -D
   warnings`, `cargo test`) and fix anything it flags before proceeding.
4. Commit with a clear, conventional message (`feat:`, `fix:`, `docs:`,
   `test:`, `refactor:`, `chore:` prefix + concise summary).
5. Push the branch and open a pull request against `main` describing what
   changed and why.

### Reviewing a pull request

When instructed to review a PR, post the review as comments/a review on
the PR itself (e.g. via `gh pr review` / `gh pr comment`) rather than only
reporting findings back in chat — the review should be visible to anyone
looking at the PR on GitHub.

### Versioning

`Cargo.toml`'s `version` follows SemVer, and a pushed `vX.Y.Z` tag matching
it triggers `.github/workflows/release.yml`. While the crate stays pre-1.0
(`0.y.z`), treat a minor bump (`0.y.z` → `0.(y+1).0`) as the breaking-change
slot and a patch bump (`0.y.z` → `0.y.(z+1)`) as backwards-compatible
fixes/additions, per Cargo's SemVer convention for `0.x` versions.

Whenever a change would warrant a version bump, say so directly to the
user (don't bump `Cargo.toml` or tag a release yourself) — state the
current version, the suggested next version, and the one-line reason
(e.g. "0.1.0 → 0.2.0: adds room-routing config, a new user-facing
feature"). Cutting the release (bumping `Cargo.toml`, tagging, pushing the
tag) is the user's call.

### Attribution

Any content an agent posts to GitHub — commits, PR descriptions, PR
comments, and review comments — must identify the acting model as author,
e.g. a `Co-Authored-By: <Model Name> <noreply@anthropic.com>` trailer on
commits, and an explicit mention (e.g. "Posted by <Model Name>") in
standalone PR/review comments. Don't post anonymously or imply the change
was authored solely by a human.
