//! The Signal gateway: signal-cli `receive` notifications -> durable inbox
//! -> isolated gray turns -> `send` replies back out. The daemon is a thin
//! TCP client: signal-cli owns the Signal protocol, retries and group
//! state; this file owns admission, queuing and turn plumbing.
//!
//! Envelopes arriving while the socket is down never reach us — unlike a
//! sync cursor there is no backlog to replay. `sig:` queue ids still
//! dedupe a redelivery within one connection, and `recover` picks up turns
//! that were running when the process stopped.
//!
//! Admission mirrors the other bridges: our own account never turns; the
//! sender's number/UUID must be on `allowed_numbers`; group messages need
//! the group on `allowed_groups` and (with `require_mention`) a mention of
//! the bot. An un-admitted DMer gets a pairing reply instead of silence.
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use crate::runner::RunError;
use crate::signal::{Rpc, Target, TransportError};
use crate::store::Store;

/// The queue file next to the config.
pub fn open_store(config_path: &Path) -> Result<Store, String> {
    Store::new(&Store::default_path(config_path))
}

/// Reconnect backoff after a socket failure: 1s doubling to a minute.
fn poll_backoff(attempt: u64) -> Duration {
    let secs = (1u64 << attempt.min(6)).min(60);
    Duration::from_secs(secs)
}

fn log_failure(what: &str, target: &str, error: &TransportError) {
    let detail = match error {
        TransportError::Net(detail) => format!("network: {detail}"),
        TransportError::Rpc(detail) => format!("rpc: {detail}"),
        TransportError::Timeout => "timed out".to_string(),
        TransportError::Closed => "connection closed".to_string(),
        TransportError::Invalid(detail) => format!("not sent: {detail}"),
    };
    eprintln!("[signal] {what} failed for {target}: {detail}");
}

struct Daemon {
    config: Value,
    config_path: PathBuf,
    store: Store,
    /// The live daemon connection; swapped wholesale on reconnect.
    rpc: RwLock<Rpc>,
    /// Our own number — self-sent envelopes never turn.
    own_account: String,
    /// Allowlisted senders (numbers or UUIDs).
    allowed: HashSet<String>,
    /// `group:<id>` groups that may turn; `*` admits all.
    groups: HashSet<String>,
    /// `group:<id>` groups that need no mention.
    free_groups: HashSet<String>,
    /// The chat lifecycle notices go to.
    home_chat: String,
    /// Observed number<->UUID pairs (both directions).
    aliases: std::sync::RwLock<HashMap<String, String>>,
    /// `gray cmd list` outcome, TTL'd; see commands.rs.
    commands: std::sync::RwLock<crate::commands::ListCache>,
}

/// A parsed inbound message: everything `on_envelope` needs in one place.
struct Inbound {
    /// Number or UUID the reply goes to for a DM.
    sender: String,
    /// `group:<id>` for group chats, the sender for DMs.
    target: String,
    conversation: String,
    /// Prompt text (message body plus quote context).
    text: String,
    /// `sig:` dedupe id.
    event_id: String,
    is_group: bool,
    is_admitted: bool,
    /// Raw attachment objects; fetched only after admission.
    attachments: Vec<Value>,
}

impl Daemon {
    /// `require_mention` config flag; default true (hermes semantics).
    fn require_mention(&self) -> bool {
        self.config
            .get("require_mention")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    /// The sender may trigger the bot: their number, their UUID, or a
    /// `uuid:`-prefixed alias is on the allowlist. Aliases learned from
    /// envelopes carrying both forms count too.
    fn user_admitted(&self, sender: &str) -> bool {
        if sender.is_empty() {
            return false;
        }
        let hit = |id: &str| {
            self.allowed.contains(id)
                || id
                    .strip_prefix("uuid:")
                    .is_some_and(|u| self.allowed.contains(u))
        };
        if hit(sender) {
            return true;
        }
        self.aliases
            .read()
            .ok()
            .and_then(|map| map.get(sender).cloned())
            .is_some_and(|alias| hit(alias.as_str()))
    }

    /// A note-to-self / linked-device send: returns the `sentMessage`,
    /// which plays the dataMessage role downstream. None for sync
    /// envelopes that are not ours to handle.
    fn unwrap_sync<'a>(&self, envelope: &'a Value) -> Option<&'a Value> {
        let sent = envelope.get("syncMessage")?.get("sentMessage")?;
        let dest = sent.get("destination").and_then(Value::as_str);
        let group = sent.pointer("/groupInfo/groupId").is_some();
        if dest == Some(self.own_account.as_str()) || (dest.is_none() && group) {
            return Some(sent);
        }
        None
    }

    /// Learn number<->UUID pairs from envelopes that carry both, so an
    /// allowlisted number still admits its UUID form (and vice versa).
    fn remember_alias(&self, envelope: &Value) {
        let number = envelope.get("sourceNumber").and_then(Value::as_str);
        let uuid = envelope.get("sourceUuid").and_then(Value::as_str);
        if let (Some(n), Some(u)) = (number, uuid) {
            if let Ok(mut map) = self.aliases.write() {
                map.insert(n.to_string(), u.to_string());
                map.insert(u.to_string(), n.to_string());
            }
        }
    }

    /// Shape one envelope into an Inbound, or None to drop it silently.
    fn parse(&self, envelope: &Value) -> Option<Inbound> {
        self.remember_alias(envelope);
        let note_to_self = envelope.get("syncMessage").is_some();
        let sync_sent = if note_to_self {
            Some(self.unwrap_sync(envelope)?)
        } else {
            None
        };
        let sender = envelope
            .get("sourceNumber")
            .or_else(|| envelope.get("sourceUuid"))
            .or_else(|| envelope.get("source"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if sender.is_empty() {
            return None;
        }
        // Our own echo never turns — except note-to-self, which is the
        // owner talking to their own account.
        if sender == self.own_account && !note_to_self {
            return None;
        }
        if envelope.get("storyMessage").is_some() {
            return None;
        }
        let data = match sync_sent {
            Some(sent) => sent,
            None => envelope
                .get("dataMessage")
                .or_else(|| envelope.pointer("/editMessage/dataMessage"))?,
        };
        let group_id = data
            .pointer("/groupInfo/groupId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let is_group = group_id.is_some();
        let target = match &group_id {
            Some(id) => format!("group:{id}"),
            None => sender.clone(),
        };
        if is_group && !self.groups.contains("*") && !self.groups.contains(&target) {
            return None;
        }
        let mut text = data
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if is_group && self.require_mention() && !self.free_groups.contains(&target) {
            let mentioned = data
                .get("mentions")
                .and_then(Value::as_array)
                .map(|ms| {
                    ms.iter().any(|m| {
                        m.get("number").and_then(Value::as_str) == Some(&self.own_account)
                            || m.get("uuid").and_then(Value::as_str) == Some(&self.own_account)
                    })
                })
                .unwrap_or(false)
                || text.contains(&format!("@{}", self.own_account));
            if !mentioned {
                return None;
            }
            text = text.replace(&format!("@{}", self.own_account), "");
        }
        let timestamp = envelope
            .get("timestamp")
            .and_then(Value::as_u64)
            .or_else(|| data.get("timestamp").and_then(Value::as_u64))
            .unwrap_or_default();
        let attachments: Vec<Value> = data
            .get("attachments")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut lines: Vec<String> = Vec::new();
        if let Some(quote_text) = data.pointer("/quote/text").and_then(Value::as_str) {
            if !quote_text.trim().is_empty() {
                let author = data
                    .pointer("/quote/author")
                    .and_then(Value::as_str)
                    .unwrap_or("someone");
                lines.push(format!("[in reply to {author}: {}]", quote_text.trim()));
            }
        }
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            lines.push(trimmed.to_string());
        }
        // A dataMessage can be an empty wrapper (key updates, deletes) —
        // no prompt and nothing attached means no turn.
        if lines.is_empty() && attachments.is_empty() {
            return None;
        }
        let event_id = if timestamp > 0 {
            format!("sig:{timestamp}:{sender}")
        } else {
            format!("sig:{}", crate::store::uuid_hex())
        };
        let conversation =
            crate::session::conversation_key(group_id.as_deref().unwrap_or(""), &sender, !is_group);
        Some(Inbound {
            sender: sender.clone(),
            is_admitted: self.user_admitted(&sender),
            target,
            conversation,
            text: lines.join("\n"),
            event_id,
            is_group,
            attachments,
        })
    }

    /// Fetch one attachment into the conversation home; returns the prompt
    /// line describing it. Failures degrade to a bracketed note, never a
    /// dropped turn. Only called for admitted senders.
    async fn attachment_line(&self, home: &Path, att: &Value, index: usize) -> Option<String> {
        let id = att.get("id").and_then(Value::as_str)?;
        let size = att.get("size").and_then(Value::as_u64).unwrap_or(0);
        let ctype = att
            .get("contentType")
            .and_then(Value::as_str)
            .unwrap_or("file");
        let name = att
            .get("fileName")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("attachment-{index}"));
        if size > crate::signal::MAX_ATTACHMENT_BYTES {
            return Some(format!(
                "[attachment skipped: {name} ({ctype}) is too large]"
            ));
        }
        let rpc = self.rpc.read().await;
        let bytes = match rpc.get_attachment(&self.own_account, id).await {
            Ok(b) if b.len() as u64 <= crate::signal::MAX_ATTACHMENT_BYTES => b,
            Ok(_) => {
                return Some(format!(
                    "[attachment skipped: {name} ({ctype}) is too large]"
                ))
            }
            Err(e) => {
                eprintln!("[signal] attachment fetch failed: {e}");
                return Some(format!("[attachment unavailable: {name} ({ctype})]"));
            }
        };
        drop(rpc);
        let dir = home.join("attachments");
        if std::fs::create_dir_all(&dir).is_err() {
            return Some(format!("[attachment unsaved: {name} ({ctype})]"));
        }
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                    c
                } else {
                    '_'
                }
            })
            .take(80)
            .collect();
        let path = dir.join(format!("{}_{safe}", crate::store::uuid_hex()));
        match std::fs::write(&path, &bytes) {
            Ok(()) => Some(format!(
                "[attachment saved: {} ({ctype})]",
                path.to_string_lossy()
            )),
            Err(_) => Some(format!("[attachment unsaved: {name} ({ctype})]")),
        }
    }

    /// Send markdown to a target, chunked and styled. Log-only failures.
    async fn reply(&self, target_raw: &str, text: &str, what: &str) {
        let Ok(target) = Target::parse(target_raw) else {
            return;
        };
        let rpc = self.rpc.read().await;
        for (plain, styles) in
            crate::text::split_signal_message(text, crate::signal::MAX_MESSAGE_CHARS)
        {
            if let Err(e) = rpc
                .send_text(&self.own_account, &target, &plain, &styles)
                .await
            {
                log_failure(what, target_raw, &e);
                return;
            }
        }
    }

    /// Is `name` a real command? The cached registry answers; a stale or
    /// absent cache costs one `cmd list`, a dead one the static set.
    async fn command_known(&self, name: &str, home: &Path) -> bool {
        let now = crate::store::now_secs();
        let cached = self.commands.read().ok().and_then(|c| {
            (now - c.fetched_at < crate::commands::LIST_TTL_SECS).then(|| c.names.clone())
        });
        let names = match cached {
            Some(names) => names,
            None => {
                let fetched = crate::commands::list_names(&self.config, home).await;
                if let Ok(mut c) = self.commands.write() {
                    *c = crate::commands::ListCache {
                        fetched_at: now,
                        names: fetched.as_ref().map(|f| f.0.clone()),
                        slow: fetched.as_ref().map(|f| f.1.clone()).unwrap_or_default(),
                    };
                }
                fetched.map(|f| f.0)
            }
        };
        crate::commands::known(&names, name)
    }

    /// Whether the cached registry marks `name` slow.
    fn command_slow(&self, name: &str) -> bool {
        self.commands
            .read()
            .ok()
            .map(|c| c.slow.contains(name))
            .unwrap_or(false)
    }

    /// Run the message as a `!`/`/` command when it names a real one.
    /// `Some` consumed the message; `None` passes it on to the normal
    /// prompt path.
    async fn command_reply(&self, msg: &Inbound) -> Option<crate::commands::Outcome> {
        let (name, args) = crate::commands::parse_prefixed(&msg.text)?;
        let home = crate::session::conversation_home(&self.config_path, &msg.conversation);
        if !self.command_known(&name, &home).await {
            return None;
        }
        let slow = self.command_slow(&name);
        Some(
            crate::commands::run(&self.config, &home, &name, &args, slow)
                .await
                .unwrap_or_else(crate::commands::Outcome::Text),
        )
    }

    /// One inbound envelope: admission, commands, attachments, enqueue.
    async fn on_envelope(&self, envelope: &Value) {
        let Some(msg) = self.parse(envelope) else {
            return;
        };
        if !msg.is_admitted {
            if !msg.is_group {
                // Only a DM hears how to get in; group strangers get silence.
                if let Some(reply) = crate::pairing::reply_for(&self.store, &msg.sender, true) {
                    self.reply(&msg.sender, &reply.text, "pairing reply").await;
                }
            }
            return;
        }

        // Bare commands never reach the model.
        if matches!(msg.text.trim(), "/new" | "/reset") {
            let home = crate::session::conversation_home(&self.config_path, &msg.conversation);
            let note = match crate::session::reset_home(&home, crate::store::now_secs()) {
                Ok(()) => "Session reset — the next message starts fresh.",
                Err(_) => "Could not reset this session.",
            };
            self.reply(&msg.target, note, "reset reply").await;
            return;
        }

        // Known `!`/`/` commands run through `gray cmd`; text answers
        // inline, prompts enqueue as ordinary turns.
        if let Some(outcome) = self.command_reply(&msg).await {
            match outcome {
                crate::commands::Outcome::Text(reply) => {
                    self.reply(&msg.target, &reply, "command reply").await;
                }
                crate::commands::Outcome::Prompt { prompt, fallback } => {
                    let capacity = self
                        .config
                        .get("queue_capacity")
                        .and_then(Value::as_u64)
                        .unwrap_or(1000);
                    let id = format!("{}:cmd", msg.event_id);
                    match self.store.enqueue(
                        &id,
                        &msg.target,
                        &msg.sender,
                        &msg.conversation,
                        &prompt,
                        capacity,
                    ) {
                        Ok(_) => {
                            if !fallback.trim().is_empty() {
                                self.reply(&msg.target, fallback.trim(), "command reply")
                                    .await;
                            }
                        }
                        Err(e) => {
                            self.reply(&msg.target, "Queue full; try again later.", "queue notice")
                                .await;
                            eprintln!("[signal] command turn enqueue failed: {e}");
                        }
                    }
                }
            }
            return;
        }

        // Attachments fetch only now: an unadmitted sender cannot make the
        // bot touch the attachment store.
        let mut prompt = msg.text.clone();
        if !msg.attachments.is_empty() {
            let home = crate::session::conversation_home(&self.config_path, &msg.conversation);
            for (i, att) in msg.attachments.iter().enumerate() {
                if let Some(line) = self.attachment_line(&home, att, i).await {
                    prompt = if prompt.is_empty() {
                        line
                    } else {
                        format!("{prompt}\n{line}")
                    };
                }
            }
        }

        let capacity = self
            .config
            .get("queue_capacity")
            .and_then(Value::as_u64)
            .unwrap_or(1000);
        match self.store.enqueue(
            &msg.event_id,
            &msg.target,
            &msg.sender,
            &msg.conversation,
            &prompt,
            capacity,
        ) {
            Ok(true) => eprintln!("[signal] queued {} from {}", msg.event_id, msg.sender),
            Ok(false) => {} // redelivered envelope — already queued or done
            Err(e) => {
                self.reply(&msg.target, "Queue full; try again later.", "queue notice")
                    .await;
                eprintln!("[signal] enqueue failed: {e}");
            }
        }
    }

    /// One claimed inbox item end to end. A failed send re-queues; a
    /// generated answer retries delivery instead of regenerating.
    async fn work(&self) -> Result<bool, String> {
        let Some(item) = self.store.claim()? else {
            return Ok(false);
        };
        let Ok(target) = Target::parse(&item.target) else {
            self.store.fail(&item.id, "unroutable target")?;
            return Ok(true);
        };
        // Typing indicator is best-effort; a dead daemon must not stall
        // the turn.
        {
            let rpc = self.rpc.read().await;
            rpc.typing(&self.own_account, &target, false).await;
        }
        let outcome = crate::runner::run_gray(
            &self.config,
            &self.config_path,
            &item.conversation,
            &item.text,
            None,
        )
        .await;
        {
            let rpc = self.rpc.read().await;
            rpc.typing(&self.own_account, &target, true).await;
        }
        match outcome {
            Ok(answer) => {
                let chunks =
                    crate::text::split_signal_message(&answer, crate::signal::MAX_MESSAGE_CHARS);
                let mut attempts = 0u32;
                let mut sent = 0usize;
                'deliver: loop {
                    let rpc = self.rpc.read().await;
                    for (plain, styles) in chunks.iter().skip(sent) {
                        match rpc
                            .send_text(&self.own_account, &target, plain, styles)
                            .await
                        {
                            Ok(_) => sent += 1,
                            Err(e) if e.is_retryable() && attempts < 5 => {
                                attempts += 1;
                                drop(rpc);
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                continue 'deliver;
                            }
                            Err(e) => {
                                log_failure("delivery", &item.target, &e);
                                if sent == 0 {
                                    self.store.fail(&item.id, "delivery failed")?;
                                } else {
                                    self.store.fail(&item.id, "delivery partially failed")?;
                                }
                                break 'deliver;
                            }
                        }
                    }
                    if sent == chunks.len() {
                        self.store.complete(&item.id, &answer)?;
                    }
                    break 'deliver;
                }
            }
            Err(RunError::Busy) => {
                self.store.requeue(&item.id)?;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                self.reply(&item.target, &e.to_string(), "error reply")
                    .await;
                self.store.fail(&item.id, &e.to_string())?;
            }
        }
        Ok(true)
    }
}

fn config_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn string_set(config: &Value, key: &str) -> HashSet<String> {
    config
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Normalize a configured group entry to the `group:<id>` target form.
fn group_set(config: &Value, key: &str) -> HashSet<String> {
    string_set(config, key)
        .into_iter()
        .map(|g| {
            if g == "*" || g.starts_with("group:") {
                g
            } else {
                format!("group:{g}")
            }
        })
        .collect()
}

/// The daemon: lock + pidfile, workers, subscribe loop, graceful stop.
pub async fn run(config_path: &Path) -> Result<(), String> {
    let dir = config_dir(config_path);
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|_| "cannot create config directory".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    // daemon.json: `status` and lifecycle read it; removed on every exit.
    let pid_path = dir.join("daemon.json");
    {
        let stamp = serde_json::json!({
            "pid": std::process::id(),
            "started_at": crate::store::now_secs()
        });
        let _ = crate::config::atomic_json(&pid_path, &stamp);
    }
    struct RemovePid(PathBuf);
    impl Drop for RemovePid {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove_pid = RemovePid(pid_path);

    // One daemon per config: two would fight over the same subscription.
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gateway.lock"))
        .map_err(|_| "cannot open gateway lock".to_string())?;
    use std::os::unix::io::AsRawFd;
    let locked = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !locked {
        return Err("Another gateway is running".to_string());
    }
    let _lock = lock_file;

    let config = crate::config::load_config(config_path)?;
    let own_account = crate::signal::account(&config).ok_or("signal account is missing")?;
    let store = open_store(config_path)?;
    let interrupted = store.recover().unwrap_or(0);

    let allowed = string_set(&config, "allowed_numbers");
    let groups = group_set(&config, "allowed_groups");
    let free_groups = group_set(&config, "free_response_groups");
    if allowed.is_empty() {
        eprintln!("[signal] warning: nobody is allowlisted; every DM gets a pairing reply");
    }

    // Prove the daemon before the first subscribe: a dead socket is a
    // setup problem, not a connectivity one.
    let rpc = Rpc::from_config(&config)
        .await
        .map_err(|_| "cannot reach the signal-cli daemon; start it or run setup".to_string())?;

    let daemon = Arc::new(Daemon {
        config: config.clone(),
        config_path: config_path.to_path_buf(),
        store,
        rpc: RwLock::new(rpc),
        own_account,
        allowed,
        groups,
        free_groups,
        home_chat: crate::signal::cfg_str(&config, "home_chat").to_string(),
        aliases: std::sync::RwLock::new(HashMap::new()),
        commands: std::sync::RwLock::new(crate::commands::ListCache::default()),
    });

    // Boot notice is computed now (it records lifecycle state) and posted
    // after the first successful subscribe.
    let gray_bin = Path::new(crate::signal::cfg_str(&daemon.config, "gray_bin"));
    let mut notice = if crate::lifecycle::enabled(&daemon.config) {
        crate::lifecycle::boot_via(gray_bin, &dir, interrupted).await
    } else {
        let _ = crate::lifecycle::boot(&dir);
        None
    };

    // Workers drain the inbox; the receive loop only ever enqueues.
    let workers = daemon
        .config
        .get("concurrency")
        .and_then(Value::as_u64)
        .unwrap_or(2)
        .clamp(1, 16);
    let stop = Arc::new(tokio::sync::Notify::new());
    let mut handles = Vec::new();
    for _ in 0..workers {
        let d = daemon.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            loop {
                match d.work().await {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => eprintln!("[signal] worker error: {e}"),
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                    _ = stop.notified() => break,
                }
            }
        }));
    }

    let shutdown = Arc::new(tokio::sync::Notify::new());
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => {},
                _ = int.recv() => {},
            }
            shutdown.notify_waiters();
        });
    }

    // The receive loop. Phase 1 keeps a subscribed connection alive
    // (reconnect + resubscribe with backoff); phase 2 drains envelopes
    // until the socket dies, then loops back.
    let mut backoff_attempt: u64 = 0;
    let stop_reason: Result<(), String> = 'outer: loop {
        loop {
            let subscribed = {
                let rpc = daemon.rpc.read().await;
                rpc.subscribe(&daemon.own_account).await.is_ok()
            };
            if subscribed {
                backoff_attempt = 0;
                if let Some(text) = notice.take() {
                    if !daemon.home_chat.is_empty() {
                        daemon
                            .reply(&daemon.home_chat.clone(), &text, "boot notice")
                            .await;
                    }
                }
                break;
            }
            match Rpc::from_config(&daemon.config).await {
                Ok(fresh) => {
                    *daemon.rpc.write().await = fresh;
                }
                Err(e) => {
                    let wait = poll_backoff(backoff_attempt);
                    backoff_attempt = backoff_attempt.saturating_add(1);
                    eprintln!(
                        "[signal] connect failed: {e}; retrying in {}s",
                        wait.as_secs()
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {},
                        _ = shutdown.notified() => break 'outer Ok(()),
                    }
                }
            }
        }
        eprintln!("[signal] subscribed to receive notifications");
        loop {
            let env = tokio::select! {
                env = async {
                    let rpc = daemon.rpc.read().await;
                    rpc.next_envelope().await
                } => env,
                _ = shutdown.notified() => break 'outer Ok(()),
            };
            match env {
                Some(envelope) => {
                    backoff_attempt = 0;
                    daemon.on_envelope(&envelope).await;
                }
                None => {
                    eprintln!("[signal] connection lost; resubscribing");
                    break;
                }
            }
        }
    };

    // Goodbye notices are plain sends, bounded.
    let gray_bin = Path::new(crate::signal::cfg_str(&daemon.config, "gray_bin"));
    let goodbye = crate::lifecycle::stop_via(gray_bin, &dir).await;
    if crate::lifecycle::enabled(&daemon.config) {
        let running: Vec<String> = daemon
            .store
            .running_targets()
            .unwrap_or_default()
            .into_iter()
            .map(|i| i.target)
            .collect();
        let targets = crate::lifecycle::shutdown_targets(&running, &daemon.home_chat, &goodbye);
        let send = async {
            for (target, text) in targets {
                daemon.reply(&target, text, "shutdown notice").await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(6), send).await;
    }

    eprintln!("[signal] shutting down");
    stop.notify_waiters();
    for h in handles {
        h.abort();
    }
    // If shutdown never ran inside the loop (early failure path), still
    // record a clean exit so the next boot does not report a crash.
    if std::fs::read(dir.join(crate::lifecycle::STATE_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|s| s.get("state").and_then(Value::as_str).map(str::to_string))
        .as_deref()
        == Some("running")
    {
        let restart = crate::lifecycle::restart_requested(&dir);
        crate::lifecycle::mark_stopped(&dir, restart);
    }
    stop_reason
}
