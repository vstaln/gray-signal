//! Outbound text shaping: markdown -> Signal plain text + `textStyles`
//! (`start:length:STYLE`, positions in UTF-16 code units), then
//! boundary-aware chunking. A port of hermes's `signal_format.py` rules:
//! fenced code becomes MONOSPACE, headings BOLD, `**`/`__` BOLD, `~~`
//! STRIKETHROUGH, `` ` `` MONOSPACE, `*`/`_` ITALIC; `- ` bullets normalize
//! to `•` because Signal renders list markers literally.

/// One style range over the final plain text, UTF-16 offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Style {
    start: u32,
    len: u32,
    kind: &'static str,
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// Convert markdown to (plain text, style range strings). Ranges never
/// leak marker characters — Signal would render them literally.
pub fn md_to_signal(text: &str) -> (String, Vec<String>) {
    let squashed = squash_blank_lines(text.trim());
    let normalized = normalize_bullets(&squashed);
    let (text, mut styles) = strip_code_fences(&normalized);
    let (text, heading_styles) = strip_headings(&text);
    styles.extend(heading_styles);
    let (plain, inline, removals) = strip_inline(&text);
    // Earlier ranges shift left by every marker byte removed before them.
    let mut all: Vec<Style> = styles
        .iter()
        .map(|s| {
            let start = adjust(s.start as usize, &removals) as u32;
            let end = adjust(s.start as usize + s.len as usize, &removals) as u32;
            Style {
                start,
                len: end.saturating_sub(start),
                kind: s.kind,
            }
        })
        .filter(|s| s.len > 0)
        .collect();
    all.extend(inline);
    all.sort_by_key(|s| s.start);
    let style_strings = ranges_to_strings(&plain, &all);
    (plain, style_strings)
}

fn squash_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = 0;
    for c in text.chars() {
        if c == '\n' {
            run += 1;
            if run <= 2 {
                out.push('\n');
            }
        } else {
            run = 0;
            out.push(c);
        }
    }
    out
}

/// `- item` -> `\u2022 item` outside ``` fences; also `*`/`+` markers.
fn normalize_bullets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        if body.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        let mut replaced = false;
        if !in_fence {
            let indent = body.len() - body.trim_start_matches([' ', '\t']).len();
            if indent <= 3 {
                for marker in ["- ", "* ", "+ "] {
                    if let Some(item) = body[indent..].strip_prefix(marker) {
                        out.push_str(&body[..indent]);
                        out.push_str("\u{2022} ");
                        out.push_str(item);
                        if line.ends_with('\n') {
                            out.push('\n');
                        }
                        replaced = true;
                        break;
                    }
                }
            }
        }
        if !replaced {
            out.push_str(line);
        }
    }
    out
}

/// ```lang\nbody``` -> body, MONOSPACE range over it. Unmatched fences
/// stay literal.
fn strip_code_fences(text: &str) -> (String, Vec<Style>) {
    let mut out = String::with_capacity(text.len());
    let mut styles = Vec::new();
    let mut rest = text;
    loop {
        let Some(open) = rest.find("```") else {
            out.push_str(rest);
            break;
        };
        let after_open = &rest[open + 3..];
        let Some(close) = after_open.find("```") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..open]);
        // First line of the fence is the language tag; body starts after it.
        let fence_body = &after_open[..close];
        let inner = match fence_body.find('\n') {
            Some(nl) => &fence_body[nl + 1..],
            None => fence_body,
        };
        let inner = inner.trim_end_matches('\n');
        let start = utf16_len(&out) as u32;
        let len = utf16_len(inner) as u32;
        out.push_str(inner);
        if len > 0 {
            styles.push(Style {
                start,
                len,
                kind: "MONOSPACE",
            });
        }
        rest = &after_open[close + 3..];
    }
    (out, styles)
}

/// `## title` -> title with a BOLD range; styles recorded in UTF-16 over
/// the new text.
fn strip_headings(text: &str) -> (String, Vec<Style>) {
    let mut out = String::with_capacity(text.len());
    let mut styles = Vec::new();
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches('\n');
        let nl = line.len() - body.len();
        let hashes = body.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && body[hashes..].starts_with([' ', '\t']) {
            let heading = body[hashes..].trim_start();
            let start = utf16_len(&out) as u32;
            out.push_str(heading);
            styles.push(Style {
                start,
                len: utf16_len(heading) as u32,
                kind: "BOLD",
            });
            out.push_str(&line[line.len() - nl..]);
        } else {
            out.push_str(line);
        }
    }
    (out, styles)
}

/// One inline match: (marker_start, marker_end, inner_start, inner_end,
/// style) — all char indices into the pre-strip text.
type InlineMatch = (usize, usize, usize, usize, &'static str);

/// Claim non-overlapping inline styled spans in priority order — the same
/// "first pattern wins" rule the Python applies.
fn find_inline_matches(chars: &[char]) -> Vec<InlineMatch> {
    let mut occupied: Vec<(usize, usize)> = Vec::new();
    let mut found: Vec<InlineMatch> = Vec::new();
    let mut claim = |ms: usize, me: usize, is: usize, ie: usize, kind: &'static str| {
        if occupied.iter().any(|(os, oe)| ms < *oe && me > *os) {
            return;
        }
        occupied.push((ms, me));
        found.push((ms, me, is, ie, kind));
    };
    // delimiter pairs: (open, close, style)
    for (delim, kind) in [
        ("**", "BOLD"),
        ("__", "BOLD"),
        ("~~", "STRIKETHROUGH"),
        ("`", "MONOSPACE"),
    ] {
        let mut i = 0;
        while let Some(ms) = find_delim(chars, i, delim) {
            let is = ms + delim.len();
            let Some(ie) = find_delim(chars, is + 1, delim) else {
                break;
            };
            if ie == is {
                i = ie + delim.len();
                continue;
            }
            claim(ms, ie + delim.len(), is, ie, kind);
            i = ie + delim.len();
        }
    }
    // `*italic*`: opener not preceded/followed by `*`, not followed by space;
    // closer not preceded/followed by `*`.
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '*'
            && (i == 0 || chars[i - 1] != '*')
            && i + 1 < chars.len()
            && chars[i + 1] != '*'
            && chars[i + 1] != ' '
        {
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == '*'
                    && chars[j - 1] != '*'
                    && (j + 1 == chars.len() || chars[j + 1] != '*')
                {
                    if j > i + 1 {
                        claim(i, j + 1, i + 1, j, "ITALIC");
                    }
                    i = j;
                    break;
                }
                j += 1;
            }
        }
        i += 1;
    }
    // `_italic_`: opener not preceded by a word char and not followed by `_`;
    // closer not preceded by `_` and not followed by a word char.
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '_'
            && (i == 0 || !word(chars[i - 1]))
            && i + 1 < chars.len()
            && chars[i + 1] != '_'
        {
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == '_'
                    && chars[j - 1] != '_'
                    && (j + 1 == chars.len() || !word(chars[j + 1]))
                {
                    if j > i + 1 {
                        claim(i, j + 1, i + 1, j, "ITALIC");
                    }
                    i = j;
                    break;
                }
                j += 1;
            }
        }
        i += 1;
    }
    found.sort_by_key(|m| m.0);
    found
}

fn find_delim(chars: &[char], from: usize, delim: &str) -> Option<usize> {
    let d: Vec<char> = delim.chars().collect();
    (from..=chars.len().saturating_sub(d.len()))
        .find(|&i| chars[i..i + d.len()] == d[..])
}

/// Strip inline markers. Returns (plain, inline styles in UTF-16 over
/// plain, removals as (char_pos, len) in the pre-strip text).
fn strip_inline(text: &str) -> (String, Vec<Style>, Vec<(usize, usize)>) {
    let chars: Vec<char> = text.chars().collect();
    let matches = find_inline_matches(&chars);
    let mut plain = String::with_capacity(text.len());
    let mut styles = Vec::new();
    let mut removals: Vec<(usize, usize)> = Vec::new();
    let mut cursor = 0usize;
    for (ms, me, is, ie, kind) in matches {
        if ms > is {
            // impossible (inner starts after the opener) but keep the shape
        }
        if is > ms {
            removals.push((ms, is - ms));
        }
        if me > ie {
            removals.push((ie, me - ie));
        }
        push_chars(&mut plain, &chars[cursor..ms]);
        let start = utf16_len(&plain) as u32;
        push_chars(&mut plain, &chars[is..ie]);
        styles.push(Style {
            start,
            len: utf16_len(&chars[is..ie].iter().collect::<String>()) as u32,
            kind,
        });
        cursor = me;
    }
    push_chars(&mut plain, &chars[cursor..]);
    removals.sort();
    (plain, styles, removals)
}

fn push_chars(out: &mut String, chars: &[char]) {
    for c in chars {
        out.push(*c);
    }
}

/// A position in the pre-strip text mapped through the marker removals —
/// the Python `_adjust` verbatim.
fn adjust(pos: usize, removals: &[(usize, usize)]) -> usize {
    let mut shift = 0usize;
    for (rp, rl) in removals {
        if *rp >= pos {
            break;
        }
        shift += (*rl).min(pos - *rp);
    }
    pos - shift
}

/// Serialize ranges as `start:len:STYLE` strings. Out-of-range spans are
/// dropped rather than sent malformed to the daemon.
fn ranges_to_strings(plain: &str, styles: &[Style]) -> Vec<String> {
    let mut bounds: Vec<usize> = Vec::with_capacity(plain.chars().count() + 1);
    bounds.push(0);
    for c in plain.chars() {
        bounds.push(bounds.last().unwrap() + utf16_len(&c.to_string()));
    }
    let total = *bounds.last().unwrap_or(&0);
    styles
        .iter()
        .filter(|s| (s.start + s.len) as usize <= total)
        .map(|s| format!("{}:{}:{}", s.start, s.len, s.kind))
        .collect()
}

/// Split the formatted answer into per-message chunks, translating style
/// ranges into each chunk's local UTF-16 offsets — the Python
/// `_split_signal_formatted_message`: styles that cross a boundary stay
/// styled instead of leaking markdown markers.
pub fn split_signal_message(text: &str, limit: usize) -> Vec<(String, Vec<String>)> {
    let (plain, style_strings) = md_to_signal(text);
    let styles: Vec<Style> = style_strings
        .iter()
        .filter_map(|s| {
            let mut it = s.splitn(3, ':');
            let start = it.next()?.parse().ok()?;
            let len = it.next()?.parse().ok()?;
            let kind: &'static str = match it.next()? {
                "BOLD" => "BOLD",
                "ITALIC" => "ITALIC",
                "STRIKETHROUGH" => "STRIKETHROUGH",
                "MONOSPACE" => "MONOSPACE",
                _ => return None,
            };
            Some(Style { start, len, kind })
        })
        .collect();
    let chars: Vec<char> = plain.chars().collect();
    // Cumulative UTF-16 length at each char boundary.
    let mut offsets: Vec<usize> = Vec::with_capacity(chars.len() + 1);
    offsets.push(0);
    for c in &chars {
        offsets.push(offsets.last().unwrap() + utf16_len(&c.to_string()));
    }
    let total_u16 = *offsets.last().unwrap_or(&0);
    if total_u16 <= limit {
        return vec![(plain, style_strings)];
    }
    let body_limit = limit.max(11) - 10; // " (i/N)" indicator reserve
    let mut chunks: Vec<(String, Vec<Style>)> = Vec::new();
    let mut start = 0usize;
    while offsets[start] < total_u16 {
        let budget = (offsets[start] + body_limit).min(total_u16);
        let mut end = start + 1;
        while end < offsets.len() && offsets[end] <= budget {
            end += 1;
        }
        end = (end - 1).max(start + 1);
        let chunk: String = chars[start..end].iter().collect();
        let base = offsets[start] as u32;
        let end_u16 = offsets[end] as u32;
        let local: Vec<Style> = styles
            .iter()
            .filter_map(|s| {
                let os = s.start.max(base);
                let oe = (s.start + s.len).min(end_u16);
                (oe > os).then(|| Style {
                    start: os - base,
                    len: oe - os,
                    kind: s.kind,
                })
            })
            .collect();
        chunks.push((chunk, local));
        start = end;
    }
    let n = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, (chunk, local))| {
            let text = if n == 1 {
                chunk
            } else {
                format!("{chunk} ({}/{n})", i + 1)
            };
            {
                let strings = ranges_to_strings(&text, &local);
                (text, strings)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bold_and_italic_strip_markers() {
        let (plain, styles) = md_to_signal("a **bold** and *ital* end");
        assert_eq!(plain, "a bold and ital end");
        assert!(styles.contains(&"2:4:BOLD".to_string()), "{styles:?}");
        assert!(styles.contains(&"11:4:ITALIC".to_string()), "{styles:?}");
    }

    #[test]
    fn code_fence_is_monospace() {
        let (plain, styles) = md_to_signal("look\n```rust\nlet x = 1;\n```\nend");
        assert_eq!(plain, "look\nlet x = 1;\nend");
        assert_eq!(styles, vec!["5:10:MONOSPACE".to_string()]);
    }

    #[test]
    fn heading_is_bold() {
        let (plain, styles) = md_to_signal("## Hi there\nbody");
        assert_eq!(plain, "Hi there\nbody");
        assert_eq!(styles, vec!["0:8:BOLD".to_string()]);
    }

    #[test]
    fn bullets_normalize() {
        let (plain, _) = md_to_signal("- one\n- two");
        assert_eq!(plain, "• one\n• two");
    }

    #[test]
    fn utf16_offsets_count_astral_as_two() {
        // '😀' is two UTF-16 code units; the bold range after it starts at 4.
        let (plain, styles) = md_to_signal("😀 **x**");
        assert_eq!(plain, "😀 x");
        assert_eq!(styles, vec!["3:1:BOLD".to_string()]);
    }

    #[test]
    fn long_text_chunks_with_local_styles() {
        let mut s = "**".to_string();
        s.push_str(&"ab".repeat(4500));
        s.push_str("**");
        let chunks = split_signal_message(&s, 8000);
        assert!(chunks.len() >= 2);
        for (text, styles) in &chunks {
            assert!(text.encode_utf16().count() <= 8000);
            for st in styles {
                let mut it = st.splitn(3, ':');
                let start: usize = it.next().unwrap().parse().unwrap();
                let len: usize = it.next().unwrap().parse().unwrap();
                assert!(start + len <= text.encode_utf16().count());
            }
        }
    }

    #[test]
    fn word_underscore_is_not_italic() {
        let (plain, styles) = md_to_signal("file_name_here");
        assert_eq!(plain, "file_name_here");
        assert!(styles.is_empty());
    }
}
