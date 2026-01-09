use crate::cli::Args;
use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

pub(crate) fn load_task(args: &Args) -> Result<String> {
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

pub(crate) fn build_system_prompt(
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

    prompt.push_str(
        "\n\nTools:\n\
- shell_command(command, workdir?, timeout_ms?, max_output_chars?)\n\
- read_file(file_path, offset?, limit?)\n\
- list_dir(dir_path, offset?, limit?, depth?)\n\
- grep_files(pattern, path?, include?, limit?)\n\
- apply_patch(patch)\n",
    );
    if submit_enabled {
        prompt.push_str("- submit(answer)\n");
    }

    prompt.push_str(
        "\nTool usage notes:\n\
- Pagination is 1-indexed: read_file.offset and list_dir.offset start at 1 (not 0). limit/depth must be >= 1.\n\
- grep_files.pattern is a Rust regex. Escape metacharacters if you want a literal match (e.g. use \"main\\(\" to search for \"main(\").\n\
- If you need to edit files, prefer apply_patch.\n",
    );

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
