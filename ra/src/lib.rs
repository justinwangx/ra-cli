use anyhow::{anyhow, bail, Result};
use clap::CommandFactory;
use clap::Parser;
use std::env;

mod agent;
mod cli;
mod constants;
mod logger;
mod prompt;
mod protocol;
mod run;
mod tools;

pub fn run_cli() -> Result<()> {
    let raw_args: Vec<String> = env::args().collect();
    if raw_args.len() == 1 || (raw_args.len() == 2 && raw_args[1] == "help") {
        let mut cmd = crate::cli::Args::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    }

    let args = crate::cli::Args::parse();
    if args.json && args.stream_json {
        bail!("--json and --stream-json cannot both be set");
    }
    let cwd = crate::run::resolve_and_validate_cwd(&args)
        .map_err(|e| anyhow!("failed to resolve cwd {}: {}", args.cwd.display(), e))?;

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

    let answer = crate::run::run_prompt(&args, &cwd, &api_key)?;
    if args.stream_json {
        // In streaming JSON mode, stdout is reserved for JSONL events.
        return Ok(());
    }
    if args.json {
        // In buffered JSON mode, we print the JSONL stream at the end (and suppress plain output).
        // The logger is already buffering and will flush to stdout on successful completion.
        // Note: run_prompt writes events to the buffer; emitting happens there on success/failure.
        return Ok(());
    }
    println!("{answer}");
    Ok(())
}
