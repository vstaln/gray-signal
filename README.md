# gray-signal

A standalone Signal channel adapter for [gray](https://github.com/vstaln/gray)
— signal-cli `receive` notifications in, `send` replies out, with its own
setup wizard, pairing allowlist, durable SQLite queue, and a background
service. Same shape as `gray-discord-plugin`/`gray-matrix`; the transport
is **signal-cli's JSON-RPC daemon over TCP**, so the Signal protocol (keys,
groups, sealing) is never reimplemented here — signal-cli owns it.

**Status: v1.** Inbound `dataMessage` envelopes from allowlisted numbers
run one isolated `gray` turn per conversation; outbound is `send` with
native Signal text styles. See [runtime policy and limits](docs/RUNTIME.md).

## Requirements

- `gray` on PATH, provider/model configured in `~/.gray/config.json`
- [signal-cli](https://github.com/AsamK/signal-cli) with a registered or
  linked account, running in daemon TCP mode:

  ```sh
  # one-time: register a number (or `signal-cli link -n "bot"` from a
  # primary device's Linked Devices QR)
  signal-cli -a +15551234567 register
  signal-cli -a +15551234567 verify <CODE>

  # then the daemon this adapter talks to:
  signal-cli -a +15551234567 daemon --tcp 127.0.0.1:7583
  ```

  Keep the daemon under a supervisor for production; `--tcp` defaults to
  `localhost:7583`, which is also this adapter's default.

## Install

```sh
cargo install --path .          # puts gray-signal on PATH
gray plugin install signal      # or let gray find the binary
```

## Setup

```sh
gray signal setup               # or: gray-signal setup
```

The wizard asks for:

- the daemon address (`host:port`, default `127.0.0.1:7583`) — probed live
- the bot account number (`+E164`) — checked against `listAccounts`
- numbers/UUIDs allowed to trigger the bot (the sender allowlist)
- a home chat for lifecycle notices (number, UUID, or `group:<id>`)
- group ids the bot may answer in (or `*`; empty = DMs only)

It saves the config privately (`~/.config/gray-signal/config.json`, mode
0600), registers the `signal_send` tool with gray, runs the doctor, and
offers to install the background service.

The account can also come from `SIGNAL_ACCOUNT` instead of the config file.

## Running

```sh
gray signal status      # service health
gray signal stop        # graceful stop (posts shutdown notices)
gray signal restart
gray signal uninstall   # service only; config and sessions stay
gray signal run         # foreground daemon (what the service execs)
gray signal doctor      # verify daemon + local gray config
gray signal queue list / cancel <id>
gray signal allowlist add|remove|list
gray signal pairing approve signal <CODE>
gray signal limits --timeout-seconds 600 --concurrency 2 --max-requests 200
```

The service probes what this box actually supervises with (runit,
`systemd --user`, or a detached pidfile daemon when no init is available).

## Talking to it

- DM the bot's number, or mention it in an allowlisted group:
  `@+15551234567 your message`.
- `/new` or `/reset` starts a fresh session for that chat.
- `/steer <text>` adds guidance to the turn already running in that conversation ("Steering the current turn."); with nothing running it is sent as a new message.
- `/stop` (or `!stop`) cancels the turn running in that conversation ("Stopped."); queued messages still run next. With nothing running it answers "Nothing running."
- Messages sent while a turn runs queue as their own next turn ("Queued — N ahead.", at most once per 30s). Set `busy_input_mode: "steer"` in the config to steer the running turn instead.
- Long command output is paged: page 1 ends with `page 1/N — /page 2`; `/page N` shows page N of the conversation's last paged output.
- A DM from a number that is not allowlisted gets a one-shot pairing code;
  the owner approves it with `gray signal pairing approve signal <CODE>`.
- Groups are off unless `allowed_groups` lists the group id (or `*`); in a
  non-free group the bot only answers when mentioned.

## Tools (registered with gray)

- `signal_send {text, to?}` — markdown rendered to native Signal styles
  (bold, italic, strikethrough, monospace), chunked at 8,000 UTF-16 units;
  `to` defaults to `home_chat` and accepts a number, UUID, or `group:<id>`.

## Limits (v1)

- Text only outbound: no attachments, reactions, or edits. Inbound
  attachments are fetched (`getAttachment`, ≤25 MB) into the conversation
  home and surfaced to the agent as file paths.
- No backlog: envelopes that arrive while the socket is down are not
  replayed — signal-cli's `subscribeReceive` is live-only.
- Delivery is at-least-once: a dead daemon between agent finish and send
  retries the same answer rather than regenerating.
- One daemon per config (`gateway.lock`).
- `require_mention` (default true) gates groups; `allowed_numbers` gates
  every sender, DM or group.
