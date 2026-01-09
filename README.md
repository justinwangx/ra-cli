# Ra

Baseline ReAct CLI agent for OpenRouter-compatible models.

> [!WARNING] > `ra` is designed for **agentic evaluations that run in sandboxed environments**, as a baseline against more advanced CLI agents like Codex, Claude Code, and Gemini CLI. It can execute arbitrary shell commands and read/write files via tool calls. If you run it on your machine outside a sandbox, do so **at your own risk** and only in a workspace you’re comfortable exposing to the model.

## Quick Start

Install:

```sh
curl -fsSL https://raw.githubusercontent.com/justinwangx/ra-cli/main/install.sh | sh
```

The script installs `ra` into `/usr/local/bin` (if writable) or `~/.local/bin`.
Set `RA_REPO` to override the GitHub repo, or `RA_VERSION` to pin a tag.

Set your OpenRouter API key:

```sh
export OPENROUTER_API_KEY="..."
```

Run a quick one-liner task (defaults to **no-submit mode**; exits on the first assistant response):

```sh
ra \
  "Summarize the repo layout."
```

## Usage

- **Modes**:
  - **Single-shot (default)**: `ra "PROMPT"` exits after the first assistant response
  - **Exec/agent**: `ra --exec ...` (or `ra --prompt-file FILE`) continues until the model calls `submit`

### Examples

```sh
# Single-shot (default)
ra "Say hi back"

# Exec/agent mode for multi-step tasks
ra --exec "Summarize the repo layout and point out anything surprising."

# Run a longer task from a file (defaults to exec/agent mode)
ra --prompt-file /path/to/prompt.txt

# Write logs somewhere specific
ra --log-dir /tmp/ra-logs --exec "List files."

# Emit JSONL log stream to stdout at the end
ra --json --exec "List files."

# Stream JSONL log events to stdout as they happen
ra --stream-json --exec "List files."
```

Logs are written to a unique `ra-<timestamp>-<session_id>.jsonl` file in `--log-dir` (default: `--cwd`), or to `--log-path` if set. Format is a Codex
`exec --json`-style JSONL stream with `thread.started`, `turn.started`, `item.*`, and `turn.completed`.

## Install (Cargo)

From crates.io:

```sh
cargo install ra-cli
```

From source:

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

## Release

Package artifacts for distribution:

- `target/x86_64-unknown-linux-musl/release/ra`
- `target/aarch64-unknown-linux-musl/release/ra`
- `target/x86_64-apple-darwin/release/ra`
- `target/aarch64-apple-darwin/release/ra`
- Optional: `ra-macos-universal`
