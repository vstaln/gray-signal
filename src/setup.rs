//! Interactive setup wizard.
//! Nothing secret is asked for: the account lives in signal-cli, not here.
use serde_json::{json, Value};
use std::path::Path;

/// Terminal IO. The real implementation talks to stdin/stderr; tests inject
/// a scripted fake. `print_line` goes to user-visible output (never logs).
pub trait Prompter {
    fn is_terminal(&self) -> bool;
    fn prompt(&mut self, text: &str) -> Result<String, String>;
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String>;
    fn confirm(&mut self, text: &str) -> Result<bool, String>;
    fn print_line(&mut self, text: &str);
}

/// Real terminal prompter (hidden input via `stty -echo`, no new deps).
pub struct Tty;

impl Prompter for Tty {
    fn is_terminal(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    fn prompt(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let res = without_echo(|| std::io::stdin().read_line(&mut line));
        let _ = std::io::stderr().write_all(b"\n");
        res.map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim().to_string())
    }
    fn confirm(&mut self, text: &str) -> Result<bool, String> {
        let answer = self.prompt(text)?.trim().to_ascii_lowercase();
        Ok(answer == "y" || answer == "yes")
    }
    fn print_line(&mut self, text: &str) {
        eprintln!("{text}");
    }
}

/// Echo off for one read. Ctrl-C would otherwise kill setup with the
/// terminal still silent, so SIGINT restores it first (tcsetattr is
/// async-signal-safe).
#[cfg(unix)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    static SAVED: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();
    extern "C" fn restore_and_exit(_: libc::c_int) {
        if let Some(t) = SAVED.get() {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
        unsafe { libc::_exit(130) };
    }
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut term) } != 0 {
        return read();
    }
    let saved = *SAVED.get_or_init(|| term);
    let mut quiet = saved;
    quiet.c_lflag &= !libc::ECHO;
    let handler = restore_and_exit as extern "C" fn(libc::c_int) as libc::sighandler_t;
    let old = unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &quiet);
        libc::signal(libc::SIGINT, handler)
    };
    let out = read();
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &saved);
        libc::signal(libc::SIGINT, old);
    }
    out
}

/// Windows echo off for one read: clear ENABLE_ECHO_INPUT on the console
/// input buffer. A console control handler puts the mode back before the
/// default Ctrl-C processing kills setup, same shape as the unix version.
#[cfg(windows)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    use std::ffi::c_void;
    type Bool = i32;
    type Dword = u32;
    type Handle = *mut c_void;
    const STD_INPUT_HANDLE: Dword = -10i32 as Dword;
    const ENABLE_ECHO_INPUT: Dword = 0x0004;
    extern "system" {
        fn GetStdHandle(which: Dword) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut Dword) -> Bool;
        fn SetConsoleMode(handle: Handle, mode: Dword) -> Bool;
        fn SetConsoleCtrlHandler(
            handler: Option<extern "system" fn(Dword) -> Bool>,
            add: Bool,
        ) -> Bool;
    }
    static SAVED: std::sync::OnceLock<Dword> = std::sync::OnceLock::new();
    extern "system" fn restore(_: Dword) -> Bool {
        if let Some(&mode) = SAVED.get() {
            unsafe {
                SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), mode);
            }
        }
        0
    }
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if input.is_null() {
        return read();
    }
    let mut mode: Dword = 0;
    if unsafe { GetConsoleMode(input, &mut mode) } == 0 {
        return read();
    }
    let saved = *SAVED.get_or_init(|| mode);
    unsafe {
        SetConsoleMode(input, saved & !ENABLE_ECHO_INPUT);
        SetConsoleCtrlHandler(Some(restore), 1);
    }
    let out = read();
    unsafe {
        SetConsoleMode(input, saved);
        SetConsoleCtrlHandler(Some(restore), 0);
    }
    out
}

#[cfg(not(any(unix, windows)))]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    read()
}

/// Resolve the gray binary (`GRAY_BIN` env or `PATH` lookup).
fn resolve_gray(explicit: &str) -> Option<String> {
    let candidate = explicit.trim();
    if candidate.is_empty() {
        return None;
    }
    if candidate.contains('/') || candidate.contains('\\') {
        let path = Path::new(candidate);
        if path.is_file() && is_executable(path) {
            return Some(candidate.to_string());
        }
        return None;
    }
    // Executables carry extensions on Windows (`gray` is `gray.exe`).
    let names: Vec<String> = {
        #[cfg(windows)]
        {
            [".exe", ".cmd", ".bat", ".com", ".ps1", ""]
                .iter()
                .map(|ext| format!("{candidate}{ext}"))
                .collect()
        }
        #[cfg(not(windows))]
        {
            vec![candidate.to_string()]
        }
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        for name in &names {
            let full = dir.join(name);
            if full.is_file() && is_executable(&full) {
                return Some(full.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn absolutize(path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn shellexpand_home(raw: &str) -> String {
    if raw == "~" || raw.starts_with("~/") {
        if let Some(home) = crate::config::user_home() {
            return if raw == "~" {
                home.to_string_lossy().into_owned()
            } else {
                home.join(&raw[2..]).to_string_lossy().into_owned()
            };
        }
    }
    raw.to_string()
}

/// Split a comma-separated prompt answer into trimmed non-empty entries.
fn csv(answer: &str) -> Vec<String> {
    answer
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Dedupe keeping order, case-insensitively.
fn merge(prior: Vec<String>, extra: &[String]) -> Vec<String> {
    let mut out = prior;
    for e in extra {
        if !out.iter().any(|u| u.eq_ignore_ascii_case(e)) {
            out.push(e.clone());
        }
    }
    out
}

/// Split a `host:port` answer; a bare host takes the signal-cli default.
fn parse_daemon_addr(raw: &str) -> Result<(String, u64), String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(("127.0.0.1".to_string(), 7583));
    }
    let (host, port) = match raw.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            (h.to_string(), p.parse::<u64>().unwrap_or(0))
        }
        _ => (raw.to_string(), 7583),
    };
    if !crate::config::host_valid(&Value::String(host.clone())) {
        return Err("Daemon host must be a hostname or IP".to_string());
    }
    if !(1..=65535).contains(&port) {
        return Err("Daemon port must be between 1 and 65535".to_string());
    }
    Ok((host, port))
}

/// The wizard: signal-cli daemon address, the bot's account, the allowlist.
/// A live connect + account check proves the answers before saving.
pub async fn run(path: &Path, io: &mut dyn Prompter) -> Result<bool, String> {
    if !io.is_terminal() {
        return Err("Setup needs a terminal for interactive prompts".to_string());
    }
    if path.exists() && !io.confirm("Replace existing configuration? [y/N] ")? {
        return Ok(false);
    }
    // Gray's binary and home are resolved silently (env, then the
    // conventional defaults) — derived facts, not questions.
    let gray = resolve_gray(&std::env::var("GRAY_BIN").unwrap_or_else(|_| "gray".to_string()))
        .ok_or_else(|| "Install gray first; executable not found".to_string())?;
    let gray_home = absolutize(Path::new(&shellexpand_home(
        &std::env::var("GRAY_HOME").unwrap_or_else(|_| "~/.gray".to_string()),
    )));

    // Whoever the old config allowed stays allowed: the wizard only adds.
    let prior = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(Value::is_object);
    let prior_allowed: Vec<String> = prior
        .as_ref()
        .and_then(|p| p.get("allowed_numbers"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    for line in [
        "Connect gray to Signal through a signal-cli daemon.",
        "Start one first, e.g.:  signal-cli -a +15551234567 daemon --tcp 127.0.0.1:7583",
        "(register/link the account in signal-cli beforehand — see README).",
        "The answers are checked against the live daemon before saving.",
    ] {
        io.print_line(line);
    }

    let addr_answer = io.prompt("signal-cli daemon address [127.0.0.1:7583]: ")?;
    let (daemon_host, daemon_port) = parse_daemon_addr(&addr_answer)?;

    // Prove the socket before asking more.
    let probe = crate::signal::Rpc::connect(&daemon_host, daemon_port as u16)
        .await
        .map_err(|e| format!("cannot reach signal-cli at {daemon_host}:{daemon_port}: {e}"))?;
    let version = probe
        .version()
        .await
        .ok()
        .and_then(|v| v.as_str().map(str::to_string).or_else(|| v.get("version").and_then(Value::as_str).map(str::to_string)))
        .unwrap_or_else(|| "unknown".to_string());
    io.print_line(&format!("signal-cli {version} reachable."));

    let account_answer = io.prompt("Bot account number (+E164, e.g. +15551234567): ")?;
    let account = account_answer.trim().to_string();
    if !crate::config::number_valid(&account) {
        return Err("Account must be a +E164 number like +15551234567".to_string());
    }
    // If the daemon lists its accounts, ours must be one of them. A daemon
    // that answers with an empty/heterogeneous list gets a warning, not a
    // hard fail — the account may still be managed implicitly.
    match probe.accounts().await {
        Ok(list) if !list.is_empty() && !list.iter().any(|a| a == &account) => {
            return Err(
                "signal-cli does not manage that account; register or link it first                  (signal-cli -a <number> register / link)"
                    .to_string(),
            )
        }
        Ok(list) if list.is_empty() => {
            io.print_line("Daemon reported no account list; continuing on faith.");
        }
        Ok(_) => io.print_line("Account is registered with the daemon."),
        Err(_) => io.print_line("Could not list daemon accounts; continuing on faith."),
    }

    io.print_line(
        "Access control: only allowlisted numbers/UUIDs can trigger the bot. Anyone \
         else who DMs it gets a pairing code the owner can approve.",
    );
    let allowed_answer = io.prompt(
        "Allowed senders (comma-separated +E164 numbers or UUIDs; first is usually yours — empty admits nobody): ",
    )?;
    let allowed = merge(prior_allowed, &csv(&allowed_answer));
    if !allowed
        .iter()
        .all(|u| crate::config::identity_valid(&Value::String(u.clone())))
    {
        return Err("Allowed senders must be +E164 numbers or UUIDs".to_string());
    }

    let home_answer = io.prompt(
        "Home chat for lifecycle notices (number, UUID, or group:<id>) [none]: ",
    )?;
    let home_chat = home_answer.trim().to_string();
    if !home_chat.is_empty() && !crate::config::target_valid(&Value::String(home_chat.clone())) {
        return Err("Home chat must be a number, UUID, or group:<id>".to_string());
    }

    io.print_line(
        "Groups are off unless allowlisted: list group ids, or * for every group. \
         In non-free groups the bot answers only when @-mentioned.",
    );
    let groups_answer = io.prompt("Allowed groups (comma-separated ids, *, or empty) [none]: ")?;
    let groups = csv(&groups_answer);
    if !groups
        .iter()
        .all(|g| crate::config::group_valid(&Value::String(g.clone())))
    {
        return Err("Groups must be group ids or *".to_string());
    }

    // `path.parent` is the state dir; like the other adapters, the saved
    // config holds absolute paths or `save_config` validation rejects it.
    let workdir = path
        .parent()
        .map(|p| absolutize(p).to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    let mut saved = json!({
        "account": account,
        "daemon_host": daemon_host,
        "daemon_port": daemon_port,
        "require_mention": true,
        "allowed_numbers": allowed,
        "gray_bin": absolutize(Path::new(&gray)).to_string_lossy(),
        "gray_home": gray_home.to_string_lossy(),
        "workdir": workdir,
        "session_reset": {
            "mode": "both",
            "idle_minutes": 1_440,
            "at_hour": 4
        }
    });
    if !home_chat.is_empty() {
        saved["home_chat"] = Value::String(home_chat);
    }
    if !groups.is_empty() {
        saved["allowed_groups"] =
            Value::Array(groups.into_iter().map(Value::String).collect());
    }
    // A re-run replaces only what the wizard asked about: limits and other
    // hand-tuned keys survive (unless they no longer validate).
    if let Some(mut merged) = prior {
        for (k, v) in saved.as_object().into_iter().flatten() {
            if k != "session_reset" || merged.get(k).is_none() {
                merged[k] = v.clone();
            }
        }
        if crate::config::validate_config(&merged).is_ok() {
            saved = merged;
        }
    }
    crate::config::save_config(path, &saved)?;
    io.print_line("Configuration saved privately.");

    // Prove it before anyone is told it worked: connect plus account
    // presence via the doctor.
    crate::doctor::doctor(&saved).await.map_err(|e| {
        format!("Saved, but the doctor disagreed: {e}. Fix it, then run `gray signal doctor` and `gray signal install`.")
    })?;
    io.print_line("Doctor verified the configuration.");
    Ok(true)
}
