# Ra

Baseline ReAct agent CLI for OpenRouter-compatible models. View the [specification](SPEC.md).

> ReAct is the most common architecture used in agent frameworks and is the baseline against which you should measure more complex agents (it can be surprisingly difficult to hand-tune agents that perform better than a ReAct agent against a diverse set of tasks!).
>
> &mdash; [UK AISI](https://github.com/UKGovernmentBEIS/inspect_ai/blob/649dbfe0a8bb670c7ef88a52839b184cca823822/docs/react-agent.qmd#L7)

## Quick Start

Install:

```sh
# Run one of the following commands:
npm i -g react-agent-cli
cargo install ra-cli
curl -fsSL https://raw.githubusercontent.com/justinwangx/ra-cli/main/install.sh | sh
```

The script installs `ra` into `/usr/local/bin` (if writable) or `~/.local/bin`.
Set `RA_VERSION` to pin a tag.

Set your OpenRouter API key:

```sh
export OPENROUTER_API_KEY="..."
```

Run a quick one-liner task (defaults to **no-submit mode**; exits on the first assistant response):

```sh
ra "Summarize the repo layout."
```

## Usage

- **Modes**:
  - **Single-shot (default)**: `ra "PROMPT"` exits after the first assistant response
  - **Exec/agent**: `ra --exec ...` (or `ra --prompt-file FILE`) continues until the model calls `submit`

> [!WARNING] > `ra` is designed for **agentic evaluations that run in sandboxed environments**, as a baseline against more advanced CLI agents like Codex, Claude Code, and Gemini CLI. It can execute arbitrary shell commands and read/write files via tool calls. If you run it on your machine outside a sandbox, do so **at your own risk** and only in a workspace you’re comfortable exposing to the model.

### Examples

```sh
# Single-shot (default)
ra "Say hi back"

# Configure the model (default: openai/gpt-4.1-mini)
ra --model openai/gpt-4.1 "Say hi back"

# Exec/agent mode for multi-step tasks
ra --exec "Summarize the repo layout and point out anything surprising."

# Run a longer task from a file (defaults to exec/agent mode)
ra --prompt-file /path/to/prompt.txt

# Use a local OpenAI-compatible server (e.g. Ollama: http://localhost:11434/v1)
ra --base-url "http://localhost:11434/v1" --api-key "local" --model "openai/gpt-4.1-mini" --exec "Explain what this repo does."

# Set your default model globally
RA_DEFAULT_MODEL="openai/gpt-4.1-mini" ra "Say hi back"

# Write logs somewhere specific
ra --log-dir /tmp/ra-logs --exec "List files."

# Emit JSONL log stream to stdout at the end
ra --json --exec "List files."

# Stream JSONL log events to stdout as they happen
ra --stream-json --exec "List files."

# Enable web browsing tools (off by default): web_search (Tavily), web_open, web_find.
export TAVILY_API_KEY="..."
ra --enable-search --exec --max-steps 25 "Find the latest release notes for Rust 1.75 and summarize them."

# Example of using open/find after a search:
ra --enable-search --exec "Search for 'Rust 1.75 release notes', open the official blog link, then find 'stabilized' and cite the line ranges."
```

Logs are written to a unique `ra-<timestamp>-<session_id>.jsonl` file in `--log-dir` (default: `--cwd`), or to `--log-path` if set. Format is a Codex
`exec --json`-style JSONL stream with `thread.started`, `turn.started`, `item.*`, and `turn.completed`.

## Install from source

```sh
cargo install --path ra
```

## Build

Install targets:

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
  x86_64-apple-darwin aarch64-apple-darwin
```

Linux:

```sh
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

macOS:

```sh
cargo build --release --target x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
```

Optional universal macOS binary:

```sh
lipo -create -output ra-macos-universal \
  target/x86_64-apple-darwin/release/ra \
  target/aarch64-apple-darwin/release/ra
```

## Cite

If you find `ra` helpful in your research or work, feel free to cite:

```BibTeX
@misc{wang2026ra,
  title = {Ra: Baseline ReAct Agent},
  author = {Justin Wang},
  year = {2026},
  howpublished = {\url{https://github.com/justinwangx/ra-cli}},
}
```
