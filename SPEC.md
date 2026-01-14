# Specification

This document specifies the behavior of `ra`.

## Scope

The spec covers:

- Agent control flow (request/response loop)
- Tool surface (names, parameters, key semantics)
- Termination behavior
- Context overflow behavior

## Definitions

- Step: one model request and its resulting assistant message, optionally followed by execution of (at most) one tool call.
- Submit-enabled: a mode where the agent continues until the model calls `submit(answer)`.

## Invariants

- At most one tool call is executed per step.
- `parallel_tool_calls` is always disabled in the model request.
- Sampling parameters are only sent if explicitly configured (otherwise provider defaults apply).
- Web tools are not present unless explicitly enabled.
- Tool outputs are bounded (pagination and/or truncation) to limit context growth.

## Prompting

The system prompt is fixed and tool-oriented. It includes the tool list, an environment header (including the working directory), and the constraint “use at most one tool call per step”.

If `AGENTS.md` exists in the current directory or any parent directory, its contents are appended to the system prompt (concatenated along the directory chain).

When submit is enabled and the model returns an assistant message without tool calls, `ra` appends a fixed “continue” user message and performs another step.

## Tools

Tools are exposed as function tools with JSON arguments.

Always available:

- `shell_command(command, workdir?, timeout_ms?, max_output_chars?)`
- `read_file(file_path, offset?, limit?)` (1-indexed pagination)
- `list_dir(dir_path, offset?, limit?, depth?)` (1-indexed pagination; bounded depth)
- `grep_files(pattern, path?, include?, limit?)`
- `apply_patch(patch)` (unified diff)

Optionally available (only when enabled):

- `web_search(query, max_results?)` (requires a configured API key; see `README.md`)
- `web_open(url, offset?, limit?)` (returns extracted, line-numbered plaintext)
- `web_find(url, pattern, max_results?, context_lines?)` (returns matching line ranges/snippets)

Pagination constraints:

- `offset` values are 1-indexed where applicable.
- `limit`/`depth` must be \(\ge 1\).

## Agent loop

`ra` runs a basic ReAct tool-use loop.

Given a task \(T\):

1. Initialize `messages` with:
   - one system message (system prompt)
   - one user message containing \(T\)
2. Repeat:
   - Enforce optional `max_steps` and `time_limit` if configured.
   - Send a Chat Completions request with:
     - `messages`
     - tool schemas
     - `tool_choice: "auto"`
     - `parallel_tool_calls: false`
   - Append the assistant message to `messages`.
   - If the assistant message contains tool calls:
     - If the first tool call is `submit` and submit is enabled, terminate and return `answer`.
     - Execute exactly one tool call (the first).
     - Append a tool result message (`role: "tool"`) with the matching `tool_call_id`.
     - Any additional tool calls in that same assistant message receive tool error results indicating they were not executed.
   - If the assistant message contains no tool calls:
     - If submit is enabled: append the fixed “continue” message and repeat.
     - Otherwise: terminate and return the assistant text.

## Context management

On a context window exceeded error from the upstream provider, `ra` prunes the message history and retries:

- Keep all system messages.
- Keep the initial user task message.
- Preserve assistant↔tool call/response pairs.
- Drop the oldest portion of the remaining conversation (preferably at a user-message boundary).

If pruning does not recover, the run terminates with an error.

## Safety

`ra` does not provide a sandbox. It can execute shell commands and modify files via patch application. When web tools are enabled, it can make outbound HTTP requests. Run it in a sandboxed environment if you need isolation.
