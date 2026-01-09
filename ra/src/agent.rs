use crate::constants::DEFAULT_CONTINUE_MESSAGE;
use crate::logger::Logger;
use crate::prompt::build_system_prompt;
use crate::protocol::{ApiErrorResponse, CompletionResult, TokenUsage, Usage};
use crate::tools::{execute_tool, parse_patch_changes, tool_error, truncate};
use anyhow::{anyhow, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

type ToolLogging = (Option<(String, String)>, Vec<Value>);

pub(crate) struct Agent {
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        client: Client,
        base_url: String,
        model: String,
        api_key: String,
        session_id: String,
        tools: Vec<Value>,
        temperature: Option<f64>,
        max_steps: Option<usize>,
        time_limit: Option<Duration>,
        max_tool_output_chars: usize,
        cwd: PathBuf,
        submit_enabled: bool,
        logger: Logger,
    ) -> Self {
        Self {
            client,
            base_url,
            model,
            api_key,
            session_id,
            tools,
            messages: Vec::new(),
            temperature,
            max_steps,
            time_limit,
            max_tool_output_chars,
            cwd,
            submit_enabled,
            logger,
            token_usage_total: TokenUsage::default(),
            next_item_id: 0,
        }
    }

    pub(crate) fn run(&mut self, task: String) -> Result<String> {
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
        self.messages.push(json!({"role": "user", "content": task}));

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
                    // tool-call/response pairs) and retry. We may need to prune multiple times.
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
                self.messages.push(json!({
                    "role": "user",
                    "content": DEFAULT_CONTINUE_MESSAGE,
                }));
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
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(request)
            .send()
            .with_context(|| format!("OpenRouter request failed: POST {}", url))?;

        let status = response.status();
        let headers = response.headers().clone();
        let body = response.text().with_context(|| {
            format!("failed to read OpenRouter response body (HTTP {})", status)
        })?;

        if !status.is_success() {
            return Err(anyhow!(format_openrouter_http_error(
                &url,
                status.as_u16(),
                &headers,
                &body
            )));
        }

        let parsed: crate::protocol::ChatCompletionResponse = serde_json::from_str(&body)
            .with_context(|| {
                let (snippet, _) = truncate(&body, 2000);
                format!(
                    "OpenRouter returned an unexpected response body (HTTP {}):\n{}",
                    status, snippet
                )
            })?;
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
        self.logger.log_event(&json!({
            "type": "thread.started",
            "thread_id": self.session_id.clone(),
        }))
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
        self.logger.log_event(&json!({
            "type": "turn.completed",
            "usage": usage,
        }))
    }

    fn log_turn_failed(&mut self, message: &str) -> Result<()> {
        self.logger.log_event(&json!({
            "type": "turn.failed",
            "error": { "message": message }
        }))
    }

    fn log_error_event(&mut self, message: &str) -> Result<()> {
        self.logger.log_event(&json!({
            "type": "error",
            "message": message,
        }))
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
        self.log_item_started(json!({
            "id": item_id,
            "type": "command_execution",
            "command": command,
            "aggregated_output": "",
            "exit_code": null,
            "status": "in_progress",
        }))
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
        self.log_item_completed(json!({
            "id": item_id,
            "type": "command_execution",
            "command": command,
            "aggregated_output": aggregated_output,
            "exit_code": exit_code,
            "status": status,
        }))
    }

    fn prepare_tool_logging(&mut self, tool_name: &str, arguments: &str) -> Result<ToolLogging> {
        if tool_name == "apply_patch" {
            let patch = serde_json::from_str::<crate::tools::ApplyPatchArgs>(arguments)
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
            let id = self.next_item_id();
            return self.log_item_completed(json!({
                "id": id,
                "type": "file_change",
                "changes": file_changes,
                "status": if status { "completed" } else { "failed" },
            }));
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
        self.logger.log_event(&json!({
            "type": "item.started",
            "item": item,
        }))
    }

    fn log_item_completed(&mut self, item: Value) -> Result<()> {
        self.logger.log_event(&json!({
            "type": "item.completed",
            "item": item,
        }))
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
        if let Ok(args) = serde_json::from_str::<crate::tools::ShellArgs>(arguments) {
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

fn parse_submit_answer(arguments: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct SubmitArgs {
        answer: String,
    }
    let args: SubmitArgs = serde_json::from_str(arguments)?;
    Ok(args.answer)
}

fn is_context_error(message: &str) -> bool {
    let msg = message.to_lowercase();
    msg.contains("context") && msg.contains("length")
}

fn format_openrouter_http_error(url: &str, status: u16, headers: &HeaderMap, body: &str) -> String {
    let request_id = headers
        .get("x-request-id")
        .or_else(|| headers.get("x-openrouter-request-id"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let mut api_message: Option<String> = None;
    if let Ok(err) = serde_json::from_str::<ApiErrorResponse>(body) {
        api_message = Some(err.error.message);
    } else if let Ok(v) = serde_json::from_str::<Value>(body) {
        // Best-effort: handle other providers that return {"error":"..."} or similar.
        if let Some(s) = v.get("error").and_then(|e| e.as_str()) {
            api_message = Some(s.to_string());
        }
    }

    let hint = match status {
        401 | 403 => "Hint: check your API key (set `OPENROUTER_API_KEY` or use `--api-key`) and that it has access to the model.",
        404 => "Hint: check `--base-url` and the model name (`--model`).",
        408 | 504 => "Hint: the request timed out; try again or use a faster model.",
        429 => "Hint: you may be rate limited; retry later or lower concurrency.",
        500 | 502 | 503 => "Hint: upstream/server error; retry later.",
        _ => "",
    };

    let (snippet, _) = truncate(body, 2000);
    let mut msg = String::new();
    msg.push_str(&format!(
        "OpenRouter API error (HTTP {}) when calling {}",
        status, url
    ));
    if !request_id.is_empty() {
        msg.push_str(&format!(" (request_id: {})", request_id));
    }
    if let Some(m) = api_message {
        if !m.trim().is_empty() {
            msg.push_str(&format!("\nMessage: {}", m.trim()));
        }
    }
    if !snippet.trim().is_empty() {
        msg.push_str("\nBody:");
        msg.push('\n');
        msg.push_str(snippet.trim());
    }
    if !hint.is_empty() {
        msg.push('\n');
        msg.push_str(hint);
    }
    msg
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
    //
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
        system.extend(non_system);
        return system;
    };

    let task_msg = non_system[task_idx].clone();
    let rest: Vec<Value> = non_system.into_iter().skip(task_idx + 1).collect();

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
