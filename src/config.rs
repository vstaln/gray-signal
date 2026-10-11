//! Private config load/save/validate.
//! Errors never contain config values.
use serde_json::Value;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Default config path: ~/.config/gray-signal/config.json
pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config/gray-signal/config.json")
}

/// Write JSON (pretty + trailing newline) atomically: parents 0700,
/// temp file + rename, replacement mode 0600.
pub fn atomic_json(path: &Path, data: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|_| "cannot create config directory".to_string())?;
        }
    }
    let mut text =
        serde_json::to_string_pretty(data).map_err(|_| "cannot encode config".to_string())?;
    text.push('\n');
    let tmp_path = path.with_extension("tmp");
    // Born 0600: nothing sensitive lives in this config, but sender numbers
    // and group ids are still private data.
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .map_err(|_| "cannot write config".to_string())?;
    }
    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))
        .map_err(|_| "cannot protect config".to_string())?;
    fs::rename(&tmp_path, path).map_err(|_| "cannot replace config".to_string())?;
    Ok(())
}

fn int_in(data: &Value, key: &str, low: u64, high: u64) -> Result<(), String> {
    if let Some(v) = data.get(key) {
        let ok = v.as_u64().is_some_and(|n| (low..=high).contains(&n));
        if !ok {
            return Err(format!("{key} must be an integer between {low} and {high}"));
        }
    }
    Ok(())
}

/// A phone number in E.164 form: `+` then 5-15 digits.
pub fn number_valid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 6
        && b.len() <= 16
        && b[0] == b'+'
        && b[1..].iter().all(|c| c.is_ascii_digit())
}

/// A Signal service id: a UUID, optionally `uuid:`-prefixed (ACI/PNI).
fn uuid_valid(s: &str) -> bool {
    let s = s.strip_prefix("uuid:").unwrap_or(s);
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                *c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// Who may talk to the bot or be a send target: a +E164 number or a
/// service-id UUID.
pub fn identity_valid(v: &Value) -> bool {
    match v.as_str() {
        Some(s) => (number_valid(s) || uuid_valid(s)) && s.len() <= 64,
        None => false,
    }
}

/// A group reference: `group:` prefix plus the daemon's base64-ish id.
/// `"*"` means every group when it appears in `allowed_groups`.
pub fn group_valid(v: &Value) -> bool {
    match v.as_str() {
        Some("*") => true,
        Some(s) => {
            let id = s.strip_prefix("group:").unwrap_or(s);
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'-' | b'_'))
        }
        None => false,
    }
}

/// A send target / home chat: a number, a UUID, or `group:<id>`.
pub fn target_valid(v: &Value) -> bool {
    match v.as_str() {
        Some(s) if s.starts_with("group:") => group_valid(v),
        Some(_) => identity_valid(v),
        None => false,
    }
}

/// The daemon host: hostname, IPv4, or bracket-free IPv6 — no spaces,
/// no scheme, no port embedded.
pub fn host_valid(v: &Value) -> bool {
    match v.as_str() {
        Some(s) => {
            !s.is_empty()
                && s.len() <= 255
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'_'))
        }
        None => false,
    }
}

/// Validate a config object. Error texts never echo config values.
pub fn validate_config(data: &Value) -> Result<(), String> {
    if !data.is_object() {
        return Err("Configuration must be an object".to_string());
    }
    // The account may live in SIGNAL_ACCOUNT instead of the file; the config
    // stands without it so an accountless file still parses.
    match data.get("account").and_then(Value::as_str) {
        Some(a) if number_valid(a) => {}
        Some(_) => return Err("account must be the bot's +E164 number".to_string()),
        None => {
            let ok = std::env::var("SIGNAL_ACCOUNT")
                .map(|t| number_valid(t.trim()))
                .unwrap_or(false);
            if !ok {
                return Err("account is missing; run setup".to_string());
            }
        }
    }
    if let Some(host) = data.get("daemon_host") {
        if !host_valid(host) {
            return Err("daemon_host must be a hostname or IP".to_string());
        }
    }
    int_in(data, "daemon_port", 1, 65535)?;
    // Inbound is allowlisted-only: the array may be empty (nobody admitted
    // yet — every DM then gets a pairing reply), but it must be an array.
    match data.get("allowed_numbers").and_then(Value::as_array) {
        Some(a) if a.iter().all(identity_valid) => {}
        _ => {
            return Err(
                "allowed_numbers must be an array of +E164 numbers or UUIDs".to_string(),
            )
        }
    }
    if let Some(groups) = data.get("allowed_groups") {
        let ok = groups.as_array().is_some_and(|a| a.iter().all(group_valid));
        if !ok {
            return Err("allowed_groups must be an array of group ids".to_string());
        }
    }
    if let Some(home) = data.get("home_chat") {
        if !target_valid(home) {
            return Err("home_chat must be a number, UUID, or group:<id>".to_string());
        }
    }
    if data
        .get("require_mention")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err("require_mention must be true or false".to_string());
    }
    for key in ["gray_bin", "gray_home", "workdir"] {
        let ok = data
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| Path::new(s).is_absolute());
        if !ok {
            return Err(format!("{key} must be an absolute path"));
        }
    }
    if data
        .get("restart_notification")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err("restart_notification must be true or false".to_string());
    }
    int_in(data, "timeout_seconds", 1, 86400)?;
    int_in(data, "concurrency", 1, 16)?;
    int_in(data, "max_requests", 1, 1000)?;
    int_in(data, "queue_capacity", 1, 100000)?;
    if data
        .get("busy_input_mode")
        .is_some_and(|v| !matches!(v.as_str(), Some("queue" | "steer")))
    {
        return Err("busy_input_mode must be queue or steer".to_string());
    }
    if let Some(policy) = data.get("session_reset") {
        if !policy.is_object() {
            return Err("session_reset must be an object".to_string());
        }
        if let Some(mode) = policy.get("mode") {
            let ok = mode
                .as_str()
                .is_some_and(|m| matches!(m, "none" | "idle" | "daily" | "both"));
            if !ok {
                return Err("session_reset.mode must be none, idle, daily, or both".to_string());
            }
        }
        if let Some(minutes) = policy.get("idle_minutes") {
            let ok = minutes.as_u64().is_some_and(|n| (1..=10_080).contains(&n));
            if !ok {
                return Err("session_reset.idle_minutes must be between 1 and 10080".to_string());
            }
        }
        if let Some(hour) = policy.get("at_hour") {
            let ok = hour.as_u64().is_some_and(|n| n <= 23);
            if !ok {
                return Err("session_reset.at_hour must be between 0 and 23".to_string());
            }
        }
    }
    Ok(())
}

/// Load + validate. Any IO/parse failure maps to the setup hint.
pub fn load_config(path: &Path) -> Result<Value, String> {
    let data: Value = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| "Configuration missing or invalid; run setup".to_string())?;
    validate_config(&data)?;
    Ok(data)
}

/// Validate + atomic save.
pub fn save_config(path: &Path, data: &Value) -> Result<(), String> {
    validate_config(data)?;
    atomic_json(path, data)
}
