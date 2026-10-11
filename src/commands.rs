//! Chat commands: `!name args` and `/name args` run through `gray cmd`
//! inside the conversation's own home and come back as a plain-text reply —
//! a command never becomes a model turn. The advertised name set is cached
//! for a few minutes; when `gray cmd list` cannot answer (an old gray, a
//! missing binary) a small static allowlist decides instead and `run`
//! itself reports whatever is still broken.
use serde_json::Value;
use std::collections::HashSet;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `gray cmd list` is re-asked at most this often.
pub const LIST_TTL_SECS: f64 = 300.0;
/// `run` is local work; bound it far below a model turn.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);
/// Registry rows marked `slow` get this window instead.
const SLOW_TIMEOUT: Duration = Duration::from_secs(120);
/// `list` only feeds the allowlist — fail fast and fall back.
const LIST_TIMEOUT: Duration = Duration::from_secs(15);

/// The commands trusted when `gray cmd list` cannot be asked: the
/// contract's v1 text/interactive coverage. Anything `run` still rejects
/// comes back as its own error reply.
const FALLBACK_COMMANDS: &[&str] = &[
    "help", "model", "usage", "new", "sessions", "cron", "skills",
];

/// The last `cmd list` outcome. `names: None` remembers a *failed* fetch —
/// the stamp is honored either way so a gray without `cmd` does not cost a
/// subprocess per message.
#[derive(Default)]
pub struct ListCache {
    pub fetched_at: f64,
    pub names: Option<HashSet<String>>,
    /// Names (and aliases) whose runs can outlast RUN_TIMEOUT.
    pub slow: HashSet<String>,
}

/// `!<name> <args>` or `/<name> <args>` as the whole (trimmed) message.
/// `None` for anything else — including a prefix whose "name" is not a
/// plausible command word — so the caller can treat it as a normal prompt.
pub fn parse_prefixed(text: &str) -> Option<(String, String)> {
    let body = text.trim();
    let body = body.strip_prefix('!').or_else(|| body.strip_prefix('/'))?;
    let mut parts = body.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    if !name_valid(name) {
        return None;
    }
    let args = parts.next().unwrap_or_default().trim().to_string();
    Some((name.to_string(), args))
}

/// Command words look like gray's own: 1-32 chars, alnum plus `-`/`_`,
/// starting alnum. Anything fancier is prose, not a command.
fn name_valid(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

/// Is `name` dispatchable under `names`? `Some` is the live registry
/// (aliases included); `None` means `cmd list` is dead and only the static
/// set may run.
pub fn known(names: &Option<HashSet<String>>, name: &str) -> bool {
    match names {
        Some(set) => set.contains(name),
        None => FALLBACK_COMMANDS.contains(&name),
    }
}

/// `cmd list` -> registered names plus aliases and the slow set; `None`
/// on any failure.
pub async fn list_names(config: &Value, home: &Path) -> Option<(HashSet<String>, HashSet<String>)> {
    let out = gray_cmd(config, home, &["cmd", "list"], LIST_TIMEOUT)
        .await
        .ok()?;
    parse_list(&out)
}

/// What a `cmd run` answer becomes: reply text, or a prompt the caller
/// queues as a turn in this conversation.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Text(String),
    Prompt { prompt: String, fallback: String },
}

/// `cmd run` -> the reply. `Err` strings are already chat-safe.
pub async fn run(
    config: &Value,
    home: &Path,
    name: &str,
    args: &str,
    slow: bool,
) -> Result<Outcome, String> {
    // v1 exposes one free-form args string per command — keep it one word.
    let mut argv = vec!["cmd", "run", name];
    if !args.is_empty() {
        argv.push(args);
    }
    let timeout = if slow { SLOW_TIMEOUT } else { RUN_TIMEOUT };
    let out = gray_cmd(config, home, &argv, timeout).await?;
    result_text(&out)
}

/// `gray <argv>` against `home`, with the turn runner's env discipline:
/// scrubbed environment, `GRAY_HOME` is this conversation's home, gray's
/// own dir first on PATH. Stdout is the wire; stderr stays out of chats.
async fn gray_cmd(
    config: &Value,
    home: &Path,
    argv: &[&str],
    timeout: Duration,
) -> Result<String, String> {
    let gray_bin = crate::signal::cfg_str(config, "gray_bin");
    if gray_bin.is_empty() {
        return Err("gray executable is missing".to_string());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home)
        .map_err(|_| "cannot create conversation home".to_string())?;
    // Conversation-scoped commands read the home's config.json — snapshot
    // the provider config exactly like a turn would before trusting it.
    if let Some(provider) =
        std::fs::read(Path::new(crate::signal::cfg_str(config, "gray_home")).join("config.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    {
        crate::config::atomic_json(&home.join("config.json"), &provider)?;
    }
    let mut cmd = tokio::process::Command::new(gray_bin);
    cmd.args(argv);
    cmd.current_dir(home);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    cmd.env_clear();
    for (k, v) in std::env::vars_os() {
        let Some(k) = k.to_str() else { continue };
        if k.starts_with("GRAY_") || k.starts_with("SIGNAL_") || k.starts_with("OPENAI_") {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("GRAY_HOME", home);
    cmd.env(
        "PATH",
        crate::runner::child_path(
            std::env::var_os("PATH"),
            std::env::var_os("HOME").map(PathBuf::from),
            Path::new(gray_bin),
        ),
    );
    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(_)) => return Err("cannot start gray".to_string()),
        Err(_) => return Err("the command timed out".to_string()),
    };
    if !out.status.success() {
        return Err("this gray has no command layer; upgrade gray".to_string());
    }
    String::from_utf8(out.stdout).map_err(|_| "gray answered with invalid output".to_string())
}

/// `cmd list` stdout -> (name set, slow set); `None` when the answer is
/// not an `ok` envelope carrying a `commands` array.
fn parse_list(stdout: &str) -> Option<(HashSet<String>, HashSet<String>)> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    if v.get("ok") != Some(&Value::Bool(true)) {
        return None;
    }
    let commands = v.pointer("/result/commands")?.as_array()?;
    let mut out = HashSet::new();
    let mut slow = HashSet::new();
    for c in commands {
        if c.get("source").and_then(Value::as_str) == Some("skill") {
            continue;
        }
        let is_slow = c.get("slow").and_then(Value::as_bool).unwrap_or(false);
        if let Some(name) = c.get("name").and_then(Value::as_str) {
            out.insert(name.to_string());
            if is_slow {
                slow.insert(name.to_string());
            }
        }
        if let Some(aliases) = c.get("aliases").and_then(Value::as_array) {
            for a in aliases.iter().filter_map(Value::as_str) {
                out.insert(a.to_string());
                if is_slow {
                    slow.insert(a.to_string());
                }
            }
        }
    }
    Some((out, slow))
}

/// `cmd run` stdout -> the text to post. Signal renders `kind:"ui"`
/// results through `fallback_text` (the component-less render), `kind:
/// "text"` through `text`; `ok:false` becomes its own error message.
fn result_text(stdout: &str) -> Result<Outcome, String> {
    let v: Value = serde_json::from_str(stdout.trim())
        .map_err(|_| "gray answered with invalid output; upgrade gray".to_string())?;
    if v.get("ok") != Some(&Value::Bool(true)) {
        return Err(v
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("the command failed")
            .to_string());
    }
    let result = v.get("result").cloned().unwrap_or(Value::Null);
    if result.get("kind").and_then(Value::as_str) == Some("prompt") {
        let prompt = result
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if prompt.is_empty() {
            return Err("the command returned an empty prompt".to_string());
        }
        let fallback = result
            .get("fallback_text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Ok(Outcome::Prompt { prompt, fallback });
    }
    let text = match result.get("kind").and_then(Value::as_str) {
        Some("text") => result.get("text"),
        Some("ui") => result.get("fallback_text"),
        _ => None,
    };
    text.and_then(Value::as_str)
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty())
        .map(Outcome::Text)
        .ok_or_else(|| "the command returned nothing".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_takes_bang_and_slash() {
        assert_eq!(
            parse_prefixed("!help"),
            Some(("help".to_string(), String::new()))
        );
        assert_eq!(
            parse_prefixed("/usage"),
            Some(("usage".to_string(), String::new()))
        );
        assert_eq!(
            parse_prefixed("  !model   claude-sonnet  "),
            Some(("model".to_string(), "claude-sonnet".to_string()))
        );
        assert_eq!(
            parse_prefixed("/cron pause job one"),
            Some(("cron".to_string(), "pause job one".to_string()))
        );
    }

    #[test]
    fn parse_passes_prose_through() {
        for text in [
            "",
            "hello",
            "tell me !help",
            "[in reply to a: b]\n!help",
            "!",
            "/",
            "!!",
            "! name",
            "!no;rm",
            "!has.dot",
            "/a/b",
            "!😀x",
        ] {
            assert_eq!(parse_prefixed(text), None, "{text:?}");
        }
    }

    #[test]
    fn dispatch_uses_registry_or_fallback() {
        let live: Option<HashSet<String>> = Some(
            ["help", "model", "theme"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        // The live registry decides alone — the static set does not widen it.
        assert!(known(&live, "theme"));
        assert!(known(&live, "help"));
        assert!(!known(&live, "new"));
        let dead = None;
        assert!(known(&dead, "help"));
        assert!(known(&dead, "sessions"));
        assert!(!known(&dead, "theme"));
        assert!(!known(&dead, "bogus"));
    }

    #[test]
    fn list_fixture_reads_names_and_aliases() {
        let fixture = r#"{"ok":true,"protocol":1,"result":{"commands":[
            {"name":"help","description":"show commands","aliases":["h","?"],
             "args":[],"scope":"global","surface":"text"},
            {"name":"model","description":"pick model","aliases":[],
             "args":[{"name":"args","required":false,"hint":"model id"}],
             "scope":"conversation","surface":"interactive"}]}}"#;
        let (names, slow) = parse_list(fixture).expect("fixture parses");
        for want in ["help", "h", "?", "model"] {
            assert!(names.contains(want), "{want}");
        }
        assert!(!names.contains("usage"));
        assert!(slow.is_empty());
    }

    #[test]
    fn list_fixture_rejects_failures() {
        assert!(parse_list(
            r#"{"ok":false,"error":{"code":"unavailable","message":"x","retryable":false}}"#
        )
        .is_none());
        assert!(parse_list(r#"{"ok":true,"result":{}}"#).is_none());
        assert!(parse_list("not json").is_none());
    }

    #[test]
    fn run_fixture_maps_result_kinds() {
        match result_text(
            r#"{"ok":true,"result":{"kind":"text","text":"**hi** there","close":true}}"#,
        ) {
            Ok(Outcome::Text(t)) => assert_eq!(t, "**hi** there"),
            other => panic!("expected text: {other:?}"),
        }
        match result_text(
            r#"{"ok":true,"result":{"kind":"ui","token":"t","document":{},"fallback_text":"pick 1-3"}}"#,
        ) {
            Ok(Outcome::Text(t)) => assert_eq!(t, "pick 1-3"),
            other => panic!("expected text: {other:?}"),
        }
        assert_eq!(
            result_text(
                r#"{"ok":false,"error":{"code":"local_only","message":"theme is TUI-only","retryable":false}}"#
            ),
            Err("theme is TUI-only".to_string())
        );
        // A ui result Signal cannot render falls back to an error.
        assert!(
            result_text(r#"{"ok":true,"result":{"kind":"ui","token":"t","document":{}}}"#).is_err()
        );
        assert!(result_text(r#"{"ok":true,"result":{"kind":"text","text":"  "}}"#).is_err());
        assert!(result_text("garbage").is_err());
    }
}
