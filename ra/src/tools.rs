use crate::constants::{DEFAULT_GREP_LIMIT, DEFAULT_LIST_LIMIT, DEFAULT_READ_LIMIT};
use crate::protocol::ToolCall;
use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use wait_timeout::ChildExt;
use walkdir::WalkDir;

pub(crate) fn execute_tool(
    tool_call: &ToolCall,
    cwd: &Path,
    max_output_chars: usize,
) -> Result<String> {
    let args: Value = serde_json::from_str(&tool_call.function.arguments)?;
    match tool_call.function.name.as_str() {
        "shell_command" => {
            let args: ShellArgs = serde_json::from_value(args)?;
            run_shell_command(&args, cwd, max_output_chars)
        }
        "read_file" => {
            let args: ReadFileArgs = serde_json::from_value(args)?;
            read_file(&args, cwd)
        }
        "list_dir" => {
            let args: ListDirArgs = serde_json::from_value(args)?;
            list_dir(&args, cwd)
        }
        "grep_files" => {
            let args: GrepFilesArgs = serde_json::from_value(args)?;
            grep_files(&args, cwd)
        }
        "apply_patch" => {
            let args: ApplyPatchArgs = serde_json::from_value(args)?;
            apply_patch(&args, cwd, max_output_chars)
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

pub(crate) fn build_tools(submit_enabled: bool) -> Vec<Value> {
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
