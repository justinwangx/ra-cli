use crate::agent::Agent;
use crate::cli::Args;
use crate::constants::DEFAULT_MAX_TOOL_OUTPUT_CHARS;
use crate::logger::Logger;
use crate::prompt::load_task;
use crate::tools::{build_tools, resolve_path};
use anyhow::{bail, Result};
use reqwest::blocking::Client;
use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

pub(crate) fn run_prompt(args: &Args, cwd: &Path, api_key: &str) -> Result<String> {
    let session_id = Uuid::new_v4().to_string();
    let logger = {
        let log_path = if let Some(path) = &args.log_path {
            resolve_path(cwd, path)
        } else {
            let log_dir = args
                .log_dir
                .as_deref()
                .map(|p| resolve_path(cwd, p))
                .unwrap_or_else(|| cwd.to_path_buf());
            // Avoid log overwrites by default by making the log filename unique per run.
            let now = OffsetDateTime::now_utc();
            let rfc3339 = now
                .format(&Rfc3339)
                .unwrap_or_else(|_| "unknown-time".to_string());
            let safe_ts = rfc3339.replace(':', "-");
            let filename = format!("ra-{}-{}.jsonl", safe_ts, session_id);
            log_dir.join(filename)
        };
        Logger::new(Some(log_path), args.stream_json, args.json)?
    };
    let logger_for_output = logger.clone();

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

    if args.web_search {
        let has_tavily_key = env::var("RA_TAVILY_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
            .is_some()
            || env::var("TAVILY_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty())
                .is_some();
        if !has_tavily_key {
            bail!(
                "--search is enabled but no Tavily API key was found. Set TAVILY_API_KEY (or RA_TAVILY_API_KEY)."
            );
        }
    }

    let tools = build_tools(submit_enabled, args.web_search);
    // Sane defaults:
    // - explicit connect timeout so we fail fast on network issues
    // - generous overall request timeout so slow generations don't hang forever
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(10 * 60))
        .build()?;
    let mut agent = Agent::new(
        client,
        args.base_url.clone(),
        args.model.clone(),
        api_key.to_string(),
        session_id,
        tools,
        args.temperature,
        args.max_steps,
        args.time_limit_sec.map(Duration::from_secs),
        args.max_tool_output_chars
            .unwrap_or(DEFAULT_MAX_TOOL_OUTPUT_CHARS),
        cwd.to_path_buf(),
        submit_enabled,
        args.web_search,
        args.retry_429,
        logger,
    );

    let prompt = load_task(args)?;
    match agent.run(prompt) {
        Ok(answer) => {
            if args.json {
                logger_for_output.emit_buffer_to_stdout()?;
            }
            Ok(answer)
        }
        Err(err) => {
            if args.json {
                // Best-effort: emit any buffered events even on error.
                let _ = logger_for_output.emit_buffer_to_stdout();
            }
            Err(err)
        }
    }
}

pub(crate) fn resolve_and_validate_cwd(args: &Args) -> Result<std::path::PathBuf> {
    Ok(fs::canonicalize(&args.cwd)?)
}

#[cfg(test)]
mod tests {
    use super::run_prompt;
    use crate::cli::Args;
    use serde_json::Value;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;
    use uuid::Uuid;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn assert_obj_has<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> &'a Value {
        obj.get(key)
            .unwrap_or_else(|| panic!("missing key `{}`", key))
    }

    #[test]
    fn jsonl_shape_check_single_shot() {
        // Minimal local stub server for /chat/completions
        // Some sandboxes disallow even loopback binds; in that case we skip this test.
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(err) => {
                eprintln!(
                    "skipping jsonl_shape_check_single_shot: bind failed: {}",
                    err
                );
                return;
            }
        };
        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{}", addr);

        let server_thread = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let body = r#"{"choices":[{"message":{"role":"assistant","content":"ok","tool_calls":null}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).expect("write");
            let _ = stream.flush();
        });

        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(cwd.is_dir(), "CARGO_MANIFEST_DIR should exist");

        let log_path = cwd
            .join("target")
            .join(format!("jsonl-shape-test-{}.jsonl", Uuid::new_v4()));
        let _ = fs::remove_file(&log_path);
        fs::create_dir_all(log_path.parent().unwrap()).expect("create log dir");

        let args = Args {
            model: "openai/gpt-4.1-mini".to_string(),
            prompt_file: None,
            cwd: cwd.clone(),
            api_key: Some("test-key".to_string()),
            base_url,
            temperature: None,
            max_steps: Some(1),
            time_limit_sec: None,
            log_dir: None,
            log_path: Some(log_path.clone()),
            json: false,
            stream_json: false,
            max_tool_output_chars: None,
            exec: false,
            no_submit: true,
            retry_429: false,
            web_search: false,
            prompt: Some("hi".to_string()),
        };

        let answer = run_prompt(&args, &cwd, "test-key").expect("run_prompt");
        assert_eq!(answer, "ok");

        server_thread.join().expect("server join");

        let contents = fs::read_to_string(&log_path).expect("read log");
        let lines: Vec<&str> = contents.lines().collect();
        assert!(
            !lines.is_empty(),
            "expected JSONL log to contain at least one event"
        );

        for (i, line) in lines.iter().enumerate() {
            let v: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("line {} invalid JSON: {}", i, e));
            let Value::Object(obj) = v else {
                panic!("line {} not a JSON object", i);
            };
            assert_obj_has(&obj, "type");
            assert_obj_has(&obj, "timestamp");
            assert_obj_has(&obj, "timestamp_ms");
        }

        // Spot-check specific event payloads we rely on.
        let thread_started: Value = serde_json::from_str(lines[0]).unwrap();
        let Value::Object(obj) = thread_started else {
            panic!("thread.started not object");
        };
        assert_eq!(
            obj.get("type").and_then(Value::as_str),
            Some("thread.started")
        );
        assert_obj_has(&obj, "thread_id");

        let turn_started: Value = serde_json::from_str(lines[1]).unwrap();
        let Value::Object(obj) = turn_started else {
            panic!("turn.started not object");
        };
        assert_eq!(
            obj.get("type").and_then(Value::as_str),
            Some("turn.started")
        );
        assert_obj_has(&obj, "prompt");
        assert_obj_has(&obj, "system_prompt");

        let turn_completed: Value = serde_json::from_str(lines[lines.len() - 1]).unwrap();
        let Value::Object(obj) = turn_completed else {
            panic!("turn.completed not object");
        };
        assert_eq!(
            obj.get("type").and_then(Value::as_str),
            Some("turn.completed")
        );
        let usage = obj.get("usage").expect("usage present");
        let Value::Object(u) = usage else {
            panic!("usage not object");
        };
        assert_obj_has(u, "input_tokens");
        assert_obj_has(u, "cached_input_tokens");
        assert_obj_has(u, "output_tokens");

        let _ = fs::remove_file(&log_path);
    }

    #[test]
    fn retries_on_transient_503() {
        // Local stub server for /chat/completions that returns 503 once, then 200 OK.
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(err) => {
                eprintln!("skipping retries_on_transient_503: bind failed: {}", err);
                return;
            }
        };
        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{}", addr);

        let server_thread = thread::spawn(move || {
            for i in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }

                if i == 0 {
                    let body = r#"{"error":{"message":"temporary upstream issue"}}"#;
                    let response = format!(
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(response.as_bytes()).expect("write 503");
                    let _ = stream.flush();
                } else {
                    let body = r#"{"choices":[{"message":{"role":"assistant","content":"ok","tool_calls":null}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(response.as_bytes()).expect("write 200");
                    let _ = stream.flush();
                }
            }
        });

        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(cwd.is_dir(), "CARGO_MANIFEST_DIR should exist");

        let log_path = cwd
            .join("target")
            .join(format!("retry-test-{}.jsonl", Uuid::new_v4()));
        let _ = fs::remove_file(&log_path);
        fs::create_dir_all(log_path.parent().unwrap()).expect("create log dir");

        let args = Args {
            model: "openai/gpt-4.1-mini".to_string(),
            prompt_file: None,
            cwd: cwd.clone(),
            api_key: Some("test-key".to_string()),
            base_url,
            temperature: None,
            max_steps: Some(1),
            time_limit_sec: None,
            log_dir: None,
            log_path: Some(log_path.clone()),
            json: false,
            stream_json: false,
            max_tool_output_chars: None,
            exec: false,
            no_submit: true,
            retry_429: false,
            web_search: false,
            prompt: Some("hi".to_string()),
        };

        let answer = run_prompt(&args, &cwd, "test-key").expect("run_prompt");
        assert_eq!(answer, "ok");

        server_thread.join().expect("server join");
    }

    #[test]
    fn retries_on_429_when_enabled() {
        // Local stub server for /chat/completions that returns 429 once (without Retry-After),
        // then 200 OK. We only retry the 429 when args.retry_429 is set.
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(err) => {
                eprintln!("skipping retries_on_429_when_enabled: bind failed: {}", err);
                return;
            }
        };
        let addr = listener.local_addr().expect("local_addr");
        let base_url = format!("http://{}", addr);

        let server_thread = thread::spawn(move || {
            for i in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }

                if i == 0 {
                    let body = r#"{"error":{"message":"rate limited"}}"#;
                    let response = format!(
                        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(response.as_bytes()).expect("write 429");
                    let _ = stream.flush();
                } else {
                    let body = r#"{"choices":[{"message":{"role":"assistant","content":"ok","tool_calls":null}}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(response.as_bytes()).expect("write 200");
                    let _ = stream.flush();
                }
            }
        });

        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        assert!(cwd.is_dir(), "CARGO_MANIFEST_DIR should exist");

        let log_path = cwd
            .join("target")
            .join(format!("retry-429-test-{}.jsonl", Uuid::new_v4()));
        let _ = fs::remove_file(&log_path);
        fs::create_dir_all(log_path.parent().unwrap()).expect("create log dir");

        let args = Args {
            model: "openai/gpt-4.1-mini".to_string(),
            prompt_file: None,
            cwd: cwd.clone(),
            api_key: Some("test-key".to_string()),
            base_url,
            temperature: None,
            max_steps: Some(1),
            time_limit_sec: None,
            log_dir: None,
            log_path: Some(log_path.clone()),
            json: false,
            stream_json: false,
            max_tool_output_chars: None,
            exec: false,
            no_submit: true,
            retry_429: true,
            web_search: false,
            prompt: Some("hi".to_string()),
        };

        let answer = run_prompt(&args, &cwd, "test-key").expect("run_prompt");
        assert_eq!(answer, "ok");

        server_thread.join().expect("server join");
    }

    #[test]
    fn browser_suite_example_flow_works_with_stubs() {
        // This test simulates the README flow:
        // web_search -> web_open -> web_find -> submit
        // using local stub servers (no external network).

        // Avoid racing env var modifications with other tests.
        let _guard = ENV_LOCK.lock().expect("env lock");

        // Start a local content server that serves a tiny HTML page.
        let page_listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(err) => {
                eprintln!(
                    "skipping browser_suite_example_flow_works_with_stubs: bind failed: {err}"
                );
                return;
            }
        };
        let page_addr = page_listener.local_addr().expect("local_addr");
        let page_url = format!("http://{}/page", page_addr);
        let page_thread = thread::spawn({
            let page_url = page_url.clone();
            move || {
                for _ in 0..2 {
                    let (mut stream, _) = page_listener.accept().expect("accept page");
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        match stream.read(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let body = format!(
                        "<html><body><h1>Release notes</h1><p>Many items were stabilized in this release.</p><p>More stabilized features here.</p><p>URL: {page_url}</p></body></html>"
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(response.as_bytes()).expect("write page");
                    let _ = stream.flush();
                }
            }
        });

        // Start a local Tavily stub server.
        let tavily_listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(err) => {
                eprintln!(
                    "skipping browser_suite_example_flow_works_with_stubs: bind failed: {err}"
                );
                return;
            }
        };
        let tavily_addr = tavily_listener.local_addr().expect("local_addr");
        let tavily_base = format!("http://{}", tavily_addr);
        let tavily_thread = thread::spawn({
            let page_url = page_url.clone();
            move || {
                let (mut stream, _) = tavily_listener.accept().expect("accept tavily");
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }

                let body = serde_json::json!({
                    "results": [
                        {"title": "Rust release notes", "url": page_url, "content": "Many items were stabilized...", "score": 0.9}
                    ]
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).expect("write tavily");
                let _ = stream.flush();
            }
        });

        // Start a local /chat/completions stub server that issues tool calls in sequence.
        let llm_listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(l) => l,
            Err(err) => {
                eprintln!(
                    "skipping browser_suite_example_flow_works_with_stubs: bind failed: {err}"
                );
                return;
            }
        };
        let llm_addr = llm_listener.local_addr().expect("local_addr");
        let base_url = format!("http://{}", llm_addr);
        let llm_thread = thread::spawn({
            let page_url = page_url.clone();
            move || {
                let responses = vec![
                    // 1) web_search
                    serde_json::json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "call-1",
                                    "type": "function",
                                    "function": {"name": "web_search", "arguments": "{\"query\":\"Rust 1.75 release notes\",\"max_results\":5}"}
                                }]
                            }
                        }]
                    }),
                    // 2) web_open
                    serde_json::json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "call-2",
                                    "type": "function",
                                    "function": {"name": "web_open", "arguments": format!("{{\"url\":\"{}\",\"offset\":1,\"limit\":200}}", page_url)}
                                }]
                            }
                        }]
                    }),
                    // 3) web_find
                    serde_json::json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "call-3",
                                    "type": "function",
                                    "function": {"name": "web_find", "arguments": format!("{{\"url\":\"{}\",\"pattern\":\"stabilized\",\"max_results\":10,\"context_lines\":1}}", page_url)}
                                }]
                            }
                        }]
                    }),
                    // 4) submit
                    serde_json::json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "call-4",
                                    "type": "function",
                                    "function": {"name": "submit", "arguments": "{\"answer\":\"ok\"}"}
                                }]
                            }
                        }]
                    }),
                ];

                for body in responses {
                    let (mut stream, _) = llm_listener.accept().expect("accept llm");
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        match stream.read(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }

                    let body = body.to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(response.as_bytes()).expect("write llm");
                    let _ = stream.flush();
                }
            }
        });

        // Configure web_search to hit our Tavily stub.
        std::env::set_var("RA_TAVILY_BASE_URL", &tavily_base);
        std::env::set_var("TAVILY_API_KEY", "test-tavily-key");

        let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let log_path = cwd
            .join("target")
            .join(format!("browser-suite-test-{}.jsonl", Uuid::new_v4()));
        let _ = fs::remove_file(&log_path);
        fs::create_dir_all(log_path.parent().unwrap()).expect("create log dir");

        let args = Args {
            model: "openai/gpt-4.1-mini".to_string(),
            prompt_file: None,
            cwd: cwd.clone(),
            api_key: Some("test-key".to_string()),
            base_url,
            temperature: None,
            max_steps: Some(10),
            time_limit_sec: None,
            log_dir: None,
            log_path: Some(log_path.clone()),
            json: false,
            stream_json: false,
            max_tool_output_chars: None,
            exec: true,
            no_submit: false,
            retry_429: false,
            web_search: true,
            prompt: Some(
                "Search for 'Rust 1.75 release notes', open the official blog link, then find 'stabilized' and cite the line ranges."
                    .to_string(),
            ),
        };

        let answer = run_prompt(&args, &cwd, "test-key").expect("run_prompt");
        assert_eq!(answer, "ok");

        llm_thread.join().expect("llm join");
        tavily_thread.join().expect("tavily join");
        page_thread.join().expect("page join");
        let _ = fs::remove_file(&log_path);
    }
}
