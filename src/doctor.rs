//! Doctor preflight checks with safe errors.
//! Every failure is a category string; config values never surface.
use serde_json::Value;

/// Production entry point: checks `config` locally, then verifies the
/// daemon is reachable, answers `version`, and knows the account
/// (membership reported via listAccounts when the daemon offers it).
pub async fn doctor(config: &Value) -> Result<(), String> {
    let gray_bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
    if !is_executable(gray_bin) {
        return Err("gray executable is missing or not executable".to_string());
    }
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let provider: Value = std::fs::read(std::path::Path::new(gray_home).join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| "gray provider configuration is missing or invalid".to_string())?;
    if provider
        .get("model")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("gray has no configured model".to_string());
    }
    let account = crate::signal::account(config)
        .ok_or_else(|| "signal account is not configured".to_string())?;
    let rpc = crate::signal::Rpc::from_config(config)
        .await
        .map_err(|e| format!("signal-cli daemon unreachable: {e}"))?;
    rpc.version()
        .await
        .map_err(|e| format!("signal-cli did not answer: {e}"))?;
    match rpc.accounts().await {
        Ok(list) if !list.is_empty() && !list.iter().any(|a| a == &account) => {
            return Err(
                "signal-cli does not manage the configured account; register or link it"
                    .to_string(),
            );
        }
        Ok(_) | Err(_) => {}
    }
    println!("signal-cli connect, version check, and account presence verified.");
    println!("Subscribing, sending, and provider generation were not tested.");
    Ok(())
}

/// `os.access(path, X_OK)` equivalent: a real file with any exec bit set.
fn is_executable(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
