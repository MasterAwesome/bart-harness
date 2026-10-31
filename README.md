<h1 align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/assets/logo-wordmark-dark.svg">
    <img alt="▚▞ Bart" src=".github/assets/logo-wordmark-light.svg" height="56">
  </picture>
</h1>

<p align="center">A terminal coding agent that works through a persistent Python interpreter.</p>

Bart reads your code, runs commands, edits files and checks the results, in your
repository and your terminal. Instead of a fixed menu of tools, the model writes
short Python cells against one long-lived interpreter, so it can loop, filter and
combine steps the way you would in a notebook. You can follow every step and open
the exact code, output and file diffs behind it.

It works with a ChatGPT subscription, Claude Code subscription (via
`claude -p`), or any provider that speaks OpenAI Responses, Chat Completions,
Anthropic Messages or Gemini.

## Install

Linux (x86_64, aarch64) and macOS (Apple Silicon, Intel). Bart needs Python 3.10+
and Bash.

```sh
curl -fsSL https://github.com/MasterAwesome/bart-harness/releases/latest/download/install.sh | sh
```

The installer checks the download and installs Bart to `~/.local/bin`. If that
directory is not on your PATH, it prints the line to add to your shell configuration.
Then run `bart` in your project.

Or build from source with
`cargo install --locked --git https://github.com/MasterAwesome/bart-harness`.

Pasting screenshots needs `wl-paste` or `xclip` on Linux. The optional Linux
sandbox needs a systemd user session, `systemd-run` and bubblewrap.

Bart runs with your user permissions and does not ask before acting, like a
shell you hand to someone else. See [approval and sandbox](#approval-and-sandbox)
to review actions first.

## How it works

The model has one tool: run a Python cell. Inside a cell it calls helpers such as
`read`, `edit`, `write`, `run` and `bg_run` like ordinary functions, and its
variables stay alive between cells. This follows the idea of executable code
actions: one cell can read twenty files, run the tests and summarize the
failures, where a tool-per-step agent needs a round trip for each.

## Connect a model

Sign in once; later runs reuse your last choice.

```sh
bart login chatgpt            # a ChatGPT subscription (--device to sign in with a code)
```

Claude Code works too: install it and sign in with `claude auth login`, and it
appears in `/model`.

Any other provider is added with its protocol and base URL, with the API key on
stdin (`--no-key` for local servers):

```sh
bart login responses:openai   --url https://api.openai.com/v1 < key-file
bart login messages:anthropic --url https://api.anthropic.com/v1 < key-file
bart login gemini:gemini      --url https://generativelanguage.googleapis.com/v1beta < key-file
bart login chat:openrouter    --url https://openrouter.ai/api/v1 < key-file
bart login chat:local         --url http://localhost:8080/v1 --no-key
```

Inside Bart, `/model` switches provider and model and `/effort` sets the reasoning
effort; **Alt+=** and **Alt+-** step it while you type. `bart models <name>` lists
a provider's models.

## Add tools with MCP

```sh
bart mcp add local -- <command> <args...>
bart mcp add remote --url https://example.com/mcp --bearer-token-env MCP_TOKEN
```

The model finds and calls MCP tools from its Python cells. `/mcps` shows each
server's status and tools and turns servers on or off.

## Everyday use

| Key or command | Does |
| --- | --- |
| Enter | Send, or steer the turn that is running |
| Tab | Queue a message for after the turn |
| Esc | Interrupt |
| Ctrl+O | Open the full code, output and diffs of each step |
| Alt+Enter | New line |
| `@` | Insert a file path from your project |
| Ctrl+V | Paste a screenshot |
| `/resume`, `/rewind` | Continue an earlier session; undo the conversation back to a message (not your files) |
| `/compact` | Shrink the context and keep working |
| `/settings` | Approval, sandbox, tool view and other preferences |

Bart follows your project's `AGENTS.md`. Skills in `~/.agents/skills/<name>/SKILL.md`
are used by starting a message with `/<name>`.

`bart --prompt "…"` runs one turn without the interface, for scripts and CI.
`bart update` installs the latest release.

## Approval and sandbox

Both are off by default. Change them in `/settings`, or for one run with
`--approval` and `--jail`.

- **Approval `manual`** shows each cell's code before it runs; you allow or deny it.
- **Approval `auto`** has a reviewer model you choose judge each cell. It allows
  routine work that serves your request and asks you about the rest.
- **Sandbox** (Linux, experimental) runs the model's code with only your project
  writable. Its `.git` stays read-only. The rest of the machine, your home
  directory included, stays readable, and the network stays on unless you turn it
  off, so treat it as protection against accidents, not against a determined attacker.

Headless runs cannot ask you, so they refuse `manual`.

## Sessions and your data

Every session is saved under `~/.local/state/bart/sessions`, readable only by you.
`/resume` (or `bart --resume`) continues one; Python starts fresh, so earlier
variables are gone. Configuration and credentials live in `~/.config/bart`.

When reporting a bug, the session's `diagnostics.jsonl` is usually what helps: it
has request timings and outcomes but no message content or credentials. Do not
share `session.jsonl`, `output/` or `~/.config/bart`, which hold your
conversations and keys.

**Bart collects no telemetry:** It connects only to the model provider you
choose and its sign-in service, the MCP servers you add, and GitHub (for
`bart update`, and to read the current Codex release number when listing ChatGPT
models). Commands the model runs can use the network like any program you run.

## Limitations

- Linux and macOS only.
- On macOS, Bart cleans up a command's process group on a best-effort basis. A
  program that moves to its own process group or session can escape cleanup.
  A program that forks and exits quickly can also leave children running in
  the same group, even after Bart reports completion or tries to stop them.
- Python variables do not survive quitting; a resumed session starts a fresh
  interpreter. (Saving interpreter state is being worked on; for now the model
  is told when its state was cleared.)
- The Linux sandbox is experimental: your home directory stays readable and it
  is not a barrier against data leaving the machine.
- Bart is pre-1.0. Settings and the session format may still change.

`bart --help` lists every command-line option and `bart mcp` the MCP commands.

Licensed under MIT or Apache-2.0, at your option.
