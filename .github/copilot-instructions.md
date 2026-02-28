# Chimney Post - SMTP to Matrix Forwarder

## Project Overview

**chimney-post** is a secure, local-only SMTP server that forwards incoming emails to Matrix as end-to-end encrypted messages. Written in Rust with minimal dependencies for maximum stability and security.

### Core Requirements

- **Local SMTP Server**: Accept emails on localhost only (no external email reception)
- **Matrix Integration**: Forward all received emails as Matrix messages with E2EE
- **Security**: All Matrix messages must use end-to-end encryption
- **Stability**: Production-grade reliability with proper error handling
- **Minimal Dependencies**: Only essential crates (SMTP, Matrix, crypto, config)
- **Configuration**: Secure, file-based configuration with environment variable support

## Architecture

### Components

1. **SMTP Server** (local only, port 25/587/custom)
   - Listen on 127.0.0.1 only
   - Parse incoming SMTP sessions
   - Extract email metadata (from, to, subject, body)
   - Queue messages for Matrix delivery

2. **Matrix Client**
   - E2EE capable Matrix client
   - Message formatting (email → Matrix)
   - Delivery confirmation and retry logic
   - Session persistence

3. **Configuration Manager**
   - TOML-based configuration
   - Environment variable overrides
   - Secure credential storage
   - Validation on startup

4. **Message Queue**
   - In-memory queue with disk persistence option
   - Retry logic for failed deliveries
   - Rate limiting support

## Rust Development Guidelines

### Core Principles (from rustic-prompt)

- **Make small, reviewable changes**: Incremental development
- **Preserve existing style**: Consistent code formatting
- **Idiomatic Rust**: Leverage ownership/borrowing, avoid unnecessary `clone()`
- **Error Handling**: Never use `unwrap()`/`expect()` in production code
- **Testing**: Add tests for all behavior changes
- **Documentation**: Doc comments for public APIs

### Validation Pipeline

Before committing any Rust code, ALWAYS run:

```bash
cargo fmt      # Format code
cargo clippy   # Lint and catch common mistakes
cargo test     # Run test suite
cargo build --release  # Verify release build
```

### Dependency Selection

**Allowed Essential Dependencies:**
- `tokio` - Async runtime (with minimal features)
- `matrix-sdk` - Matrix client with E2EE support
- `mailin` or `smtp-server` - SMTP server implementation
- `serde`, `serde_json` - Serialization
- `toml` - Configuration parsing
- `tracing`, `tracing-subscriber` - Logging
- `anyhow` or `thiserror` - Error handling
- `clap` - CLI argument parsing (minimal features)

**Minimize:**
- Avoid large frameworks
- Use feature flags to reduce binary size
- Prefer well-maintained, security-audited crates
- Avoid dependencies with deep dependency trees

### Error Handling Pattern

```rust
// Use Result types everywhere
pub type Result<T> = std::result::Result<T, ChimneyError>;

// Custom error enum with thiserror
#[derive(Debug, thiserror::Error)]
pub enum ChimneyError {
    #[error("SMTP error: {0}")]
    Smtp(#[from] SmtpError),
    
    #[error("Matrix error: {0}")]
    Matrix(String),
    
    #[error("Configuration error: {0}")]
    Config(String),
    
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// Never use unwrap() or expect() in production paths
// Always propagate or handle errors explicitly
```

### Project Structure

```
chimney-post/
├── Cargo.toml
├── Cargo.lock
├── README.md
├── LICENSE
├── .gitignore
├── config.example.toml
├── src/
│   ├── main.rs              # Entry point, CLI, orchestration
│   ├── lib.rs               # Library exports
│   ├── config.rs            # Configuration management
│   ├── error.rs             # Error types
│   ├── smtp/
│   │   ├── mod.rs           # SMTP module
│   │   ├── server.rs        # SMTP server implementation
│   │   └── parser.rs        # Email parsing
│   ├── matrix/
│   │   ├── mod.rs           # Matrix module
│   │   ├── client.rs        # Matrix client
│   │   ├── crypto.rs        # E2EE handling
│   │   └── formatter.rs     # Email to Matrix message conversion
│   └── queue/
│       ├── mod.rs           # Queue module
│       └── memory.rs        # In-memory queue implementation
├── tests/
│   ├── integration_test.rs
│   └── fixtures/
└── systemd/
    └── chimney-post.service
```

## Configuration Schema

### config.toml

```toml
[smtp]
# Bind address (must be local)
bind = "127.0.0.1:2525"
# Maximum message size in bytes
max_message_size = 10485760  # 10MB
# Connection timeout in seconds
timeout = 30

[matrix]
# Homeserver URL
homeserver = "https://matrix.org"
# User ID (@user:server.com)
user_id = "@bot:example.com"
# Device display name
device_name = "chimney-post"
# Room ID to send messages to
room_id = "!roomid:example.com"
# Store path for E2EE keys (secure this!)
store_path = "/var/lib/chimney-post/matrix-store"

[matrix.credentials]
# Password (prefer environment variable)
password = "${MATRIX_PASSWORD}"
# Or use access token
# access_token = "${MATRIX_ACCESS_TOKEN}"

[logging]
# Trace, Debug, Info, Warn, Error
level = "info"
# Log format: json or pretty
format = "json"

[queue]
# Maximum retry attempts
max_retries = 5
# Retry backoff in seconds
retry_backoff = 60
```

### Environment Variables

```bash
MATRIX_PASSWORD=secret123
MATRIX_ACCESS_TOKEN=syt_xxxxx
CHIMNEY_CONFIG=/etc/chimney-post/config.toml
```

## Security Requirements

### Credentials Management

- **NEVER** commit credentials to git
- Use environment variables for sensitive data
- Support `.env` file (gitignored) for local development
- Document credential setup in README
- Use secure file permissions (0600) for config files

### Network Security

- **ONLY** bind to 127.0.0.1 (never 0.0.0.0)
- Validate SMTP commands to prevent injection
- Sanitize email content before Matrix forwarding
- Rate limit to prevent abuse
- Set reasonable message size limits

### Matrix E2EE

- **MANDATORY**: All Matrix messages must be E2EE
- Use `matrix-sdk` with crypto feature enabled
- Persist crypto store securely
- Handle key verification gracefully
- Document device verification process

### Audit Trail

- Log all email receptions (metadata only, not content)
- Log all Matrix delivery attempts
- Log authentication events
- Use structured logging (JSON format for production)

## Testing Strategy

### Unit Tests

- Test email parsing with various formats
- Test configuration validation
- Test error handling paths
- Test message formatting

### Integration Tests

- Mock SMTP client sending emails
- Mock Matrix server responses
- Test retry logic
- Test E2EE encryption/decryption

### Manual Testing Checklist

- [ ] Send test email via `telnet localhost 2525`
- [ ] Verify Matrix message delivery
- [ ] Verify E2EE encryption in Matrix client
- [ ] Test service restart and state persistence
- [ ] Test invalid configuration handling
- [ ] Test network failure recovery

## Implementation Phases

### Phase 1: Foundation
- [ ] Project scaffolding (Cargo.toml, directory structure)
- [ ] Configuration management (TOML parsing, validation)
- [ ] Error types definition
- [ ] Logging setup
- [ ] Basic CLI interface

### Phase 2: SMTP Server
- [ ] SMTP server implementation (local bind only)
- [ ] Email parsing (headers, body, attachments)
- [ ] Message queue integration
- [ ] Unit tests for SMTP

### Phase 3: Matrix Client
- [ ] Matrix SDK integration
- [ ] E2EE setup and verification
- [ ] Message formatting (email → Matrix)
- [ ] Delivery confirmation
- [ ] Unit tests for Matrix client

### Phase 4: Integration
- [ ] Connect SMTP server to Matrix client via queue
- [ ] Retry logic implementation
- [ ] Integration tests
- [ ] Error recovery testing

### Phase 5: Deployment
- [ ] systemd service file
- [ ] Installation documentation
- [ ] Configuration examples
- [ ] Security hardening guide
- [ ] Monitoring/logging setup

## Development Workflow

### Starting a New Feature

1. Create a new branch: `git checkout -b feature/name`
2. Write tests first (TDD approach)
3. Implement feature incrementally
4. Run validation pipeline: `cargo fmt && cargo clippy && cargo test`
5. Update documentation
6. Commit with clear message

### Code Review Checklist

- [ ] All tests pass
- [ ] No `unwrap()` or `expect()` in production code
- [ ] Error handling is comprehensive
- [ ] Public APIs have doc comments
- [ ] No security issues (credentials, network binding)
- [ ] Logging is appropriate
- [ ] Performance is reasonable

### Commit Message Format

```
<type>: <short summary>

<detailed description>

<breaking changes if any>
```

Types: `feat`, `fix`, `docs`, `test`, `refactor`, `chore`

## Performance Considerations

- Use `tokio` for async I/O (non-blocking)
- Avoid blocking operations in async contexts
- Use bounded queues to prevent memory exhaustion
- Implement backpressure if needed
- Profile with `cargo flamegraph` for bottlenecks

## Monitoring and Observability

- Structured logging with `tracing`
- Metrics: emails received, Matrix messages sent, errors
- Health check endpoint (optional HTTP server)
- Signal handling (graceful shutdown on SIGTERM)

## Documentation Requirements

General Guidelines for documentation in code and in readme files:

- Avoid typical AI pitfalls like vagueness or overgeneralization.
- Be specific about configuration options and their effects.

### Checking documentation

When instructed to check documentation, use this wikipedia article for references as to what to avoid:

https://en.wikipedia.org/wiki/Wikipedia:Signs_of_AI_writing

Then make sure these things are not included in any docs and fix them if necessary.

### README.md

- Project description
- Features list
- Installation instructions
- Configuration guide
- Usage examples
- Security considerations
- Contributing guidelines
- License information

### Doc Comments

```rust
/// Starts the SMTP server on the configured address.
///
/// # Errors
///
/// Returns an error if:
/// - The bind address is invalid
/// - The port is already in use
/// - The server fails to start
///
/// # Examples
///
/// ```no_run
/// let config = Config::load("config.toml")?;
/// start_smtp_server(&config).await?;
/// ```
pub async fn start_smtp_server(config: &Config) -> Result<()> {
    // implementation
}
```

## Questions to Answer During Development

1. **SMTP Server**: Which crate? `mailin`, `async-smtp`, or custom implementation?
2. **Matrix SDK**: Use `matrix-sdk` with E2EE - confirm minimal feature set
3. **Queue**: In-memory only or disk-backed? (Start with in-memory)
4. **Attachments**: Forward as Matrix file uploads or inline text?
5. **HTML Emails**: Convert to formatted Matrix messages or plain text?
6. **Multiple Recipients**: One Matrix message or separate messages per recipient?

## Anti-Patterns to Avoid

- ❌ Binding to 0.0.0.0 (security risk)
- ❌ Using `unwrap()` without comments explaining safety
- ❌ Storing credentials in code or config files (use env vars)
- ❌ Ignoring errors (always handle or propagate)
- ❌ Blocking async code with `std::thread::sleep`
- ❌ Large dependencies for trivial functionality
- ❌ Unencrypted Matrix messages
- ❌ No tests for critical paths

## Success Criteria

- ✅ Successfully receives emails via SMTP on localhost
- ✅ Forwards emails to Matrix with E2EE
- ✅ Runs 24/7 without crashes
- ✅ Handles network failures gracefully
- ✅ Configuration is simple and secure
- ✅ Binary size < 20MB
- ✅ Memory usage < 100MB under normal load
- ✅ All tests pass
- ✅ No Clippy warnings
- ✅ Complete documentation

## Additional Resources

- [Matrix SDK Documentation](https://docs.rs/matrix-sdk/)
- [Rust Async Book](https://rust-lang.github.io/async-book/)
- [SMTP RFC 5321](https://tools.ietf.org/html/rfc5321)
- [Matrix E2EE Documentation](https://matrix.org/docs/guides/end-to-end-encryption-implementation-guide)

---

**Remember**: Security, stability, and simplicity are the top priorities. When in doubt, choose the most secure and straightforward approach.
