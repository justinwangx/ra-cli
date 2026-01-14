use clap::{ArgGroup, Parser};
use std::path::PathBuf;

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
    name = "ra",
    version,
    about = "Ra is a baseline ReAct CLI agent for OpenRouter-compatible models.",
    long_about = "Ra is a baseline ReAct CLI agent for OpenRouter-compatible models."
)]
pub(crate) struct Args {
    #[arg(
        long,
        default_value = "openai/gpt-4.1-mini",
        env = "RA_DEFAULT_MODEL",
        help = "Model ID to use (OpenRouter format)."
    )]
    pub(crate) model: String,

    #[arg(long, value_name = "FILE", help = "Read the prompt from a file.")]
    pub(crate) prompt_file: Option<PathBuf>,

    #[arg(
        long,
        value_name = "DIR",
        default_value = ".",
        help = "Working directory for file and shell tools."
    )]
    pub(crate) cwd: PathBuf,

    #[arg(long, help = "OpenRouter API key (overrides OPENROUTER_API_KEY).")]
    pub(crate) api_key: Option<String>,

    #[arg(
        long,
        default_value = "https://openrouter.ai/api/v1",
        help = "OpenRouter API base URL."
    )]
    pub(crate) base_url: String,

    #[arg(long, help = "Sampling temperature (omit to use provider default).")]
    pub(crate) temperature: Option<f64>,

    #[arg(long, help = "Maximum number of tool steps before terminating.")]
    pub(crate) max_steps: Option<usize>,

    #[arg(long, help = "Time limit in seconds before terminating.")]
    pub(crate) time_limit_sec: Option<u64>,

    #[arg(long, help = "Directory to write the JSONL log file.")]
    pub(crate) log_dir: Option<PathBuf>,

    #[arg(
        long,
        value_name = "FILE",
        help = "Path to write the JSONL log file (overrides --log-dir)."
    )]
    pub(crate) log_path: Option<PathBuf>,

    #[arg(
        long,
        default_value_t = false,
        help = "Print the JSONL event stream to stdout after completion (suppresses plain final answer output)."
    )]
    pub(crate) json: bool,

    #[arg(
        long,
        default_value_t = false,
        help = "Stream the JSONL event stream to stdout as events occur (suppresses plain final answer output)."
    )]
    pub(crate) stream_json: bool,

    #[arg(long, help = "Maximum tool output characters to retain.")]
    pub(crate) max_tool_output_chars: Option<usize>,

    #[arg(
        long,
        default_value_t = false,
        help = "Force agent/exec mode (enable submit tool and continue until submit is called)."
    )]
    pub(crate) exec: bool,

    #[arg(
        long,
        default_value_t = false,
        help = "Force disabling the submit tool and stop on the first assistant response without tool calls."
    )]
    pub(crate) no_submit: bool,

    #[arg(
        long,
        env = "RA_RETRY_429",
        default_value_t = false,
        help = "Retry HTTP 429 responses (rate limited). By default, 429s are only retried when Retry-After is present."
    )]
    pub(crate) retry_429: bool,

    #[arg(
        long = "enable-search",
        alias = "search",
        env = "RA_WEB_SEARCH",
        default_value_t = false,
        help = "Enable web tools (off by default): web_search (Tavily), web_open, web_find."
    )]
    pub(crate) web_search: bool,

    #[arg(value_name = "PROMPT", help = "Prompt text (quote for spaces).")]
    pub(crate) prompt: Option<String>,
}
