use crate::constants::DEFAULT_CONTINUE_MESSAGE;
use crate::logger::Logger;
use crate::prompt::build_system_prompt;
use crate::protocol::{ApiErrorResponse, CompletionResult, TokenUsage, Usage};
use crate::tools::{execute_tool, parse_patch_changes, tool_error, truncate};
use anyhow::{anyhow, Context, Result};
use reqwest::blocking::Client;
use reqwest::header::HeaderMap;
use reqwest::header::RETRY_AFTER;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::error::Error as StdError;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    retry_429: bool,
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
        retry_429: bool,
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
            retry_429,
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
        // Completions are safe to retry. Small bounded retries make us resilient against transient
        // stalls/timeouts while reading the response body.
        const MAX_RETRIES: usize = 2;
        let mut last_http_err: Option<anyhow::Error> = None;

        for attempt in 0..=MAX_RETRIES {
            let response = match self
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .json(request)
                .send()
            {
                Ok(r) => r,
                Err(err) => {
                    if attempt < MAX_RETRIES && should_retry_reqwest_error(&err) {
                        sleep_backoff(attempt, None);
                        continue;
                    }
                    return Err(anyhow!(err)).with_context(|| {
                        format!(
                            "OpenRouter request failed: POST {} (attempt {}/{})",
                            url,
                            attempt + 1,
                            MAX_RETRIES + 1
                        )
                    });
                }
            };

            let status = response.status();
            let headers = response.headers().clone();

            // Read bytes first so we can retry on body-read timeouts, and decode lossily for
            // error messages (JSON should be UTF-8, but we don't want to fail formatting).
            let body_bytes = match response.bytes() {
                Ok(b) => b,
                Err(err) => {
                    if attempt < MAX_RETRIES && should_retry_reqwest_error(&err) {
                        sleep_backoff(attempt, None);
                        continue;
                    }
                    return Err(anyhow!(err)).with_context(|| {
                        format!(
                            "failed to read OpenRouter response body (HTTP {}) (attempt {}/{})",
                            status,
                            attempt + 1,
                            MAX_RETRIES + 1
                        )
                    });
                }
            };
            let body = String::from_utf8_lossy(&body_bytes).to_string();

            if !status.is_success() {
                // Align with Codex defaults: do not blindly retry 429s unless the server
                // provides an explicit Retry-After. This avoids retry-storming under hard limits.
                let retry_after = headers.get(RETRY_AFTER).and_then(parse_retry_after_secs);
                let retry_allowed = if status.as_u16() == 429 {
                    self.retry_429 || retry_after.is_some()
                } else {
                    true
                };

                if attempt < MAX_RETRIES && retry_allowed && should_retry_status(status) {
                    sleep_backoff(attempt, retry_after);
                    last_http_err = Some(anyhow!(format_openrouter_http_error(
                        &url,
                        status.as_u16(),
                        &headers,
                        &body
                    )));
                    continue;
                }
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
            return Ok(CompletionResult {
                message: choice.message,
                usage: parsed.usage,
            });
        }

        // Defensive: we should have returned above. If we didn't, return the most recent HTTP
        // error (e.g. repeated 503/429), or a generic error otherwise.
        Err(last_http_err.unwrap_or_else(|| anyhow!("OpenRouter request failed after retries")))
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

fn should_retry_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
}

fn should_retry_reqwest_error(err: &reqwest::Error) -> bool {
    // reqwest classifies some truncated/chunked-body failures as decode errors (e.g. gzip, chunked
    // framing), not as body errors. Those are safe to retry for our POST /chat/completions calls.
    err.is_timeout()
        || err.is_connect()
        || err.is_body()
        || err.is_decode()
        || error_chain_has_retryable_io_dyn(err)
}

fn error_chain_has_retryable_io_dyn(err: &(dyn StdError + 'static)) -> bool {
    // Treat common transport truncation as transient:
    // - unexpected EOF during chunked framing ("unexpected EOF during chunk size line")
    // - broken pipe / connection reset while reading
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind;
            if matches!(
                io.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
            ) {
                return true;
            }
        }
        cur = e.source();
    }
    false
}

fn parse_retry_after_secs(v: &reqwest::header::HeaderValue) -> Option<u64> {
    // Retry-After can be delta-seconds or an HTTP-date; we only parse delta-seconds.
    let s = v.to_str().ok()?.trim();
    s.parse::<u64>().ok()
}

fn sleep_backoff(attempt: usize, retry_after_secs: Option<u64>) {
    // Exponential backoff with small jitter, capped.
    // attempt=0 -> ~250ms, attempt=1 -> ~500ms, attempt=2 -> ~1000ms
    let base_ms = 250u64.saturating_mul(1u64 << attempt.min(10));
    let capped_ms = base_ms.min(3_000);
    let jitter_ms = (now_millis() % 50) as u64; // 0..49ms
    let delay_ms = retry_after_secs
        .map(|s| s.saturating_mul(1000))
        .unwrap_or(capped_ms)
        .saturating_add(jitter_ms);
    thread::sleep(Duration::from_millis(delay_ms));
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_millis(0))
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::error_chain_has_retryable_io_dyn;
    use std::error::Error as StdError;
    use std::fmt;

    #[test]
    fn treats_unexpected_eof_as_retryable() {
        #[derive(Debug)]
        struct Wrapper(std::io::Error);
        impl fmt::Display for Wrapper {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "wrapper")
            }
        }
        impl StdError for Wrapper {
            fn source(&self) -> Option<&(dyn StdError + 'static)> {
                Some(&self.0)
            }
        }

        let io = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof");
        let w = Wrapper(io);
        assert!(error_chain_has_retryable_io_dyn(&w));
    }

    #[test]
    fn does_not_mark_other_io_as_retryable() {
        #[derive(Debug)]
        struct Wrapper(std::io::Error);
        impl fmt::Display for Wrapper {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "wrapper")
            }
        }
        impl StdError for Wrapper {
            fn source(&self) -> Option<&(dyn StdError + 'static)> {
                Some(&self.0)
            }
        }

        let io = std::io::Error::new(std::io::ErrorKind::Other, "other");
        let w = Wrapper(io);
        assert!(!error_chain_has_retryable_io_dyn(&w));
    }
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
