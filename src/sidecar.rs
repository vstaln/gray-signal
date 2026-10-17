//! NDJSON protocol 1.1 sidecar.
//! Manifest answers without config; delivery failures never echo secrets.
use serde_json::{json, Value};
use std::path::Path;
use std::sync::LazyLock;

/// Static manifest (`signal` 0.1.x, protocol 1.1, no commands).
pub static MANIFEST: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "name": "signal",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "requires_plugins": ["gateway"],
        "commands": [],
        "hooks": ["prompt/context"],
        "tools": [{
            "name": "signal_send",
            "label": "Signal Send",
            "description": "Send a message to a Signal number or group through the signal-cli daemon. Markdown is rendered as Signal text styles (bold, italic, strikethrough, monospace); long text is split into 8000-unit chunks. Text only — attachments are inbound-only.",
            "parameters": {
                "type": "object",
                "properties": {
                    "to": {"type": "string", "description": "+E164 number, UUID, or group:<id>; default: the configured home_chat"},
                    "text": {"type": "string", "description": "Message text (markdown rendered as Signal styles)"}
                },
                "required": ["text"]
            }
        }]
    })
});

const PROMPT_CONTEXT: &str = "Signal tools: signal_send posts text to a Signal number, UUID, or group:<id> (default: the configured home_chat). Markdown becomes native Signal styles (bold/italic/strikethrough/monospace); replies are chunked at 8000 UTF-16 units. Text only — outbound attachments are not supported, inbound attachments arrive as saved-file paths in the prompt. Never send secrets.";

fn is_error() -> Value {
    json!({
        "content": "Signal delivery failed. Run gray signal doctor; do not blindly retry partial sends.",
        "is_error": true
    })
}

/// A specific, fixable failure. `detail` must never contain config values.
fn not_sent(detail: impl std::fmt::Display) -> Value {
    json!({
        "content": format!("Not sent: {detail}. Nothing reached Signal; fix it and call again."),
        "is_error": true
    })
}

/// Dispatch one protocol method. Never echoes config/account text: failures
/// describe the caller's own input or a sanitized reason only.
pub async fn dispatch(method: &str, params: &Value, config_path: &Path) -> Value {
    if method == "plugin/manifest" {
        return MANIFEST.clone();
    }
    if method == "prompt/context" {
        return json!({"text": PROMPT_CONTEXT});
    }
    if method == "tool/call" {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        return match name {
            "signal_send" => send_text(params, config_path).await,
            _ => is_error(),
        };
    }
    json!({"error": "Unsupported method"})
}

async fn send_text(params: &Value, config_path: &Path) -> Value {
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    let text = args.get("text").and_then(Value::as_str).unwrap_or("");
    if text.trim().is_empty() {
        return not_sent("text is empty");
    }
    let config = match crate::config::load_config(config_path) {
        Ok(c) => c,
        Err(_) => return is_error(),
    };
    let fallback = crate::signal::cfg_str(&config, "home_chat").to_string();
    let target = args
        .get("to")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(&fallback)
        .to_string();
    if target.is_empty() {
        return not_sent("no target given and home_chat is not configured");
    }
    match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        crate::signal::standalone_send(&config, &target, text),
    )
    .await
    {
        Ok(Ok(msg)) => json!({"content": msg}),
        Ok(Err(e)) => not_sent(e),
        Err(_) => is_error(),
    }
}

/// The sidecar loop: one JSON request per stdin line, one `{"id", "result"}`
/// reply per line on stdout. `plugin/shutdown` and EOF exit.
pub fn serve(config_path: &Path) -> ! {
    use std::io::{BufRead, Write};
    const LIMIT: usize = 256 * 1024;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("sidecar runtime builds");
    loop {
        let mut buf: Vec<u8> = Vec::new();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(_) => std::process::exit(1),
        };
        if n == 0 {
            std::process::exit(0);
        }
        if buf.len() > LIMIT {
            std::process::exit(1);
        }
        let text = match String::from_utf8(buf) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !req.is_object() {
            continue;
        }
        if req.get("method").and_then(Value::as_str) == Some("plugin/shutdown") {
            std::process::exit(0);
        }
        let id = match req.get("id") {
            Some(Value::Number(_)) => req.get("id").cloned().unwrap(),
            _ => continue,
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = match req.get("params") {
            Some(Value::Object(_)) => req.get("params").cloned().unwrap(),
            _ => json!({}),
        };
        let result = rt.block_on(dispatch(method, &params, config_path));
        let row = json!({"id": id, "result": result});
        if writeln!(out, "{row}").is_err() {
            std::process::exit(1);
        }
        if out.flush().is_err() {
            std::process::exit(1);
        }
    }
}
