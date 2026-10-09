//! Signal-side pairing. A user who DMs a bot that does not admit them is
//! told their own number/UUID and a one-shot code; the owner approves the
//! code with `gray signal pairing approve signal <code>` and that identity
//! joins the allowlist. No terminal wizard, no 5-minute timer, nothing to
//! hunt — the discovery happens where the user already is.

use serde_json::Value;
use std::path::Path;

/// A fresh code: 8 uppercase alphanumerics (short enough
/// to read off a phone, 36^8 space, single-use).
pub fn gen_code() -> String {
    const ALPHABET: &[u8; 36] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut bytes = [0u8; 8];
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(&mut bytes);
        }
    }
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

/// The reply an un-admitted DMer sees. Signal numbers and service ids are
/// account-controlled — whoever holds the keys controls the identity.
pub fn unconfigured_reply(identity: &str, code: &str) -> String {
    format!(
        "access not configured.\n\
         Your Signal ID: {identity}\n\
         Pairing code: {code}\n\
         Ask the bot owner to approve with:\n\
         gray signal pairing approve signal {code}"
    )
}

/// What the gateway does when a message is not admitted and the sender is a
/// human DMing the bot: mint (or reuse) a code and reply. Group messages
/// and admitted senders never see this — `None` for them.
pub struct PairingReply {
    pub text: String,
    #[allow(dead_code)]
    pub code: String,
}

pub fn reply_for(store: &crate::store::Store, identity: &str, dm: bool) -> Option<PairingReply> {
    if !dm || identity.is_empty() {
        return None;
    }
    let code = store
        .pairing_code_for(identity)
        .ok()
        .flatten()
        .unwrap_or_else(gen_code);
    store.pairing_insert(&code, identity).ok()?;
    Some(PairingReply {
        text: unconfigured_reply(identity, &code),
        code,
    })
}

/// Consume a code and admit its identity into `allowed_numbers`. Config is
/// rewritten privately (0600) exactly as the wizard would.
pub fn approve(config_path: &Path, platform: &str, code: &str) -> Result<String, String> {
    if platform != "signal" {
        return Err(format!(
            "unknown platform '{platform}'; the only one is signal"
        ));
    }
    let store = crate::gateway::open_store(config_path)?;
    let identity = store
        .pairing_take(code.trim())?
        .ok_or_else(|| "unknown or already-used pairing code".to_string())?;
    if !crate::config::identity_valid(&Value::String(identity.clone())) {
        return Err("pairing code did not resolve to a Signal number or UUID".to_string());
    }
    let mut config = crate::config::load_config(config_path)?;
    let mut allowed: Vec<String> = config
        .get("allowed_numbers")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if allowed.iter().any(|u| u == &identity) {
        return Ok(format!("{identity} was already allowed"));
    }
    allowed.push(identity.clone());
    config["allowed_numbers"] =
        Value::Array(allowed.into_iter().map(Value::String).collect());
    crate::config::save_config(config_path, &config)?;
    Ok(format!(
        "{identity} can now trigger the bot; restart the gateway to apply (it re-reads on start)"
    ))
}
