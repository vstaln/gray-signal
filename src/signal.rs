//! signal-cli daemon JSON-RPC 2.0 over TCP (`signal-cli daemon --tcp
//! host:port`). One connection carries requests both ways: outbound `send`,
//! `sendTyping`, `getAttachment` calls and the inbound `receive`
//! notifications `subscribeReceive` registers for. The Signal protocol
//! itself is never implemented here — signal-cli owns keys, groups and
//! delivery; this file is only the wire client.
//!
//! The socket splits into a reader task and a shared writer. Lines with an
//! `id` answer pending requests; notification lines push `params.envelope`
//! to the subscriber channel. A dead socket fails every pending call and
//! closes the envelope channel, which is how the gateway learns to
//! reconnect.
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

/// Signal's practical message ceiling (hermes MAX_MESSAGE_LENGTH). Chunks
/// are budgeted in UTF-16 units because `textStyles` offsets are.
pub const MAX_MESSAGE_CHARS: usize = 8000;

/// Largest attachment the daemon will fetch and pin to disk.
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum TransportError {
    /// Socket refused, reset, DNS failed — worth retrying.
    Net(String),
    /// The daemon answered with a JSON-RPC error object.
    Rpc(String),
    Timeout,
    /// The socket closed mid-request or the envelope stream ended.
    Closed,
    /// Caller's own input was rejected before the wire.
    Invalid(String),
}

impl TransportError {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Net(_) | Self::Timeout | Self::Closed
        )
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Net(d) => write!(f, "network: {d}"),
            Self::Rpc(d) => write!(f, "signal-cli error: {d}"),
            Self::Timeout => f.write_str("signal-cli call timed out"),
            Self::Closed => f.write_str("signal-cli connection closed"),
            Self::Invalid(d) => write!(f, "not sent: {d}"),
        }
    }
}
impl std::error::Error for TransportError {}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// One shared connection. `Rpc` is cheap to `Arc`: calls are serialized on
/// the writer while `next_envelope` drains the notification channel.
pub struct Rpc {
    writer: AsyncMutex<OwnedWriteHalf>,
    pending: Pending,
    next_id: AtomicU64,
    envelopes: AsyncMutex<mpsc::UnboundedReceiver<Value>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Rpc {
    /// Swapping in a fresh connection drops the old one: abort its reader
    /// so the socket fully closes and pending calls see `Closed`.
    fn drop(&mut self) {
        self.reader.abort();
        fail_pending(&self.pending);
    }
}

/// Where a reply goes: a single recipient (number or UUID) or a group id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Recipient(String),
    Group(String),
}

impl Target {
    /// `group:<id>` selects a group; anything else is a direct recipient.
    pub fn parse(raw: &str) -> Result<Target, TransportError> {
        let raw = raw.trim();
        if let Some(id) = raw.strip_prefix("group:") {
            if id.is_empty() {
                return Err(TransportError::Invalid("empty group id".to_string()));
            }
            return Ok(Target::Group(id.to_string()));
        }
        if !crate::config::identity_valid(&Value::String(raw.to_string())) {
            return Err(TransportError::Invalid(
                "target must be a +E164 number, a UUID, or group:<id>".to_string(),
            ));
        }
        Ok(Target::Recipient(raw.to_string()))
    }

    fn route(&self, params: &mut Value) {
        match self {
            Target::Recipient(n) => params["recipient"] = json!([n]),
            Target::Group(id) => params["groupId"] = json!(id),
        }
    }

    /// The `dm:`/`group:` conversation key the queue and replies share.
    pub fn conversation(&self) -> String {
        match self {
            Target::Recipient(n) => format!("dm:{n}"),
            Target::Group(id) => format!("group:{id}"),
        }
    }
}

/// Fail every in-flight call; the gateway sees `Closed` and reconnects.
fn fail_pending(pending: &Pending) {
    if let Ok(mut map) = pending.lock() {
        for (_, tx) in map.drain() {
            let _ = tx.send(Err("connection closed".to_string()));
        }
    }
}

async fn read_loop(
    reader: OwnedReadHalf,
    pending: Pending,
    envelopes: mpsc::UnboundedSender<Value>,
) {
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            _ => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            let tx = pending.lock().ok().and_then(|mut m| m.remove(&id));
            if let Some(tx) = tx {
                let result = if let Some(err) = msg.get("error") {
                    Err(err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("rpc error")
                        .to_string())
                } else {
                    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                };
                let _ = tx.send(result);
            }
            continue;
        }
        if msg.get("method").and_then(Value::as_str) == Some("receive") {
            if let Some(env) = msg.pointer("/params/envelope") {
                let _ = envelopes.send(env.clone());
            }
        }
    }
    fail_pending(&pending);
}

impl Rpc {
    /// Dial the daemon. Does not subscribe yet.
    pub async fn connect(host: &str, port: u16) -> Result<Rpc, TransportError> {
        let stream = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect((host, port)),
        )
        .await
        .map_err(|_| TransportError::Timeout)?
        .map_err(|e| TransportError::Net(e.to_string()))?;
        let _ = stream.set_nodelay(true);
        let (reader, writer) = stream.into_split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (env_tx, env_rx) = mpsc::unbounded_channel();
        let reader_task = tokio::spawn(read_loop(reader, pending.clone(), env_tx));
        Ok(Rpc {
            writer: AsyncMutex::new(writer),
            pending,
            next_id: AtomicU64::new(1),
            envelopes: AsyncMutex::new(env_rx),
            reader: reader_task,
        })
    }

    /// Config-driven connect (`daemon_host`/`daemon_port`, default
    /// 127.0.0.1:7583).
    pub async fn from_config(config: &Value) -> Result<Rpc, TransportError> {
        let host = cfg_str(config, "daemon_host");
        let host = if host.is_empty() { "127.0.0.1" } else { host };
        let port = config
            .get("daemon_port")
            .and_then(Value::as_u64)
            .unwrap_or(7583)
            .clamp(1, 65535) as u16;
        Self::connect(host, port).await
    }

    /// One JSON-RPC call. Errors never echo params (no secrets downstream).
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| TransportError::Closed)?
            .insert(id, tx);
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line = frame.to_string();
        line.push('\n');
        let wrote = {
            let mut w = self.writer.lock().await;
            w.write_all(line.as_bytes())
                .await
                .and_then(|_| std::io::Result::Ok(()))
        };
        if wrote.is_err() {
            self.pending.lock().ok().and_then(|mut m| m.remove(&id));
            return Err(TransportError::Closed);
        }
        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(_))) => Err(TransportError::Closed),
            Ok(Err(_)) => Err(TransportError::Closed),
            Err(_) => {
                self.pending.lock().ok().and_then(|mut m| m.remove(&id));
                Err(TransportError::Timeout)
            }
        }
    }

    /// Register this connection for `receive` notifications of `account`.
    pub async fn subscribe(&self, account: &str) -> Result<(), TransportError> {
        self.call("subscribeReceive", json!({"account": account}))
            .await
            .map(|_| ())
    }

    /// Next inbound envelope; `None` once the socket dies.
    pub async fn next_envelope(&self) -> Option<Value> {
        self.envelopes.lock().await.recv().await
    }

    /// `send` with optional `start:length:STYLE` ranges (UTF-16 offsets).
    pub async fn send_text(
        &self,
        account: &str,
        target: &Target,
        message: &str,
        styles: &[String],
    ) -> Result<Value, TransportError> {
        let mut params = json!({"account": account, "message": message});
        target.route(&mut params);
        if styles.len() == 1 {
            params["textStyle"] = json!(styles[0]);
        } else if !styles.is_empty() {
            params["textStyles"] = json!(styles);
        }
        let result = self.call("send", params).await?;
        // signal-cli reports per-recipient failures inside the result.
        if let Some(results) = result.get("results").and_then(Value::as_array) {
            for r in results {
                let rtype = r.get("type").and_then(Value::as_str).unwrap_or("");
                if !rtype.is_empty() && rtype != "SUCCESS" {
                    return Err(TransportError::Rpc(format!("delivery {rtype}")));
                }
                if r.get("success") == Some(&Value::Bool(false)) {
                    return Err(TransportError::Rpc("recipient delivery failed".to_string()));
                }
            }
        }
        Ok(result)
    }

    /// Typing indicator; `stop` clears it early. Best-effort by callers.
    pub async fn typing(&self, account: &str, target: &Target, stop: bool) {
        let mut params = json!({"account": account});
        target.route(&mut params);
        if stop {
            params["stop"] = json!(true);
        }
        let _ = self.call("sendTyping", params).await;
    }

    /// Fetch one inbound attachment's bytes.
    pub async fn get_attachment(
        &self,
        account: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>, TransportError> {
        let result = self
            .call(
                "getAttachment",
                json!({"account": account, "id": attachment_id}),
            )
            .await?;
        let b64 = result
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| TransportError::Rpc("attachment had no data".to_string()))?;
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| TransportError::Rpc("attachment data was not base64".to_string()))
    }

    /// `version` — the cheapest liveness check the daemon answers.
    pub async fn version(&self) -> Result<Value, TransportError> {
        self.call("version", json!({})).await
    }

    /// Accounts the daemon currently manages (empty in multi-account mode
    /// can mean "all of them"; a missing configured account is still
    /// reported by the caller).
    pub async fn accounts(&self) -> Result<Vec<String>, TransportError> {
        let result = self.call("listAccounts", json!({})).await?;
        Ok(result
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|acc| {
                        acc.get("number")
                            .or_else(|| acc.get("account"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .or_else(|| acc.as_str().map(str::to_string))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
}

/// The bot's own number: `account` config key first, `SIGNAL_ACCOUNT` env
/// second. Never logged.
pub fn account(config: &Value) -> Option<String> {
    config
        .get("account")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("SIGNAL_ACCOUNT")
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
        })
}

/// Config string helper: empty for missing/null so call sites stay flat.
pub fn cfg_str<'a>(config: &'a Value, key: &str) -> &'a str {
    config.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Outbound path for the `signal_send` sidecar tool: connect, convert,
/// chunk, send, drop. Failures name the caller's input or a category.
pub async fn standalone_send(
    config: &Value,
    target_raw: &str,
    text: &str,
) -> Result<String, TransportError> {
    let account = account(config)
        .ok_or_else(|| TransportError::Invalid("account is not configured".to_string()))?;
    let target = Target::parse(target_raw)?;
    let rpc = Rpc::from_config(config).await?;
    let chunks = crate::text::split_signal_message(text, MAX_MESSAGE_CHARS);
    for (plain, styles) in &chunks {
        rpc.send_text(&account, &target, plain, styles).await?;
    }
    Ok(format!(
        "Sent {} message(s) to {}.",
        chunks.len(),
        target_raw
    ))
}
