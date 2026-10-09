# Runtime notes

Requires a gray core implementing `gray -p --json` protocol 1 (the same
`{"protocol":1,"turn_id",...}` NDJSON the discord/slack adapters consume).

## Transport

signal-cli daemon JSON-RPC 2.0 over TCP (`signal-cli daemon --tcp
host:port`, default `127.0.0.1:7583`) — newline-delimited JSON on one
connection both ways. tokio only; no HTTP, no Signal protocol code: the
daemon owns accounts, keys, groups, re-encryption and delivery.

One connection is shared: a reader task demultiplexes inbound lines —
responses carry `id` and resolve pending requests; `receive`
notifications push `params.envelope` to the subscription channel. Calls
have a 30 s timeout; a dead socket fails every pending call and closes
the envelope channel, which is the reconnect signal. Reconnects re-dial
and re-`subscribeReceive` with 1 s → 60 s backoff, workers untouched.

RPCs used: `subscribeReceive`, `send` (`recipient`/`groupId`, `message`,
`textStyle`/`textStyles`), `sendTyping` (with `stop`), `getAttachment`,
`version`, `listAccounts`.

## Inbound policy

- Sender is `sourceNumber` else `sourceUuid` else `source`. The bot never
  answers its own account — except `syncMessage` note-to-self envelopes
  (destination == account, or a group send with no destination), which
  unwrap to `dataMessage` like hermes.
- Only `dataMessage` (and `editMessage.dataMessage`, which is a fresh
  turn) produces work; receipts, typing, stories and contentless wrappers
  are dropped.
- Admission: the sender's number/UUID must be on `allowed_numbers` —
  groups do not bypass it. Group messages additionally need the
  `group:<id>` on `allowed_groups` (`*` admits all), and with
  `require_mention` (default) a mention of the account in
  `dataMessage.mentions` or a textual `@+number`, unless the group is in
  `free_response_groups`.
- An un-admitted DMer gets a pairing reply (code minted in SQLite);
  `gray signal pairing approve signal <code>` admits the number/UUID.
  Group strangers get silence.
- Quotes are flattened into the prompt (`[in reply to <author>: <text>]`);
  attachments are fetched via `getAttachment` (≤25 MB) into
  `<conversation>/attachments/` and surfaced as `[attachment saved: path]`
  prompt lines. Attachments are inbound-only by design.
- Conversation keys: `dm:<sender>` or `group:<group_id>`.
- Session reset policy `session_reset` (`both`: 1,440 idle minutes or the
  next 04:00 local boundary) mirrors the other adapters; `/new` and
  `/reset` force it early.

## Queue

Accepted messages commit to `signal.sqlite` next to the config before any
agent work: `queued` → `running` → `done`/`failed`/`cancelled`. Boot
re-queues interrupted `running` rows and reports the count in the startup
notice. `concurrency` workers (default 2, max 16) claim items; a
per-conversation flock serializes turns inside one chat. `queue list`
shows pending work; `queue cancel <id>` cancels what is still queued.

Inbox ids are `sig:<timestamp_ms>:<sender>` — a redelivered envelope
dedupes on insert. Envelopes that arrive while the socket is down are
lost, not queued: `subscribeReceive` is live-only and signal-cli does not
retain a consumer backlog for it.

## Delivery

Replies go through `send` with `textStyles` — markdown is converted to
plain text plus `start:length:STYLE` ranges in UTF-16 code units
(BOLD/ITALIC/STRIKETHROUGH/MONOSPACE; headings bold, `- ` bullets become
`•`). Answers longer than 8,000 UTF-16 units split into `(i/N)` chunks
with styles re-based per chunk so markers never leak. A generated answer
retries delivery (bounded) across a daemon outage rather than
regenerating — delivery is at-least-once, never exactly-once. A typing
indicator brackets a running turn, best-effort.

`signal_send` is a standalone resolve-and-send, not an outbox transaction
— its errors must not be blindly retried.

## Limits

`gray signal limits --timeout-seconds 600 --concurrency 2 --max-requests 200`
sets turn wall time, worker count, and per-turn provider requests; restart
to apply. `queue_capacity` (default 1,000) bounds the inbox; a full queue
tells the sender instead of dropping silently.

## Lifecycle notices

`lifecycle.json` + `restart_pending` next to the config, worded by
`gray gateway lifecycle boot|stop` when the installed gray has it (local
fallback otherwise). Chats with a running turn hear the shutdown/restart
notice; the home chat hears the boot notice after the first subscribe.
`"restart_notification": false` silences it.
