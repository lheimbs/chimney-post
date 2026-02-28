# Testing Guide

This guide covers end-to-end, integration, and unit testing for chimney-post.
It is written for the current codebase and matrix-sdk 0.16.0.
Follow each section in order for full coverage.

## Prerequisites

- Rust toolchain installed (rustc, cargo).
- A local Matrix homeserver for integration tests (recommended: a Synapse test instance).
- Access to a Matrix account that can join the target room.
- A Matrix client that can verify E2EE (Element Desktop or Element Web).
- Local SMTP client (for example: `swaks`, `telnet`, or `openssl s_client`).

## Test Data and Environment Setup

### 1) Create a dedicated Matrix room

Create a room exclusively for testing and enable encryption in the room settings. Invite the bot user configured for chimney-post.

### 2) Create a test config

Copy the example config and set the Matrix and SMTP values.

Checklist:

- Use a localhost bind for SMTP.
- Use a room ID (not alias).
- Use either password or access token, not both.
- Set a writable store path for the E2EE store.

### 3) Ensure local-only SMTP binding

The SMTP server must bind to 127.0.0.1. Any non-loopback bind should fail validation.

## Unit Tests

Run the unit test suite:

- `cargo test`

What this covers:

- Config validation for loopback-only SMTP binding.
- Email parser logic (subject parsing, folded header handling).
- Matrix formatter behavior for missing fields.

## Integration Tests

Run integration tests:

- `cargo test --test integration_test`

What this covers:

- Config rejects non-loopback SMTP bind (integration-level).

## Manual End-to-End Tests

These tests verify SMTP intake, parsing, queueing, and Matrix delivery with E2EE.

### A) SMTP session acceptance

1. Start chimney-post with your test config.
2. Send a message to the local SMTP listener.
3. Confirm the SMTP session completes with a 250 response.

Expected:

- SMTP session is accepted on localhost only.
- No errors in logs for session parsing.

### B) Email parsing coverage

Send multiple emails to exercise parser behavior:

1. Subject with mixed case header (Subject vs subject vs SUBJECT).
2. Folded Subject header.
3. Missing Subject header.
4. Multipart email (plain text + HTML).

Expected:

- Subject is captured correctly.
- Folded headers are unfolded correctly.
- Missing subject results in a sensible default in the formatted message.
- Plain text body is used if HTML is present.

### C) Queue behavior

1. Stop the Matrix server temporarily.
2. Send an email via SMTP.
3. Restart the Matrix server.

Expected:

- Message is queued while Matrix is unavailable.
- Delivery resumes after Matrix is reachable.
- Retry logic respects the configured backoff.

### D) Matrix login modes

Run two separate tests:

1. Password login:
   - Provide `matrix.credentials.password` in config.
   - Omit access token.
2. Access token login:
   - Provide `matrix.credentials.access_token` and `matrix.credentials.device_id`.
   - Omit password.

Expected:

- Both modes authenticate successfully.
- The device ID is stable across restarts when using access tokens.

### E) E2EE verification

1. Ensure the Matrix room is encrypted.
2. Send an email.
3. Verify in the Matrix client that the message is encrypted end-to-end.

Expected:

- The client shows the message as encrypted.
- No plaintext is sent when the room is encrypted.

### F) Large message behavior

1. Send an email close to `smtp.max_message_size`.
2. Send an email that exceeds `smtp.max_message_size`.

Expected:

- Near-limit message is accepted and forwarded.
- Oversized message is rejected at SMTP with a clear error.

### G) Multiple recipients

1. Send an email with multiple recipients (To, Cc).

Expected:

- Message is forwarded once to the configured Matrix room.
- Recipient list is preserved in the formatted output.

## Security and Safety Checks

### 1) Local-only enforcement

Attempt to bind to a non-loopback address. This should fail at startup.

Expected:

- Clear error message in logs.
- Process exits without starting SMTP listener.

### 2) Credential handling

Confirm that logs never include:

- Access tokens
- Passwords
- Full email body content

Expected:

- Only metadata is logged.

### 3) Store path permissions

Ensure the Matrix store path is private.

Expected:

- Files are not world-readable.

## Observability Checks

Confirm that logs include:

- SMTP connections and sender metadata.
- Matrix delivery attempts and failures.
- Retry events and backoff intervals.

## Troubleshooting Matrix Delivery

If Matrix delivery fails:

1. Verify the homeserver URL.
2. Verify the room ID exists and the bot user is joined.
3. Confirm E2EE is enabled on the room.
4. Check that the store path is writable.

Expected:

- Errors should be explicit and actionable in logs.

## Validation Pipeline

Run the standard validation sequence after changes:

- `cargo fmt`
- `cargo clippy`
- `cargo test`
- `cargo build --release`

## Notes

- This guide assumes the default queue is in-memory. If you add a disk-backed queue, extend the queue section to cover persistence and restart behavior.
- The Matrix SDK handles encryption automatically when the room is encrypted. Manual encryption checks are not required for sending messages.
