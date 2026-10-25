# Answering Emails from Matrix

Chimney Post normally works in one direction: a local service sends an email,
and it shows up in your Matrix room. With `[reply]` enabled it also works in
the other direction. Reply to a forwarded email in Matrix, and the answer goes
back by email to whoever sent it — ping, then pong.

This is **off by default**. With it off, Chimney Post never syncs from Matrix
and never sends email.

## Answering an email

1. A forwarded email arrives in the room.
2. In your Matrix client, use **Reply** on that message (a normal reply, not a
   thread) and write:

   ```txt
   !reply Thanks, restarting the service now.
   ```

3. Once the email is sent, Chimney Post reacts to your message with ✅.

The answer goes to the email's `Reply-To` address, or to its `From` address if
there is no `Reply-To`. A template can show this address before you answer;
see the `reply_to` variable under [Message Templates](../README.md#message-templates).
The subject is `Re: <original subject>`. The answer is threaded under the
original with `In-Reply-To`/`References`, so the receiving side can match the
two. The body is your text after the prefix, sent as plain text.

## The strict format

A message only counts as an email answer when **all** of these hold:

| Rule | Why |
|---|---|
| It starts with the prefix word (`!reply` by default, `reply.prefix`), followed by a space or newline | Ordinary chat in the room never sends mail. |
| It is a plain text message (`m.text`) | Emotes, notices, files and edits are never sent. |
| It is a Matrix **reply** to a forwarded email, not a thread message | The reply decides who the email goes to. Nothing you type can change the recipient. |
| The forwarded email was delivered to **this room** within `reply.max_age_hours` (default 7 days) | Old mail can't be answered forever, and you can't answer an email delivered to a different room. |
| The answer is non-empty, at most `reply.max_body_len` characters (default 4000), with no control characters except newlines and tabs | |

Messages without the prefix are ignored silently. A message *with* the prefix
that breaks a rule gets a notice from the bot saying why it was not sent, for
example `Not sent: send it as a reply to the forwarded email (not in a thread).`

## Who may answer

On top of the format, the sender and the message have to pass these checks:

- **Allowlist.** Only the Matrix users in `reply.allowed_senders` can answer.
  Messages from anyone else, including other members of the room, are ignored
  with no notice.
- **End-to-end encryption.** The message itself must be encrypted. Answers in
  an unencrypted room are refused, even with `matrix.require_e2ee = false`.
  Without encryption there is no sending device to check.
- **Cross-signed device** (`reply.require_cross_signed_device`, default
  `true`). The message must come from a device its owner has verified with
  cross-signing. Element and other clients do this when you log in a new
  session and verify it. This means a stolen access token used on a new device
  cannot send mail. A message whose sender does not own the sending device is
  always refused.
- **Rate limit.** At most `reply.max_per_hour` answers (default 20) are
  accepted in any rolling hour, across all rooms.
- **Exactly once.** Each Matrix message is sent at most once, even if the
  homeserver delivers it again, for example after a restart.

## Delivery

Accepted answers are stored in the same SQLite database as the email queue
(`queue.db_path`) before anything is sent, so a restart doesn't lose them.
They are then submitted to `[reply.smtp]`:

- `security = "starttls"` (default, port 587) or `"tls"` (port 465). TLS is
  required, and the server certificate is checked against the system trust
  store.
- `security = "none"` (port 25) is accepted **only** for a loopback host:
  `localhost`, `127.0.0.1` or `::1`. That is, a local MTA such as postfix
  that relays the mail itself. Any other host is rejected at startup.
- `username`/`password` are optional and must be set together. Supply them
  through `${REPLY_SMTP_USERNAME}`/`${REPLY_SMTP_PASSWORD}`, the same way as
  the Matrix credentials. For Docker Compose, add both to the service's
  `environment:` in `compose.yaml`.

A failed send is retried with the `[queue]` settings (`max_retries`,
`retry_backoff`). A permanent refusal (a 5xx reply such as "no such user")
gives up straight away. Either way, an answer that cannot be sent is moved to
the `reply_dead_letter` table and the bot posts a notice in the room.

## Example

```toml
[reply]
enabled = true
allowed_senders = ["@you:example.com"]
from = "chimney-post@example.com"

[reply.smtp]
host = "smtp.example.com"
security = "starttls"
username = "${REPLY_SMTP_USERNAME}"
password = "${REPLY_SMTP_PASSWORD}"
```

See `config.example.toml` for every option and its default.

## What leaves the machine

Your answer is end-to-end encrypted in Matrix. Once Chimney Post sends it, it
is an ordinary email, protected only by TLS on the way to your SMTP server and
by whatever happens after that. Don't put anything in an answer you wouldn't
put in an email.

The answer text is also stored unencrypted in `queue.db` until it is sent, the
same way forwarded emails are. Keep that file readable only by the service
user.

## Database tables

| Table | Holds | Kept for |
|---|---|---|
| `reply_targets` | Which Matrix event each forwarded email became, its reply address, Message-ID and subject | `max_age_hours`, then pruned |
| `reply_events` | Matrix events already accepted as answers (dedupe + rate limit) | `max_age_hours`, then pruned |
| `reply_outbox` | Answers waiting to be sent | Until sent or dead-lettered |
| `reply_dead_letter` | Answers that could not be sent, with the last error | Kept for inspection |

Emails delivered before `[reply]` was enabled have no `reply_targets` entry,
so they cannot be answered.
