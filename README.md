# Chimney Post

Local-only SMTP server that forwards incoming email to Matrix with end-to-end encryption.

:warning: Created with AI! Do not use in Prod! :warning:

## Status

Prototype in progress.

## Quick Start

1. Copy the example config:
   - `cp config.example.toml config.toml`
2. Set Matrix credentials via environment variables.
3. Run the service:
   - `cargo run`

## Security

- Binds to localhost only.
- Matrix messages must be end-to-end encrypted.
- Store Matrix credentials in environment variables, not in config files.
