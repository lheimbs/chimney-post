# Future option: persist & restore the Matrix session

> Status: **not implemented** — a deliberate roadmap note. Today's recommended
> setup is pinning `device_id` (see the `[matrix.credentials]` section in the
> README). This document records the technically superior alternative so it
> isn't lost.

## Problem this solves

`MatrixClient::connect` logs in afresh on every start. With `password` auth and
a pinned `device_id`, that re-login reuses the same Matrix device, so the crypto
store stays consistent — good enough for a single-server bot. But it still:

- sends the password to the homeserver on every boot, and
- mints a fresh access token each time (old tokens linger server-side), and
- relies on the operator remembering to set a stable `device_id`.

## The improvement

After the first successful login, persist the full `MatrixSession`
(`user_id`, `device_id`, `access_token`, `refresh_token`) to disk. On
subsequent starts, `restore_session` from that file instead of logging in. Only
fall back to a fresh `password` / `access_token` login when no saved session
exists or the saved one is rejected. This eliminates per-boot re-login entirely
and keeps one stable device automatically, without the operator choosing a
`device_id`.

## Sketch

1. Add a config key, e.g. `matrix.session_path` (default alongside
   `queue.db_path`, under the systemd `StateDirectory`).
2. In `connect()`:
   - If the session file exists → `client.restore_session(saved)`.
     - On `is_mismatched_account` (or any restore failure) → run the existing
       `reset_crypto_store` recovery, delete the stale session file, and fall
       through to a fresh login.
   - Else → perform the current `password` / `access_token` login.
   - After any successful fresh login → capture
     `client.matrix_auth().session()` and write it to the session file.
3. Serialize with `serde_json` (a new dependency; `MatrixSession` does not
   round-trip cleanly through `toml`).
4. Write the file with `0600` permissions — it contains the access token, a new
   secret-at-rest surface that must be documented and protected.

## Why it isn't done yet

- The session file is a sensitive credential on disk (token), with its own
  failure modes (corruption, server-side revocation/expiry).
- It still requires `password` / `access_token` configured as the fallback, so
  it does not remove the credential — it adds a file on top of it.
- `connect()` gains real branching that interleaves with the two existing auth
  modes and the crypto-store recovery path.
- `connect()` has no automated test coverage (it needs a live/mock homeserver),
  so most of this would be validated manually.

For a single-server notification forwarder, pinning `device_id` delivers ~95% of
the benefit for ~1% of the effort. Revisit this if you want a fully unattended,
zero-re-login setup.
