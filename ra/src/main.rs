use anyhow::{anyhow, bail, Context, Result};
use clap::{ArgGroup, CommandFactory, Parser};
use globset::{Glob, GlobSetBuilder};
use regex::Regex;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;
use wait_timeout::ChildExt;
use walkdir::WalkDir;

const DEFAULT_CONTINUE_MESSAGE: &str = "Please proceed to the next step using your best judgement. If you believe you are finished, double check your work to continue to refine and improve your submission.";
const DEFAULT_MAX_TOOL_OUTPUT_CHARS: usize = 8000;
const DEFAULT_READ_LIMIT: usize = 200;
const DEFAULT_LIST_LIMIT: usize = 200;
const DEFAULT_GREP_LIMIT: usize = 100;

#[derive(Parser, Debug)]
#[command(group(
    ArgGroup::new("task_input")
        .required(true)
        .args(["prompt_file", "prompt"])
))]
#[command(group(
    ArgGroup::new("exec_mode")
        .required(false)
        .args(["exec", "no_submit"])
))]
#[command(
    about = "Ra is a baseline ReAct CLI agent for OpenRouter-compatible models.",
    long_about = "Ra is a baseline ReAct CLI agent for OpenRouter-compatible models."
)]
struct Args {
    #[arg(
        long,
        default_value = "openai/gpt-4.1-mini",
        env = "RA_DEFAULT_MODEL",
        help = "Model ID to use (OpenRouter format)."
    )]
    model: String,

    #[arg(long, value_name = "FILE", help = "Read the prompt from a file.")]
    prompt_file: Option<PathBuf>,

    #[arg(
        long,
        value_name = "DIR",
        default_value = ".",
        help = "Working directory for file and shell tools."
    )]
    cwd: PathBuf,

    #[arg(long, help = "OpenRouter API key (overrides OPENROUTER_API_KEY).")]
    api_key: Option<String>,

    #[arg(
        long,
        default_value = "https://openrouter.ai/api/v1",
        help = "OpenRouter API base URL."
    )]
    base_url: String,

    #[arg(long, help = "Sampling temperature (omit to use provider default).")]
    temperature: Option<f64>,

    #[arg(long, help = "Maximum number of tool steps before terminating.")]
    max_steps: Option<usize>,

    #[arg(long, help = "Time limit in seconds before terminating.")]
    time_limit_sec: Option<u64>,

    #[arg(long, help = "Directory to write the JSONL log file.")]
    log_dir: Option<PathBuf>,

    #[arg(long, help = "Maximum tool output characters to retain.")]
    max_tool_output_chars: Option<usize>,

    #[arg(
        long,
        default_value_t = false,
        help = "Force agent/exec mode (enable submit tool and continue until submit is called)."
    )]
    exec: bool,

    #[arg(
        long,
        default_value_t = false,
        help = "Force disabling the submit tool and stop on the first assistant response without tool calls."
    )]
    no_submit: bool,

    #[arg(value_name = "PROMPT", help = "Prompt text (quote for spaces).")]
    prompt: Option<String>,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
}

#[derive(Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ToolFunction,
}

#[derive(Clone, Serialize, Deserialize)]
struct ToolFunction {
    name: String,
    arguments: String,
}

#[derive(Clone, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Clone, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<i64>,
}

#[derive(Clone, Deserialize)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<i64>,
}

#[derive(Deserialize)]
struct ApiErrorResponse {
    error: ApiErrorDetail,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    message: String,
}

#[derive(Clone, Default, Serialize)]
struct TokenUsage {
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
    total_tokens: i64,
}

impl TokenUsage {
    fn add_assign(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_output_tokens += other.reasoning_output_tokens;
        self.total_tokens += other.total_tokens;
    }
}

struct CompletionResult {
    message: ChatMessage,
    usage: Option<Usage>,
}

type ToolLogging = (Option<(String, String)>, Vec<Value>);

struct Logger {
    writer: BufWriter<File>,
}

impl Logger {
    fn new(log_path: PathBuf) -> Result<Self> {
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log directory {}", parent.display()))?;
        }
        let file = File::create(&log_path)
            .with_context(|| format!("failed to create log file {}", log_path.display()))?;
        Ok(Self {
            writer: BufWriter::new(file),
        })
    }

    fn log_event(&mut self, event: &Value) -> Result<()> {
        serde_json::to_writer(&mut self.writer, event)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }
}

struct Agent {
    client: Client,
    base_url: String,
    model: String,
    api_key: String,
    session_id: String,
    tools: Vec<Value>,
    messages: Vec<Value>,
    temperature: Option<f64>,
    max_steps: Option<usize>,
    time_limit: Option<Duration>,
    max_tool_output_chars: usize,
    cwd: PathBuf,
    submit_enabled: bool,
    logger: Logger,
    token_usage_total: TokenUsage,
    next_item_id: u64,
}

impl Agent {
    fn run(&mut self, task: String) -> Result<String> {
        let start = Instant::now();
        let mut steps = 0usize;

        let (system_prompt, agents_text) = build_system_prompt(
            &self.cwd,
            self.max_steps,
            self.time_limit,
            self.submit_enabled,
        )?;
        self.log_thread_started()?;
        self.log_turn_started(&task, &system_prompt, agents_text.as_deref())?;

        self.messages
            .push(json!({"role": "system", "content": system_prompt}));
        let user_message = json!({"role": "user", "content": task});
        self.messages.push(user_message);

        loop {
            if let Some(max_steps) = self.max_steps {
                if steps >= max_steps {
                    let message = format!("Terminated: max_steps ({}) reached.", max_steps);
                    self.log_warning_item(&message)?;
                    self.log_turn_completed()?;
                    return Ok(message);
                }
            }
            if let Some(limit) = self.time_limit {
                if start.elapsed() >= limit {
                    let message = "Terminated: time_limit reached.".to_string();
                    self.log_warning_item(&message)?;
                    self.log_turn_completed()?;
                    return Ok(message);
                }
            }

            steps += 1;
            let request = self.build_request()?;
            let completion = match self.send_request(&request) {
                Ok(result) => result,
                Err(err) => {
                    let err_msg = err.to_string();
                    if !is_context_error(&err_msg) {
                        self.log_error_event(&err_msg)?;
                        self.log_turn_failed(&err_msg)?;
                        return Err(err);
                    }

                    // Context overflow: prune like basicagent (keep system + initial user task +
                    // tool-call/response pairs) and retry. We may need to prune multiple times
                    // depending on model context size and tool output volume.
                    let mut last_len = self.messages.len();
                    let mut recovered: Option<CompletionResult> = None;
                    loop {
                        self.messages = prune_messages(&self.messages);
                        let new_len = self.messages.len();
                        if new_len >= last_len {
                            break;
                        }
                        last_len = new_len;

                        let request = self.build_request()?;
                        match self.send_request(&request) {
                            Ok(result) => {
                                recovered = Some(result);
                                break;
                            }
                            Err(err) => {
                                if is_context_error(&err.to_string()) {
                                    continue;
                                }
                                let msg = err.to_string();
                                self.log_error_event(&msg)?;
                                self.log_turn_failed(&msg)?;
                                return Err(err);
                            }
                        }
                    }

                    if let Some(result) = recovered {
                        result
                    } else {
                        let message = "Terminated: context length exceeded.".to_string();
                        self.log_warning_item(&message)?;
                        self.log_turn_completed()?;
                        return Ok(message);
                    }
                }
            };

            let message = completion.message;
            let content_text = message.content.clone().unwrap_or_default();
            let mut assistant = json!({
                "role": "assistant",
                "content": message.content,
            });
            if let Some(tool_calls) = &message.tool_calls {
                assistant["tool_calls"] = serde_json::to_value(tool_calls)?;
            }
            if !content_text.trim().is_empty() {
                self.log_agent_message(&content_text)?;
            }
            self.messages.push(assistant);

            if let Some(usage) = completion.usage {
                self.update_usage(&usage);
            }

            let tool_calls = message.tool_calls.clone().unwrap_or_default();
            if !tool_calls.is_empty() {
                let mut first = true;
                for tool_call in tool_calls {
                    if first {
                        first = false;
                        if tool_call.function.name == "submit" && self.submit_enabled {
                            let answer = parse_submit_answer(&tool_call.function.arguments)?;
                            if !answer.trim().is_empty() {
                                self.log_agent_message(&answer)?;
                            }
                            self.log_turn_completed()?;
                            return Ok(answer);
                        }

                        let tool_name = tool_call.function.name.as_str();
                        let (command_item, file_changes) =
                            self.prepare_tool_logging(tool_name, &tool_call.function.arguments)?;
                        if let Some((item_id, command)) = &command_item {
                            self.log_command_execution_started(item_id, command)?;
                        }

                        let result =
                            execute_tool(&tool_call, &self.cwd, self.max_tool_output_chars);
                        let success = result.is_ok();
                        let content = match result {
                            Ok(value) => value,
                            Err(err) => tool_error(format!("{err:#}")),
                        };
                        self.messages.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_call.id,
                            "content": content,
                        }));
                        self.log_tool_result(
                            tool_name,
                            command_item,
                            file_changes,
                            &content,
                            success,
                        )?;
                    } else {
                        let content =
                            tool_error("Multiple tool calls in one step are not supported.".into());
                        self.messages.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_call.id,
                            "content": content,
                        }));
                        self.log_warning_item(
                            "Multiple tool calls in one step are not supported.",
                        )?;
                    }
                }
                continue;
            }

            if self.submit_enabled {
                let continue_message = json!({
                    "role": "user",
                    "content": DEFAULT_CONTINUE_MESSAGE,
                });
                self.messages.push(continue_message);
                continue;
            }

            let final_text = message.content.unwrap_or_default();
            self.log_turn_completed()?;
            return Ok(final_text);
        }
    }

    fn build_request(&self) -> Result<Value> {
        let mut body = json!({
            "model": self.model,
            "messages": self.messages,
            "tools": self.tools,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
        });
        if let Some(temp) = self.temperature {
            body["temperature"] = json!(temp);
        }
        Ok(body)
    }

    fn send_request(&self, request: &Value) -> Result<CompletionResult> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(request)
            .send()
            .context("request failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let err: ApiErrorResponse = response.json().context("error response decode failed")?;
            let message = format!("{} (HTTP {})", err.error.message, status);
            return Err(anyhow!(message));
        }

        let parsed: ChatCompletionResponse = response.json().context("response decode failed")?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no choices in response"))?;
        Ok(CompletionResult {
            message: choice.message,
            usage: parsed.usage,
        })
    }

    fn log_thread_started(&mut self) -> Result<()> {
        let event = json!({
            "type": "thread.started",
            "thread_id": self.session_id.clone(),
        });
        self.logger.log_event(&event)
    }

    fn log_turn_started(
        &mut self,
        prompt: &str,
        system_prompt: &str,
        agents_text: Option<&str>,
    ) -> Result<()> {
        let mut event = json!({
            "type": "turn.started",
            "prompt": prompt,
            "system_prompt": system_prompt,
        });
        if let Some(text) = agents_text {
            event["agents_instructions"] = json!(text);
        }
        self.logger.log_event(&event)
    }

    fn log_turn_completed(&mut self) -> Result<()> {
        let usage = json!({
            "input_tokens": self.token_usage_total.input_tokens,
            "cached_input_tokens": self.token_usage_total.cached_input_tokens,
            "output_tokens": self.token_usage_total.output_tokens,
        });
        let event = json!({
            "type": "turn.completed",
            "usage": usage,
        });
        self.logger.log_event(&event)
    }

    fn log_turn_failed(&mut self, message: &str) -> Result<()> {
        let event = json!({
            "type": "turn.failed",
            "error": {
                "message": message,
            }
        });
        self.logger.log_event(&event)
    }

    fn log_error_event(&mut self, message: &str) -> Result<()> {
        let event = json!({
            "type": "error",
            "message": message,
        });
        self.logger.log_event(&event)
    }

    fn log_warning_item(&mut self, message: &str) -> Result<()> {
        let item = json!({
            "id": self.next_item_id(),
            "type": "error",
            "message": message,
        });
        self.log_item_completed(item)
    }

    fn log_agent_message(&mut self, text: &str) -> Result<()> {
        let item = json!({
            "id": self.next_item_id(),
            "type": "agent_message",
            "text": text,
        });
        self.log_item_completed(item)
    }

    fn log_command_execution_started(&mut self, item_id: &str, command: &str) -> Result<()> {
        let item = json!({
            "id": item_id,
            "type": "command_execution",
            "command": command,
            "aggregated_output": "",
            "exit_code": null,
            "status": "in_progress",
        });
        self.log_item_started(item)
    }

    fn log_command_execution_completed(
        &mut self,
        item_id: &str,
        command: &str,
        aggregated_output: &str,
        exit_code: Option<i32>,
        success: bool,
    ) -> Result<()> {
        let status = if success { "completed" } else { "failed" };
        let item = json!({
            "id": item_id,
            "type": "command_execution",
            "command": command,
            "aggregated_output": aggregated_output,
            "exit_code": exit_code,
            "status": status,
        });
        self.log_item_completed(item)
    }

    fn prepare_tool_logging(&mut self, tool_name: &str, arguments: &str) -> Result<ToolLogging> {
        if tool_name == "apply_patch" {
            let patch = serde_json::from_str::<ApplyPatchArgs>(arguments)
                .map(|args| args.patch)
                .unwrap_or_default();
            let changes = parse_patch_changes(&patch);
            Ok((None, changes))
        } else {
            let command = tool_command_string(tool_name, arguments);
            let item_id = self.next_item_id();
            Ok((Some((item_id, command)), Vec::new()))
        }
    }

    fn log_tool_result(
        &mut self,
        tool_name: &str,
        command_item: Option<(String, String)>,
        file_changes: Vec<Value>,
        content: &str,
        success: bool,
    ) -> Result<()> {
        if tool_name == "apply_patch" {
            let status = parse_command_output(content)
                .map(|(code, _)| code == 0)
                .unwrap_or_else(|| success && !output_is_error_json(content));
            let item = json!({
                "id": self.next_item_id(),
                "type": "file_change",
                "changes": file_changes,
                "status": if status { "completed" } else { "failed" },
            });
            return self.log_item_completed(item);
        }

        let (item_id, command) = command_item
            .unwrap_or_else(|| (self.next_item_id(), tool_command_string(tool_name, "")));

        let (exit_code, aggregated_output, status) = if tool_name == "shell_command" {
            match parse_command_output(content) {
                Some((code, output)) => (Some(code), output, code == 0),
                None => (None, content.to_string(), success),
            }
        } else {
            let is_error = output_is_error_json(content);
            let status = success && !is_error;
            let code = if status { Some(0) } else { Some(1) };
            (code, content.to_string(), status)
        };

        self.log_command_execution_completed(
            &item_id,
            &command,
            &aggregated_output,
            exit_code,
            status,
        )
    }

    fn log_item_started(&mut self, item: Value) -> Result<()> {
        let event = json!({
            "type": "item.started",
            "item": item,
        });
        self.logger.log_event(&event)
    }

    fn log_item_completed(&mut self, item: Value) -> Result<()> {
        let event = json!({
            "type": "item.completed",
            "item": item,
        });
        self.logger.log_event(&event)
    }

    fn update_usage(&mut self, usage: &Usage) {
        let last_usage = token_usage_from_usage(usage);
        self.token_usage_total.add_assign(&last_usage);
    }

    fn next_item_id(&mut self) -> String {
        let id = format!("item_{}", self.next_item_id);
        self.next_item_id += 1;
        id
    }
}

fn token_usage_from_usage(usage: &Usage) -> TokenUsage {
    let cached = usage
        .prompt_tokens_details
        .as_ref()
        .and_then(|details| details.cached_tokens)
        .unwrap_or(0);
    let reasoning = usage
        .completion_tokens_details
        .as_ref()
        .and_then(|details| details.reasoning_tokens)
        .unwrap_or(0);
    let total = if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.prompt_tokens + usage.completion_tokens
    };
    TokenUsage {
        input_tokens: usage.prompt_tokens,
        cached_input_tokens: cached,
        output_tokens: usage.completion_tokens,
        reasoning_output_tokens: reasoning,
        total_tokens: total,
    }
}

fn tool_command_string(tool_name: &str, arguments: &str) -> String {
    if tool_name == "shell_command" {
        if let Ok(args) = serde_json::from_str::<ShellArgs>(arguments) {
            return format!("bash -lc {}", args.command);
        }
        return format!("bash -lc {}", arguments);
    }
    if arguments.trim().is_empty() {
        format!("tool:{}", tool_name)
    } else {
        format!("tool:{} {}", tool_name, arguments)
    }
}

fn parse_command_output(output: &str) -> Option<(i32, String)> {
    #[derive(Deserialize)]
    struct CommandOutput {
        exit_code: i32,
        #[serde(default)]
        stdout: String,
        #[serde(default)]
        stderr: String,
    }

    let parsed: CommandOutput = serde_json::from_str(output).ok()?;
    let mut aggregated = parsed.stdout;
    if !parsed.stderr.trim().is_empty() {
        if !aggregated.is_empty() {
            aggregated.push('\n');
        }
        aggregated.push_str(&parsed.stderr);
    }
    Some((parsed.exit_code, aggregated))
}

fn output_is_error_json(output: &str) -> bool {
    match serde_json::from_str::<Value>(output) {
        Ok(Value::Object(map)) => map.contains_key("error"),
        _ => false,
    }
}

fn parse_patch_changes(patch: &str) -> Vec<Value> {
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

fn load_task(args: &Args) -> Result<String> {
    if let Some(prompt_file) = &args.prompt_file {
        let mut buf = String::new();
        File::open(prompt_file)
            .with_context(|| format!("failed to open prompt file {}", prompt_file.display()))?
            .read_to_string(&mut buf)
            .with_context(|| format!("failed to read prompt file {}", prompt_file.display()))?;
        return Ok(buf);
    }
    if let Some(prompt) = &args.prompt {
        return Ok(prompt.clone());
    }
    bail!("prompt or prompt_file is required")
}

fn build_system_prompt(
    cwd: &Path,
    max_steps: Option<usize>,
    time_limit: Option<Duration>,
    submit_enabled: bool,
) -> Result<(String, Option<String>)> {
    let mut prompt = String::from(
        "You are a CLI agent. Use tools to inspect and modify the workspace to complete the task.\n\
Rules:\n\
- Use at most one tool call per step.\n\
- Prefer tools over guessing. Tool outputs are authoritative.",
    );
    if submit_enabled {
        prompt.push_str("\n- If you are done, call submit with a concise final answer.");
    } else {
        prompt.push_str("\n- If you are done, respond with a concise final answer.");
    }

    let max_steps_str = max_steps
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unset".to_string());
    let time_limit_str = time_limit
        .map(|v| v.as_secs().to_string())
        .unwrap_or_else(|| "unset".to_string());

    prompt.push_str(&format!(
        "\nEnvironment:\n- cwd: {}\n- max_steps: {}\n- time_limit_sec: {}\n- network_access: enabled\n- sandbox: none",
        cwd.display(),
        max_steps_str,
        time_limit_str,
    ));

    let agents_text = load_agents_instructions(cwd)?;
    if let Some(agent_notes) = agents_text.as_ref() {
        prompt.push_str("\n\n");
        prompt.push_str(agent_notes);
    }

    Ok((prompt, agents_text))
}

fn load_agents_instructions(cwd: &Path) -> Result<Option<String>> {
    let mut notes = Vec::new();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        let candidate = dir.join("AGENTS.md");
        if candidate.is_file() {
            let content = fs::read_to_string(&candidate)
                .with_context(|| format!("failed to read {}", candidate.display()))?;
            notes.push(content);
        }
        current = dir.parent();
    }
    if notes.is_empty() {
        Ok(None)
    } else {
        Ok(Some(notes.join("\n\n")))
    }
}

fn parse_submit_answer(arguments: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct SubmitArgs {
        answer: String,
    }
    let args: SubmitArgs = serde_json::from_str(arguments)?;
    Ok(args.answer)
}

fn execute_tool(tool_call: &ToolCall, cwd: &Path, max_output_chars: usize) -> Result<String> {
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

fn is_context_error(message: &str) -> bool {
    let msg = message.to_lowercase();
    msg.contains("context") && msg.contains("length")
}

fn prune_messages(messages: &[Value]) -> Vec<Value> {
    let mut system = Vec::new();
    let mut non_system = Vec::new();
    for msg in messages {
        if msg
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|r| r == "system")
        {
            system.push(msg.clone());
        } else {
            non_system.push(msg.clone());
        }
    }

    // Spec-aligned pruning:

    // - Keep all system messages
    // - Keep the initial user task message
    // - Preserve assistant<->tool call/response pairs
    // - Drop the oldest chunk of the remaining conversation (preferably at a user-boundary)
    let task_idx = non_system.iter().position(|m| {
        m.get("role")
            .and_then(Value::as_str)
            .is_some_and(|r| r == "user")
    });
    let Some(task_idx) = task_idx else {
        // No user message found; fall back to system + whatever we had.
        system.extend(non_system);
        return system;
    };

    let task_msg = non_system[task_idx].clone();
    let rest: Vec<Value> = non_system.into_iter().skip(task_idx + 1).collect();

    // Drop the oldest chunk of the remaining conversation. Since we don't have token
    // counts here, we approximate a "chunk" as the oldest third of messages and
    // prefer cutting at the next user message boundary to avoid slicing mid-turn.
    let drop_target = rest.len() / 3;
    let mut cut_idx = drop_target.min(rest.len());
    if let Some(boundary) = rest
        .iter()
        .enumerate()
        .skip(cut_idx)
        .find(|(_, m)| {
            m.get("role")
                .and_then(Value::as_str)
                .is_some_and(|r| r == "user")
        })
        .map(|(i, _)| i)
    {
        cut_idx = boundary;
    }

    let mut preserved = Vec::new();
    preserved.push(task_msg);
    preserved.extend(rest.into_iter().skip(cut_idx));

    let mut valid = Vec::new();
    let mut active_tool_ids: HashSet<String> = HashSet::new();

    for msg in preserved {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "assistant" => {
                active_tool_ids = extract_tool_call_ids(&msg);
                valid.push(msg);
            }
            "tool" => {
                if let Some(call_id) = msg.get("tool_call_id").and_then(Value::as_str) {
                    if active_tool_ids.contains(call_id) {
                        valid.push(msg);
                    }
                }
            }
            "user" => {
                active_tool_ids.clear();
                valid.push(msg);
            }
            _ => valid.push(msg),
        }
    }

    system.extend(valid);
    system
}

fn extract_tool_call_ids(msg: &Value) -> HashSet<String> {
    let mut ids = HashSet::new();
    if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
        for tc in tool_calls {
            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                ids.insert(id.to_string());
            }
        }
    }
    ids
}

fn tool_error(message: String) -> String {
    json!({
        "error": message,
    })
    .to_string()
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    workdir: Option<String>,
    timeout_ms: Option<u64>,
    max_output_chars: Option<usize>,
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
struct ReadFileArgs {
    file_path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

fn read_file(args: &ReadFileArgs, cwd: &Path) -> Result<String> {
    let offset = args.offset.unwrap_or(1);
    let limit = args
        .limit
        .unwrap_or(DEFAULT_READ_LIMIT)
        .min(DEFAULT_READ_LIMIT);
    if offset < 1 || limit < 1 {
        return Ok(tool_error("offset and limit must be >= 1".to_string()));
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
struct ListDirArgs {
    dir_path: String,
    offset: Option<usize>,
    limit: Option<usize>,
    depth: Option<usize>,
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
            "offset, limit, and depth must be >= 1".to_string(),
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
struct GrepFilesArgs {
    pattern: String,
    path: Option<String>,
    include: Option<String>,
    limit: Option<usize>,
}

fn grep_files(args: &GrepFilesArgs, cwd: &Path) -> Result<String> {
    let limit = args
        .limit
        .unwrap_or(DEFAULT_GREP_LIMIT)
        .min(DEFAULT_GREP_LIMIT);
    if limit < 1 {
        return Ok(tool_error("limit must be >= 1".to_string()));
    }

    let pattern = Regex::new(&args.pattern)
        .with_context(|| format!("invalid regex pattern: {}", args.pattern))?;

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
            if !set.is_match(entry.path()) {
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
struct ApplyPatchArgs {
    patch: String,
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

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn truncate(value: &str, limit: usize) -> (String, bool) {
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

fn build_tools(submit_enabled: bool) -> Vec<Value> {
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
                        "workdir": { "type": "string", "description": "Working directory for the command." },
                        "timeout_ms": { "type": "number", "description": "Timeout in milliseconds." },
                        "max_output_chars": { "type": "number", "description": "Maximum output characters to return." }
                    },
                    "required": ["command"],
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
                        "offset": { "type": "number", "description": "1-indexed start line." },
                        "limit": { "type": "number", "description": "Maximum number of lines to return." }
                    },
                    "required": ["file_path"],
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
                        "offset": { "type": "number", "description": "1-indexed start entry." },
                        "limit": { "type": "number", "description": "Maximum number of entries to return." },
                        "depth": { "type": "number", "description": "Maximum directory depth to traverse." }
                    },
                    "required": ["dir_path"],
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
                        "pattern": { "type": "string", "description": "Regex pattern to search for." },
                        "path": { "type": "string", "description": "Root path to search." },
                        "include": { "type": "string", "description": "Optional glob filter for files." },
                        "limit": { "type": "number", "description": "Maximum number of matches to return." }
                    },
                    "required": ["pattern"],
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

fn main() -> Result<()> {
    let raw_args: Vec<String> = env::args().collect();
    if raw_args.len() == 1 || (raw_args.len() == 2 && raw_args[1] == "help") {
        let mut cmd = Args::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    }
    let args = Args::parse();
    let cwd = fs::canonicalize(&args.cwd)
        .with_context(|| format!("failed to resolve cwd {}", args.cwd.display()))?;
    let api_key = args
        .api_key
        .clone()
        .or_else(|| env::var("OPENROUTER_API_KEY").ok());
    let api_key = match api_key {
        Some(key) => key,
        None => {
            bail!("missing API key: set --api-key or OPENROUTER_API_KEY");
        }
    };

    if api_key.is_empty() {
        bail!("missing API key: set --api-key or OPENROUTER_API_KEY");
    }

    let task = load_task(&args)?;
    let answer = run_prompt(&args, task, &cwd, &api_key)?;
    println!("{}", answer);
    Ok(())
}

fn run_prompt(args: &Args, prompt: String, cwd: &Path, api_key: &str) -> Result<String> {
    let log_dir = args.log_dir.clone().unwrap_or_else(|| cwd.to_path_buf());
    let log_path = log_dir.join("ra.jsonl");
    let logger = Logger::new(log_path)?;

    // UX default:
    // - `ra "PROMPT"` behaves like a normal CLI by default (no submit; exit on first assistant reply).
    // - `ra --prompt-file file.txt` runs in agent mode by default (submit-enabled; continues until submit).
    // Explicit overrides: --exec / --no-submit.
    if args.exec && args.no_submit {
        bail!("--exec and --no-submit cannot both be set");
    }
    let submit_enabled = if args.exec {
        true
    } else if args.no_submit {
        false
    } else {
        args.prompt_file.is_some()
    };

    let tools = build_tools(submit_enabled);
    let client = Client::new();
    let mut agent = Agent {
        client,
        base_url: args.base_url.clone(),
        model: args.model.clone(),
        api_key: api_key.to_string(),
        session_id: Uuid::new_v4().to_string(),
        tools,
        messages: Vec::new(),
        temperature: args.temperature,
        max_steps: args.max_steps,
        time_limit: args.time_limit_sec.map(Duration::from_secs),
        max_tool_output_chars: args
            .max_tool_output_chars
            .unwrap_or(DEFAULT_MAX_TOOL_OUTPUT_CHARS),
        cwd: cwd.to_path_buf(),
        submit_enabled,
        logger,
        token_usage_total: TokenUsage::default(),
        next_item_id: 0,
    };

    agent.run(prompt)
}
