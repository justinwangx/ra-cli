use crate::constants::{DEFAULT_GREP_LIMIT, DEFAULT_LIST_LIMIT, DEFAULT_READ_LIMIT};
use crate::protocol::ToolCall;
use anyhow::{anyhow, Context, Result};
use globset::{Glob, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use wait_timeout::ChildExt;
use walkdir::WalkDir;

fn web_max_bytes() -> u64 {
    const DEFAULT: u64 = 2 * 1024 * 1024; // 2 MiB
    const MIN: u64 = 64 * 1024; // 64 KiB
    const MAX: u64 = 20 * 1024 * 1024; // 20 MiB

    let raw = std::env::var("RA_WEB_MAX_BYTES").ok();
    let Some(raw) = raw.filter(|s| !s.trim().is_empty()) else {
        return DEFAULT;
    };
    match raw.trim().parse::<u64>() {
        Ok(v) => v.clamp(MIN, MAX),
        Err(_) => DEFAULT,
    }
}

fn should_retry_reqwest_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect()
}

fn should_retry_http_status(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
}

fn sleep_backoff(attempt: usize) {
    // Small bounded backoff (roughly 0.5s, 1s, 2s).
    let ms = match attempt {
        0 => 500,
        1 => 1_000,
        _ => 2_000,
    };
    thread::sleep(Duration::from_millis(ms));
}

pub(crate) struct ToolExecContext<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) max_output_chars: usize,
}

pub(crate) fn execute_tool(tool_call: &ToolCall, ctx: &ToolExecContext<'_>) -> Result<String> {
    let args: Value = serde_json::from_str(&tool_call.function.arguments)?;
    match tool_call.function.name.as_str() {
        "shell_command" => {
            let args: ShellArgs = serde_json::from_value(args)?;
            run_shell_command(&args, ctx.cwd, ctx.max_output_chars)
        }
        "read_file" => {
            let args: ReadFileArgs = serde_json::from_value(args)?;
            read_file(&args, ctx.cwd)
        }
        "list_dir" => {
            let args: ListDirArgs = serde_json::from_value(args)?;
            list_dir(&args, ctx.cwd)
        }
        "grep_files" => {
            let args: GrepFilesArgs = serde_json::from_value(args)?;
            grep_files(&args, ctx.cwd)
        }
        "web_search" => {
            let args: WebSearchArgs = serde_json::from_value(args)?;
            web_search(&args, ctx.max_output_chars)
        }
        "web_open" => {
            let args: WebOpenArgs = serde_json::from_value(args)?;
            web_open(&args, ctx.max_output_chars)
        }
        "web_find" => {
            let args: WebFindArgs = serde_json::from_value(args)?;
            web_find(&args, ctx.max_output_chars)
        }
        "apply_patch" => {
            let args: ApplyPatchArgs = serde_json::from_value(args)?;
            apply_patch(&args, ctx.cwd, ctx.max_output_chars)
        }
        other => Ok(tool_error(format!("Unknown tool: {}", other))),
    }
}

pub(crate) fn tool_error(message: String) -> String {
    json!({
        "error": message,
    })
    .to_string()
}

pub(crate) fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

pub(crate) fn build_tools(submit_enabled: bool, web_search_enabled: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "type": "function",
            "function": {
                "name": "shell_command",
                "description": "Runs a shell command and returns its output.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The shell command to execute." },
                        "workdir": { "type": ["string", "null"], "description": "Working directory for the command." },
                        "timeout_ms": { "type": ["number", "null"], "description": "Timeout in milliseconds." },
                        "max_output_chars": { "type": ["number", "null"], "description": "Maximum output characters to return." }
                    },
                    // Some providers enforce `required` includes all keys in `properties`.
                    // We keep optional semantics by allowing `null` and handling null/missing in the tool.
                    "required": ["command", "workdir", "timeout_ms", "max_output_chars"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Reads a paginated range of lines from a file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file_path": { "type": "string", "description": "Path to the file to read." },
                        "offset": { "type": ["integer", "null"], "minimum": 1, "default": 1, "description": "1-indexed start line (>= 1)." },
                        "limit": { "type": ["integer", "null"], "minimum": 1, "default": 200, "description": "Maximum number of lines to return (>= 1)." }
                    },
                    "required": ["file_path", "offset", "limit"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "Lists directory entries with pagination and depth control.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "dir_path": { "type": "string", "description": "Path to the directory to list." },
                        "offset": { "type": ["integer", "null"], "minimum": 1, "default": 1, "description": "1-indexed start entry (>= 1)." },
                        "limit": { "type": ["integer", "null"], "minimum": 1, "default": 200, "description": "Maximum number of entries to return (>= 1)." },
                        "depth": { "type": ["integer", "null"], "minimum": 1, "default": 1, "description": "Maximum directory depth to traverse (>= 1)." }
                    },
                    "required": ["dir_path", "offset", "limit", "depth"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "grep_files",
                "description": "Searches files for a pattern and returns matching lines.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Rust regex pattern to search for (escape metacharacters for literal matches)." },
                        "path": { "type": ["string", "null"], "description": "Root path to search." },
                        "include": { "type": ["string", "null"], "description": "Optional glob filter for files (matched against path relative to root)." },
                        "limit": { "type": ["integer", "null"], "minimum": 1, "default": 100, "description": "Maximum number of matches to return (>= 1)." }
                    },
                    "required": ["pattern", "path", "include", "limit"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "apply_patch",
                "description": "Applies a unified diff patch.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "patch": { "type": "string", "description": "Unified diff to apply." }
                    },
                    "required": ["patch"],
                    "additionalProperties": false
                }
            }
        }),
    ];

    if web_search_enabled {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Searches the web for up-to-date information (Tavily). Requires RA_TAVILY_API_KEY (or TAVILY_API_KEY).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query." },
                        "max_results": { "type": ["integer", "null"], "minimum": 1, "maximum": 10, "default": 5, "description": "Max results to return (1-10)." }
                    },
                    "required": ["query", "max_results"],
                    "additionalProperties": false
                }
            }
        }));
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "web_open",
                "description": "Fetches a URL and returns extracted text with line numbers (for quoting/citations).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "URL to fetch (http/https)." },
                        "offset": { "type": ["integer", "null"], "minimum": 1, "default": 1, "description": "1-indexed start line (>= 1)." },
                        "limit": { "type": ["integer", "null"], "minimum": 1, "default": 200, "description": "Maximum number of lines to return (>= 1)." }
                    },
                    "required": ["url", "offset", "limit"],
                    "additionalProperties": false
                }
            }
        }));
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "web_find",
                "description": "Finds occurrences of a pattern in the extracted text of a URL and returns matching line ranges (for citations).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "URL to fetch (http/https)." },
                        "pattern": { "type": "string", "description": "Case-insensitive substring to find." },
                        "max_results": { "type": ["integer", "null"], "minimum": 1, "maximum": 50, "default": 10, "description": "Max matches to return (1-50)." },
                        "context_lines": { "type": ["integer", "null"], "minimum": 0, "maximum": 10, "default": 2, "description": "Lines of context before/after each match (0-10)." }
                    },
                    "required": ["url", "pattern", "max_results", "context_lines"],
                    "additionalProperties": false
                }
            }
        }));
    }

    if submit_enabled {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "submit",
                "description": "Signals completion and returns the final answer.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "answer": { "type": "string", "description": "Final answer." }
                    },
                    "required": ["answer"],
                    "additionalProperties": false
                }
            }
        }));
    }

    tools
}

pub(crate) fn truncate(value: &str, limit: usize) -> (String, bool) {
    let mut out = String::new();
    for (count, ch) in value.chars().enumerate() {
        if count >= limit {
            break;
        }
        out.push(ch);
    }
    if value.chars().count() > limit {
        out.push_str("\n...[truncated]...");
        (out, true)
    } else {
        (out, false)
    }
}

#[derive(Deserialize)]
pub(crate) struct WebSearchArgs {
    pub(crate) query: String,
    pub(crate) max_results: Option<usize>,
}

fn web_search(args: &WebSearchArgs, max_output_chars: usize) -> Result<String> {
    let api_key = std::env::var("RA_TAVILY_API_KEY")
        .or_else(|_| std::env::var("TAVILY_API_KEY"))
        .ok();
    let Some(api_key) = api_key.filter(|k| !k.is_empty()) else {
        return Ok(tool_error(
            "web_search requires RA_TAVILY_API_KEY (or TAVILY_API_KEY)".to_string(),
        ));
    };

    let max_results = args.max_results.unwrap_or(5).clamp(1, 10);
    tavily_web_search(args, &api_key, max_results, max_output_chars)
}

fn tavily_web_search(
    args: &WebSearchArgs,
    api_key: &str,
    max_results: usize,
    max_output_chars: usize,
) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client for web_search")?;

    let tavily_base_url = std::env::var("RA_TAVILY_BASE_URL")
        .unwrap_or_else(|_| "https://api.tavily.com".to_string());
    let tavily_base_url = tavily_base_url.trim_end_matches('/').to_string();
    let endpoint = format!("{}/search", tavily_base_url);

    const MAX_RETRIES: usize = 2;
    let mut last_body: Option<String> = None;
    let mut last_status: Option<reqwest::StatusCode> = None;

    for attempt in 0..=MAX_RETRIES {
        let resp = client
            .post(&endpoint)
            .header(
                reqwest::header::USER_AGENT,
                format!("ra-cli/{}", env!("CARGO_PKG_VERSION")),
            )
            .json(&json!({
                "api_key": api_key,
                "query": args.query,
                "max_results": max_results,
                "search_depth": "basic",
                "include_answer": false,
                "include_raw_content": false,
                "include_images": false
            }))
            .send();

        let resp = match resp {
            Ok(r) => r,
            Err(err) => {
                if attempt < MAX_RETRIES && should_retry_reqwest_error(&err) {
                    sleep_backoff(attempt);
                    continue;
                }
                return Ok(tool_error(format!(
                    "web_search HTTP request failed: POST {}: {}",
                    endpoint, err
                )));
            }
        };

        let status = resp.status();
        let text = resp.text().unwrap_or_else(|_| "".to_string());
        last_body = Some(text.clone());
        last_status = Some(status);

        if status.is_success() {
            // Continue parsing below using `text`.
        } else {
            if attempt < MAX_RETRIES && should_retry_http_status(status) {
                sleep_backoff(attempt);
                continue;
            }
            let (snippet, _) = truncate(&text, 2000);
            return Ok(tool_error(format!(
                "web_search provider error (HTTP {}): {}",
                status.as_u16(),
                snippet
            )));
        }

        // Parse the successful body.
        #[derive(Deserialize)]
        struct TavilyResponse {
            #[serde(default)]
            results: Vec<TavilyResult>,
        }

        #[derive(Deserialize)]
        struct TavilyResult {
            #[serde(default)]
            title: Option<String>,
            url: String,
            #[serde(default)]
            content: Option<String>,
            #[serde(default)]
            score: Option<f64>,
        }

        let parsed: TavilyResponse = serde_json::from_str(&text).with_context(|| {
            let (snippet, _) = truncate(&text, 2000);
            format!("web_search provider returned invalid JSON: {}", snippet)
        })?;

        let results: Vec<Value> = parsed
            .results
            .into_iter()
            .take(max_results)
            .map(|r| {
                json!({
                    "title": r.title.unwrap_or_default(),
                    "url": r.url,
                    "content": r.content.unwrap_or_default(),
                    "score": r.score
                })
            })
            .collect();

        let out = json!({
            "provider": "tavily",
            "query": args.query,
            "max_results": max_results,
            "results": results
        })
        .to_string();
        let (out, truncated) = truncate(&out, max_output_chars);
        if truncated {
            return Ok(json!({
                "provider": "tavily",
                "query": args.query,
                "max_results": max_results,
                "results_truncated": true,
                "output": out
            })
            .to_string());
        }
        return Ok(out);
    }

    // Defensive: should have returned in loop.
    let status = last_status.map(|s| s.as_u16()).unwrap_or(0);
    let body = last_body.unwrap_or_default();
    let (snippet, _) = truncate(&body, 2000);
    Ok(tool_error(format!(
        "web_search failed after retries (HTTP {}): {}",
        status, snippet
    )))
}

#[derive(Deserialize)]
pub(crate) struct WebOpenArgs {
    pub(crate) url: String,
    pub(crate) offset: Option<usize>,
    pub(crate) limit: Option<usize>,
}

#[derive(Deserialize)]
pub(crate) struct WebFindArgs {
    pub(crate) url: String,
    pub(crate) pattern: String,
    pub(crate) max_results: Option<usize>,
    pub(crate) context_lines: Option<usize>,
}

fn web_open(args: &WebOpenArgs, max_output_chars: usize) -> Result<String> {
    let offset = args.offset.unwrap_or(1);
    let limit = args
        .limit
        .unwrap_or(DEFAULT_READ_LIMIT)
        .min(DEFAULT_READ_LIMIT);
    if offset < 1 || limit < 1 {
        return Ok(tool_error(
            "invalid pagination: web_open.offset and web_open.limit must be >= 1 (offset is 1-indexed)"
                .to_string(),
        ));
    }

    let (text, meta) = match fetch_url_as_text(&args.url) {
        Ok(v) => v,
        Err(err) => return Ok(tool_error(err.to_string())),
    };
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return Ok(json!({
            "url": args.url,
            "meta": meta,
            "total_lines": 0,
            "start_line": 0,
            "end_line": 0,
            "lines": []
        })
        .to_string());
    }
    if offset > total {
        return Ok(tool_error(format!(
            "offset ({offset}) is beyond total lines ({total})"
        )));
    }
    let end = (offset + limit - 1).min(total);
    let mut numbered = Vec::new();
    for (idx, line) in lines[offset - 1..end].iter().enumerate() {
        numbered.push(format!("{}: {}", idx + offset, line));
    }
    let out = json!({
        "url": args.url,
        "meta": meta,
        "total_lines": total,
        "start_line": offset,
        "end_line": end,
        "lines": numbered
    })
    .to_string();
    let (out, truncated) = truncate(&out, max_output_chars);
    if truncated {
        Ok(json!({"truncated": true, "output": out}).to_string())
    } else {
        Ok(out)
    }
}

fn web_find(args: &WebFindArgs, max_output_chars: usize) -> Result<String> {
    let pattern = args.pattern.trim().to_string();
    if pattern.is_empty() {
        return Ok(tool_error("web_find.pattern must be non-empty".to_string()));
    }
    let max_results = args.max_results.unwrap_or(10).clamp(1, 50);
    let context_lines = args.context_lines.unwrap_or(2).clamp(0, 10);

    let (text, meta) = match fetch_url_as_text(&args.url) {
        Ok(v) => v,
        Err(err) => return Ok(tool_error(err.to_string())),
    };
    let lines: Vec<&str> = text.lines().collect();
    let needle = pattern.to_lowercase();

    let mut matches = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if matches.len() >= max_results {
            break;
        }
        if line.to_lowercase().contains(&needle) {
            let line_no = i + 1;
            let start = line_no.saturating_sub(context_lines);
            let start = start.max(1);
            let end = (line_no + context_lines).min(lines.len());
            let mut snippet = Vec::new();
            for j in start..=end {
                snippet.push(format!("{}: {}", j, lines[j - 1]));
            }
            matches.push(json!({
                "start_line": start,
                "end_line": end,
                "match_line": line_no,
                "snippet": snippet,
            }));
        }
    }

    let out = json!({
        "url": args.url,
        "meta": meta,
        "pattern": pattern,
        "matches": matches,
        "truncated_matches": matches.len() >= max_results
    })
    .to_string();
    let (out, truncated) = truncate(&out, max_output_chars);
    if truncated {
        Ok(json!({"truncated": true, "output": out}).to_string())
    } else {
        Ok(out)
    }
}

fn fetch_url_as_text(url: &str) -> Result<(String, Value)> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(anyhow!(
            "web_* only supports http:// or https:// URLs (got: {})",
            url
        ));
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client for web_open/web_find")?;

    const MAX_RETRIES: usize = 2;
    let mut last_status: Option<reqwest::StatusCode> = None;
    let mut last_body: Option<String> = None;
    let mut last_content_type: Option<String> = None;
    let mut last_content_length: Option<u64> = None;

    for attempt in 0..=MAX_RETRIES {
        let resp = client
            .get(url)
            .header(
                reqwest::header::USER_AGENT,
                format!("ra-cli/{}", env!("CARGO_PKG_VERSION")),
            )
            .send();

        let resp = match resp {
            Ok(r) => r,
            Err(err) => {
                if attempt < MAX_RETRIES && should_retry_reqwest_error(&err) {
                    sleep_backoff(attempt);
                    continue;
                }
                return Err(anyhow!("GET {} failed: {}", url, err));
            }
        };

        let status = resp.status();
        let headers = resp.headers().clone();
        let content_type = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let content_length = headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());

        let max_bytes = web_max_bytes();
        let truncated_by_limit = content_length.is_some_and(|len| len > max_bytes);
        let mut buf = Vec::new();
        let _ = resp.take(max_bytes).read_to_end(&mut buf);
        let body = String::from_utf8_lossy(&buf).to_string();

        last_status = Some(status);
        last_body = Some(body.clone());
        last_content_type = Some(content_type.clone());
        last_content_length = content_length;

        if status.is_success() {
            // Continue parsing below.
            let is_html = content_type.contains("text/html")
                || content_type.contains("application/xhtml")
                || body.contains("<html");
            let is_text = content_type.starts_with("text/") || content_type.is_empty();
            let is_json = content_type.starts_with("application/json");

            if !(is_html || is_text || is_json) {
                return Err(anyhow!(
                    "unsupported content type for web_open/web_find: {} (url: {})",
                    if content_type.is_empty() {
                        "<missing content-type>"
                    } else {
                        content_type.as_str()
                    },
                    url
                ));
            }

            let text = if is_html { html_to_text(&body) } else { body };
            return Ok((
                normalize_text(&text),
                json!({
                    "status": status.as_u16(),
                    "content_type": content_type,
                    "content_length": content_length,
                    "max_bytes": max_bytes,
                    "truncated": truncated_by_limit
                }),
            ));
        }

        if attempt < MAX_RETRIES && should_retry_http_status(status) {
            sleep_backoff(attempt);
            continue;
        }

        // Non-success final: return structured meta+error like before.
        return Ok((
            String::new(),
            json!({
                "status": status.as_u16(),
                "content_type": content_type,
                "content_length": content_length,
                "max_bytes": max_bytes,
                "truncated": truncated_by_limit,
                "error": truncate(&body, 2000).0
            }),
        ));
    }

    // Defensive: should have returned above.
    let status = last_status.map(|s| s.as_u16()).unwrap_or(0);
    let content_type = last_content_type.unwrap_or_default();
    let content_length = last_content_length;
    let max_bytes = web_max_bytes();
    let body = last_body.unwrap_or_default();
    Ok((
        String::new(),
        json!({
            "status": status,
            "content_type": content_type,
            "content_length": content_length,
            "max_bytes": max_bytes,
            "truncated": content_length.is_some_and(|len| len > max_bytes),
            "error": truncate(&body, 2000).0
        }),
    ))
}

fn html_to_text(html: &str) -> String {
    // Minimal HTML-to-text for a baseline agent (no external deps).
    // This is intentionally simple: good enough for citations/line numbers, not perfect rendering.
    let re_script = Regex::new(r"(?is)<script[^>]*>.*?</script>").unwrap();
    let re_style = Regex::new(r"(?is)<style[^>]*>.*?</style>").unwrap();
    let re_nav = Regex::new(r"(?is)<nav[^>]*>.*?</nav>").unwrap();
    let re_header = Regex::new(r"(?is)<header[^>]*>.*?</header>").unwrap();
    let re_footer = Regex::new(r"(?is)<footer[^>]*>.*?</footer>").unwrap();
    let re_aside = Regex::new(r"(?is)<aside[^>]*>.*?</aside>").unwrap();

    let mut s = re_script.replace_all(html, "\n").to_string();
    s = re_style.replace_all(&s, "\n").to_string();
    // Remove common boilerplate containers so the model doesn't waste steps paging through nav.
    s = re_nav.replace_all(&s, "\n").to_string();
    s = re_header.replace_all(&s, "\n").to_string();
    s = re_footer.replace_all(&s, "\n").to_string();
    s = re_aside.replace_all(&s, "\n").to_string();

    // Add newlines for some block-ish tags to preserve structure a bit.
    let re_breaks = Regex::new(r"(?i)</(p|div|h1|h2|h3|h4|h5|h6|li|tr|br)\s*>").unwrap();
    s = re_breaks.replace_all(&s, "\n").to_string();

    let re_tags = Regex::new(r"(?is)<[^>]+>").unwrap();
    s = re_tags.replace_all(&s, "").to_string();
    s
}

fn normalize_text(text: &str) -> String {
    // Collapse excessive blank lines and trim trailing spaces.
    let mut out_lines = Vec::new();
    let mut last_blank = false;
    for line in text.lines() {
        let line = line.trim_end().to_string();
        let blank = line.trim().is_empty();
        if blank {
            if last_blank {
                continue;
            }
            last_blank = true;
            out_lines.push(String::new());
        } else {
            last_blank = false;
            out_lines.push(line);
        }
    }
    out_lines.join("\n")
}

#[derive(Deserialize)]
pub(crate) struct ShellArgs {
    pub(crate) command: String,
    pub(crate) workdir: Option<String>,
    pub(crate) timeout_ms: Option<u64>,
    pub(crate) max_output_chars: Option<usize>,
}

fn run_shell_command(args: &ShellArgs, cwd: &Path, max_output_chars: usize) -> Result<String> {
    let workdir = args
        .workdir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| cwd.to_path_buf());
    let workdir = resolve_path(cwd, &workdir);

    let mut child = Command::new("bash")
        .arg("-lc")
        .arg(&args.command)
        .current_dir(&workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn shell command in {}", workdir.display()))?;

    let mut timed_out = false;
    if let Some(timeout_ms) = args.timeout_ms {
        let timeout = Duration::from_millis(timeout_ms);
        if child.wait_timeout(timeout)?.is_none() {
            timed_out = true;
            let _ = child.kill();
        }
    }

    let output = child.wait_with_output()?;
    let limit = args.max_output_chars.unwrap_or(max_output_chars);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let (stdout, stdout_truncated) = truncate(&stdout, limit);
    let (stderr, stderr_truncated) = truncate(&stderr, limit);
    let truncated = stdout_truncated || stderr_truncated;

    let result = json!({
        "exit_code": output.status.code().unwrap_or(-1),
        "stdout": stdout,
        "stderr": stderr,
        "timed_out": timed_out,
        "truncated": truncated,
    });
    Ok(result.to_string())
}

#[derive(Deserialize)]
pub(crate) struct ReadFileArgs {
    pub(crate) file_path: String,
    pub(crate) offset: Option<usize>,
    pub(crate) limit: Option<usize>,
}

fn read_file(args: &ReadFileArgs, cwd: &Path) -> Result<String> {
    let offset = args.offset.unwrap_or(1);
    let limit = args
        .limit
        .unwrap_or(DEFAULT_READ_LIMIT)
        .min(DEFAULT_READ_LIMIT);
    if offset < 1 || limit < 1 {
        return Ok(tool_error(
            "invalid pagination: read_file.offset and read_file.limit must be >= 1 (offset is 1-indexed)"
                .to_string(),
        ));
    }

    let path = resolve_path(cwd, Path::new(&args.file_path));
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read file {}", path.display()))?;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if offset > total {
        return Ok(tool_error(format!(
            "offset ({}) is beyond total lines ({})",
            offset, total
        )));
    }
    let end = (offset + limit - 1).min(total);
    let mut numbered = Vec::new();
    for (idx, line) in lines[offset - 1..end].iter().enumerate() {
        numbered.push(format!("{}: {}", idx + offset, line));
    }
    let result = json!({
        "file_path": path.display().to_string(),
        "total_lines": total,
        "start_line": offset,
        "end_line": end,
        "lines": numbered,
    });
    Ok(result.to_string())
}

#[derive(Deserialize)]
pub(crate) struct ListDirArgs {
    pub(crate) dir_path: String,
    pub(crate) offset: Option<usize>,
    pub(crate) limit: Option<usize>,
    pub(crate) depth: Option<usize>,
}

fn list_dir(args: &ListDirArgs, cwd: &Path) -> Result<String> {
    let offset = args.offset.unwrap_or(1);
    let limit = args
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .min(DEFAULT_LIST_LIMIT);
    let depth = args.depth.unwrap_or(1);
    if offset < 1 || limit < 1 || depth < 1 {
        return Ok(tool_error(
            "invalid pagination: list_dir.offset, list_dir.limit, and list_dir.depth must be >= 1 (offset is 1-indexed)"
                .to_string(),
        ));
    }

    let dir = resolve_path(cwd, Path::new(&args.dir_path));
    let mut entries = Vec::new();
    for entry in WalkDir::new(&dir).max_depth(depth) {
        let entry = entry?;
        if entry.depth() == 0 {
            continue;
        }
        let entry_type = if entry.file_type().is_dir() {
            "dir"
        } else {
            "file"
        };
        entries.push(json!({
            "path": entry.path().display().to_string(),
            "type": entry_type,
        }));
    }
    entries.sort_by(|a, b| {
        let a_path = a.get("path").and_then(Value::as_str).unwrap_or("");
        let b_path = b.get("path").and_then(Value::as_str).unwrap_or("");
        a_path.cmp(b_path)
    });

    let total = entries.len();
    if offset > total && total > 0 {
        return Ok(tool_error(format!(
            "offset ({}) is beyond total entries ({})",
            offset, total
        )));
    }
    let end = (offset + limit - 1).min(total);
    let slice = if total == 0 {
        Vec::new()
    } else {
        entries[offset - 1..end].to_vec()
    };

    let result = json!({
        "dir_path": dir.display().to_string(),
        "total_entries": total,
        "start_index": if total == 0 { 0 } else { offset },
        "end_index": if total == 0 { 0 } else { end },
        "entries": slice,
    });
    Ok(result.to_string())
}

#[derive(Deserialize)]
pub(crate) struct GrepFilesArgs {
    pub(crate) pattern: String,
    pub(crate) path: Option<String>,
    pub(crate) include: Option<String>,
    pub(crate) limit: Option<usize>,
}

fn grep_files(args: &GrepFilesArgs, cwd: &Path) -> Result<String> {
    let limit = args
        .limit
        .unwrap_or(DEFAULT_GREP_LIMIT)
        .min(DEFAULT_GREP_LIMIT);
    if limit < 1 {
        return Ok(tool_error(
            "invalid limit: grep_files.limit must be >= 1".to_string(),
        ));
    }

    let pattern = match Regex::new(&args.pattern) {
        Ok(p) => p,
        Err(err) => {
            return Ok(tool_error(format!(
                "invalid regex pattern: {}: {} (tip: escape metacharacters for literal matches, e.g. \"main\\\\(\" to match \"main(\")",
                args.pattern, err
            )));
        }
    };

    let globset = if let Some(include) = &args.include {
        let glob = Glob::new(include).context("invalid include glob")?;
        let mut builder = GlobSetBuilder::new();
        builder.add(glob);
        Some(builder.build()?)
    } else {
        None
    };

    let root = args
        .path
        .as_ref()
        .map(|p| resolve_path(cwd, Path::new(p)))
        .unwrap_or_else(|| cwd.to_path_buf());

    let mut matches = Vec::new();
    let mut truncated = false;
    for entry in WalkDir::new(&root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        if let Some(ref set) = globset {
            // Match include globs against the path relative to the search root so callers can
            // use patterns like "ra/src/main.rs" or "**/*.rs" without needing absolute paths.
            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            if !set.is_match(rel) {
                continue;
            }
        }
        let content = match fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for (idx, line) in content.lines().enumerate() {
            if pattern.is_match(line) {
                matches.push(json!({
                    "path": entry.path().display().to_string(),
                    "line": idx + 1,
                    "text": line,
                }));
                if matches.len() >= limit {
                    truncated = true;
                    break;
                }
            }
        }
        if matches.len() >= limit {
            break;
        }
    }

    let result = json!({
        "pattern": args.pattern.clone(),
        "root": root.display().to_string(),
        "matches": matches,
        "truncated": truncated,
    });
    Ok(result.to_string())
}

#[derive(Deserialize)]
pub(crate) struct ApplyPatchArgs {
    pub(crate) patch: String,
}

fn apply_patch(args: &ApplyPatchArgs, cwd: &Path, max_output_chars: usize) -> Result<String> {
    let strip_level = detect_patch_strip_level(&args.patch);
    let mut child = Command::new("patch")
        .arg(format!("-p{}", strip_level))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn patch command")?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(args.patch.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let (stdout, stdout_truncated) = truncate(&stdout, max_output_chars);
    let (stderr, stderr_truncated) = truncate(&stderr, max_output_chars);
    let truncated = stdout_truncated || stderr_truncated;

    let result = json!({
        "strip_level": strip_level,
        "exit_code": output.status.code().unwrap_or(-1),
        "stdout": stdout,
        "stderr": stderr,
        "truncated": truncated,
    });
    Ok(result.to_string())
}

fn detect_patch_strip_level(patch: &str) -> usize {
    // Most patches generated by git include a/ and b/ prefixes in file paths and require -p1.
    // Plain unified diffs without those prefixes typically require -p0.
    for line in patch.lines() {
        if line.starts_with("diff --git a/") {
            return 1;
        }
        if line.starts_with("--- a/") || line.starts_with("+++ a/") {
            return 1;
        }
        if line.starts_with("--- b/") || line.starts_with("+++ b/") {
            return 1;
        }
    }
    0
}

pub(crate) fn parse_patch_changes(patch: &str) -> Vec<Value> {
    let mut changes = Vec::new();
    let mut seen = HashSet::new();
    let mut old_path: Option<String> = None;

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            old_path = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            let new_path = rest.trim().to_string();
            if let Some(old) = old_path.take() {
                let (kind, raw_path) = if old == "/dev/null" {
                    ("add", new_path)
                } else if new_path == "/dev/null" {
                    ("delete", old)
                } else {
                    ("update", new_path)
                };
                let path = strip_patch_prefix(&raw_path);
                if !path.is_empty() && seen.insert(path.clone()) {
                    changes.push(json!({
                        "path": path,
                        "kind": kind,
                    }));
                }
            }
        }
    }

    changes
}

fn strip_patch_prefix(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed == "/dev/null" {
        return String::new();
    }
    trimmed
        .strip_prefix("a/")
        .or_else(|| trimmed.strip_prefix("b/"))
        .unwrap_or(trimmed)
        .to_string()
}
