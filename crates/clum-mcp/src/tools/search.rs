//! Recording content search: scan locally synced asciinema v2 .cast files
//! for keyword or regex matches, with ANSI-aware text extraction.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::LazyLock;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::warn;

use super::ToolContext;
use crate::recording_sync::list_local_recordings;
use clum_core::types::AuditAction;

/// Maximum files to scan in a single call (safety limit).
const MAX_SCAN_FILES: usize = 100;
/// Maximum context lines on each side of a match.
const MAX_CONTEXT: usize = 10;
/// Maximum total matches per call.
const MAX_MATCHES: usize = 200;

pub(crate) async fn search_recordings(ctx: &ToolContext, args: Value) -> Result<Value> {
    let start = std::time::Instant::now();
    let query = args["query"]
        .as_str()
        .context("missing 'query'")?
        .to_string();
    if query.is_empty() {
        anyhow::bail!("'query' must not be empty");
    }

    let host = args["host"].as_str();
    let date_from = args["date_from"].as_str();
    let date_to = args["date_to"].as_str();
    let session = args["session"].as_str();
    let match_mode = args["match_mode"].as_str().unwrap_or("plain");
    let search_input = args["search_input"].as_bool().unwrap_or(true);
    let search_output = args["search_output"].as_bool().unwrap_or(true);
    let context_lines = args["context_lines"]
        .as_u64()
        .unwrap_or(2)
        .min(MAX_CONTEXT as u64) as usize;
    let limit = args["limit"].as_u64().unwrap_or(50).min(MAX_MATCHES as u64) as usize;
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;

    // Get candidate recording files.
    let mut candidates = list_local_recordings(&ctx.recordings_dir, host, None, session).await?;

    // Apply date range filter on top of list_local_recordings.
    candidates.retain(|r| {
        let d = r["date"].as_str().unwrap_or("");
        if let Some(from) = date_from {
            if d < from {
                return false;
            }
        }
        if let Some(to) = date_to {
            if d > to {
                return false;
            }
        }
        true
    });

    // Sort by date descending (newest first — most searches are for recent events).
    candidates.sort_by(|a, b| {
        b["date"]
            .as_str()
            .unwrap_or("")
            .cmp(a["date"].as_str().unwrap_or(""))
    });

    if candidates.len() > MAX_SCAN_FILES {
        candidates.truncate(MAX_SCAN_FILES);
    }

    let mut matches: Vec<Value> = Vec::new();
    let mut scanned_files: usize = 0;
    let mut scanned_bytes: u64 = 0;

    for candidate in &candidates {
        if matches.len() >= limit + offset {
            break;
        }
        let path = candidate["path"].as_str().unwrap_or("");
        let file_matches = match scan_cast_file(
            path,
            &query,
            match_mode,
            search_input,
            search_output,
            context_lines,
        )
        .await
        {
            Ok(matches) => matches,
            Err(e) => {
                warn!("scan_cast_file failed for {path}: {e}");
                Vec::new()
            }
        };
        scanned_files += 1;

        for m in file_matches {
            if matches.len() >= limit + offset {
                break;
            }
            scanned_bytes += m.bytes;
            if matches.len() >= offset {
                let host = candidate["host"].as_str().unwrap_or("");
                let date = candidate["date"].as_str().unwrap_or("");
                let file = candidate["file"].as_str().unwrap_or("");
                let session = candidate["session"].as_str().unwrap_or("");
                let user = candidate["user"].as_str().unwrap_or("");
                let pane = candidate["pane"].as_str().unwrap_or("");
                matches.push(json!({
                    "host": host,
                    "date": date,
                    "file": file,
                    "session": session,
                    "user": user,
                    "pane": pane,
                    "line": m.line_number,
                    "elapsed_secs": m.elapsed,
                    "event_type": m.event_type,
                    "matched_text": m.matched_text,
                    "context_before": m.context_before,
                    "context_after": m.context_after,
                }));
            }
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;

    super::audit(
        ctx,
        AuditAction::SearchRecordings,
        "",
        "",
        None,
        &format!(
            "query:{} files:{} matches:{}",
            query,
            scanned_files,
            matches.len()
        ),
        None,
        true,
        duration_ms,
        None,
    )
    .await;

    Ok(json!({
        "ok": true,
        "total": matches.len(),
        "matches": matches,
        "scanned_files": scanned_files,
        "scanned_bytes": scanned_bytes,
    }))
}

struct FileMatch {
    line_number: u64,
    elapsed: f64,
    event_type: String,
    matched_text: String,
    context_before: Vec<String>,
    context_after: Vec<String>,
    bytes: u64,
}

/// Scan a single .cast file for matches. Streams the file line-by-line
/// to avoid loading the entire file into a single string allocation.
async fn scan_cast_file(
    path_str: &str,
    query: &str,
    match_mode: &str,
    search_input: bool,
    search_output: bool,
    context_lines: usize,
) -> Result<Vec<FileMatch>> {
    let path = Path::new(path_str);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = tokio::fs::File::open(path).await?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    match lines.next_line().await {
        Ok(Some(header)) if !header.is_empty() => {}
        Ok(Some(_)) => return Ok(Vec::new()),
        Ok(None) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    }

    let mut events: Vec<(u64, f64, String, String)> = Vec::new();
    let mut line_num: u64 = 1;
    loop {
        match lines.next_line().await {
            Ok(Some(raw)) => {
                if let Some((elapsed, event_type, text)) = parse_event_line(&raw) {
                    let clean = strip_ansi(&text);
                    events.push((line_num, elapsed, event_type, clean));
                }
                line_num += 1;
            }
            Ok(None) => break,
            Err(e) => return Err(e.into()),
        }
    }

    let regex = if match_mode == "regex" {
        Some(regex::Regex::new(query).context("invalid regex")?)
    } else {
        None
    };

    let mut results: Vec<FileMatch> = Vec::new();
    for (idx, (line_num, elapsed, event_type, clean)) in events.iter().enumerate() {
        let is_input = event_type == "i";
        if (is_input && !search_input) || (!is_input && !search_output) {
            continue;
        }

        let matched = if let Some(ref re) = regex {
            re.is_match(clean)
        } else {
            clean.contains(query)
        };

        if matched {
            let context_before: Vec<String> = events
                .iter()
                .take(idx)
                .rev()
                .take(context_lines)
                .rev()
                .map(|(_, _, _, t)| t.clone())
                .collect();

            let context_after: Vec<String> = events
                .iter()
                .skip(idx + 1)
                .take(context_lines)
                .map(|(_, _, _, t)| t.clone())
                .collect();

            let bytes = clean.len() as u64
                + context_before.iter().map(|s| s.len() as u64).sum::<u64>()
                + context_after.iter().map(|s| s.len() as u64).sum::<u64>();

            results.push(FileMatch {
                line_number: *line_num,
                elapsed: *elapsed,
                event_type: event_type.clone(),
                matched_text: clean.clone(),
                context_before,
                context_after,
                bytes,
            });
        }
    }

    Ok(results)
}

fn parse_event_line(line: &str) -> Option<(f64, String, String)> {
    let line = line.trim();
    if line.is_empty() || !line.starts_with('[') {
        return None;
    }

    let value: Value = serde_json::from_str(line).ok()?;
    let arr = value.as_array()?;
    if arr.len() < 3 {
        return None;
    }

    let elapsed = arr[0].as_f64()?;
    let event_type = arr[1].as_str()?.to_string();
    if event_type != "i" && event_type != "o" {
        return None;
    }
    let text = arr[2].as_str().unwrap_or("").to_string();
    Some((elapsed, event_type, text))
}

/// Strip ANSI escape sequences from text.
fn strip_ansi(text: &str) -> String {
    // ANSI escape sequences: ESC [ ... (digits/semicolons/question marks) ... letter
    // Compile once is better, but this is called per-line and the regex is simple.
    static ANSI_RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new("\x1b\\[[0-9;?]*[a-zA-Z]").unwrap());
    let result = ANSI_RE.replace_all(text, "").to_string();
    // Also strip the ESC character alone (some sequences like ESC without bracket).
    result.replace('\x1b', "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_strip_ansi_removes_color_codes() {
        assert_eq!(strip_ansi("\x1b[1;36mHello\x1b[0m World"), "Hello World");
    }

    #[test]
    fn test_strip_ansi_removes_cursor_moves() {
        assert_eq!(strip_ansi("\x1b[?1049h\x1b[1;1Hsystemctl"), "systemctl");
    }

    #[test]
    fn test_strip_ansi_preserves_plain_text() {
        assert_eq!(
            strip_ansi("systemctl restart nginx"),
            "systemctl restart nginx"
        );
    }

    #[test]
    fn test_strip_ansi_empty_input() {
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn test_parse_event_line_output() {
        let (elapsed, event_type, text) =
            parse_event_line(r#"[2.345, "o", "hello world"]"#).unwrap();
        assert!((elapsed - 2.345).abs() < 0.001);
        assert_eq!(event_type, "o");
        assert_eq!(text, "hello world");
    }

    #[test]
    fn test_parse_event_line_input() {
        let (_, event_type, text) = parse_event_line(r#"[5.0, "i", "ls -la\n"]"#).unwrap();
        assert_eq!(event_type, "i");
        assert_eq!(text, "ls -la\n");
    }

    #[test]
    fn test_parse_event_line_not_event() {
        assert!(parse_event_line(r#"{"version": 2}"#).is_none());
        assert!(parse_event_line("not json").is_none());
        assert!(parse_event_line("").is_none());
    }

    #[tokio::test]
    async fn test_scan_cast_file_basic_substring() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("test.cast");

        let mut f = std::fs::File::create(&cast_path).unwrap();
        f.write_all(b"{\"version\":2,\"width\":80,\"height\":24}\n")
            .unwrap();
        f.write_all(b"[0.5, \"o\", \"root@tf01:~# \"]\n").unwrap();
        f.write_all(b"[1.2, \"i\", \"systemctl restart nginx\"]\n")
            .unwrap();
        f.write_all(b"[2.0, \"o\", \"Restarting nginx...\"]\n")
            .unwrap();
        f.write_all(b"[3.5, \"o\", \"done.\"]\n").unwrap();
        f.write_all(b"[4.0, \"exit\", 0]\n").unwrap();

        let results = scan_cast_file(cast_path.to_str().unwrap(), "nginx", "plain", true, true, 1)
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].matched_text, "systemctl restart nginx");
        assert_eq!(results[0].event_type, "i");
        assert_eq!(results[1].matched_text, "Restarting nginx...");
        assert_eq!(results[1].event_type, "o");
    }

    #[tokio::test]
    async fn test_scan_cast_file_context_lines() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("context.cast");

        let mut f = std::fs::File::create(&cast_path).unwrap();
        f.write_all(b"{\"version\":2,\"width\":80,\"height\":24}\n")
            .unwrap();
        f.write_all(b"[0.5, \"o\", \"line before\"]\n").unwrap();
        f.write_all(b"[1.0, \"o\", \"matched line\"]\n").unwrap();
        f.write_all(b"[1.5, \"o\", \"line after\"]\n").unwrap();

        let results = scan_cast_file(
            cast_path.to_str().unwrap(),
            "matched",
            "plain",
            true,
            true,
            1,
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].context_before, vec!["line before"]);
        assert_eq!(results[0].context_after, vec!["line after"]);
    }

    #[tokio::test]
    async fn test_scan_cast_file_regex_match() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("regex.cast");

        let mut f = std::fs::File::create(&cast_path).unwrap();
        f.write_all(b"{\"version\":2,\"width\":80,\"height\":24}\n")
            .unwrap();
        f.write_all(b"[1.0, \"o\", \"error: connection refused\"]\n")
            .unwrap();
        f.write_all(b"[2.0, \"o\", \"error: timeout\"]\n").unwrap();
        f.write_all(b"[3.0, \"o\", \"success\"]\n").unwrap();

        let results = scan_cast_file(
            cast_path.to_str().unwrap(),
            r"error: (connection|timeout)",
            "regex",
            true,
            true,
            0,
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_scan_cast_file_filter_event_type() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("filter.cast");

        let mut f = std::fs::File::create(&cast_path).unwrap();
        f.write_all(b"{\"version\":2,\"width\":80,\"height\":24}\n")
            .unwrap();
        f.write_all(b"[1.0, \"i\", \"hello cmd\"]\n").unwrap();
        f.write_all(b"[2.0, \"o\", \"hello output\"]\n").unwrap();

        // Search only input events.
        let results = scan_cast_file(
            cast_path.to_str().unwrap(),
            "hello",
            "plain",
            true,
            false,
            0,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event_type, "i");

        // Search only output events.
        let results = scan_cast_file(
            cast_path.to_str().unwrap(),
            "hello",
            "plain",
            false,
            true,
            0,
        )
        .await
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event_type, "o");
    }

    #[tokio::test]
    async fn test_scan_cast_file_ansi_stripped_match() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("ansi.cast");

        let mut f = std::fs::File::create(&cast_path).unwrap();
        f.write_all(b"{\"version\":2,\"width\":80,\"height\":24}\n")
            .unwrap();
        let line = format!(
            "[1.0, \"o\", {}]\n",
            serde_json::to_string("\x1b[1;32mOK\x1b[0m systemctl restarted").unwrap()
        );
        f.write_all(line.as_bytes()).unwrap();

        let results = scan_cast_file(
            cast_path.to_str().unwrap(),
            "systemctl restarted",
            "plain",
            true,
            true,
            0,
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].matched_text, "OK systemctl restarted");
    }

    #[tokio::test]
    async fn test_scan_cast_file_nonexistent_file() {
        let results = scan_cast_file(
            "/nonexistent/path/file.cast",
            "test",
            "plain",
            true,
            true,
            0,
        )
        .await
        .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_scan_cast_file_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let cast_path = dir.path().join("empty.cast");
        std::fs::write(&cast_path, "").unwrap();

        let results = scan_cast_file(cast_path.to_str().unwrap(), "test", "plain", true, true, 0)
            .await
            .unwrap();
        assert!(results.is_empty());
    }
}
