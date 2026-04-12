# papagaia

Write with your voice. Rewrite with a shortcut.

Papagaia is a lightweight writing helper for Linux Wayland desktops, inspired by tools like Wispr Flow. It helps you get polished words onto the screen faster: speak naturally, clean up rough drafts, fix grammar, shorten text, and rewrite selections without leaving the app you are already using.

It is built for people who want a fast, keyboard-first writing flow on Linux: trigger a command from your compositor, talk or select text, and let Papagaia handle the cleanup.

- Turn speech into clean, ready-to-send writing
- Rewrite selected text with saved or ad-hoc prompts
- Stay in your current app instead of bouncing between tools
- Fit naturally into keyboard-driven Wayland workflows
- Bring your own speech-to-text CLI, LLM CLI, and input tools

Papagaia is intentionally:

- Linux-only
- Wayland-only
- focused on writing, editing, and dictation
- designed around compositor shortcuts and small CLI commands
- agnostic about the tools behind the workflow

## What Papagaia Is

It focuses on two everyday jobs:

1. Transform the current selection with a prompt, then paste the result back into the active application.
2. Record speech, transcribe it locally, and type the final text into the focused application.

In practice, that means you can draft with your voice, polish rough text instantly, and keep momentum while writing emails, notes, docs, commits, chat replies, or anything else that lives in a text box.

For prompt-based transformations, Papagaia delegates to whatever CLI tool you configure under `[engine]`. For dictation, it runs whatever speech-to-text command you configure under `[whisper]`. That means you can use `whisper.cpp`, a wrapper around `faster-whisper`, `whisperx`, or another CLI entirely, as long as it fits the configured command contract.

## Why You Might Want It

- You want to write faster without treating dictation as a separate app.
- You like the idea of Wispr Flow-style voice writing, but want something Linux- and Wayland-friendly.
- You want selection-based actions like "fix grammar", "shorten", or "rewrite clearly" available anywhere.
- You want local microphone transcription without depending entirely on a cloud dictation product.
- You want to mix and match your own speech, LLM, and input tooling instead of adopting a closed stack.
- You prefer tools that are small, inspectable, and easy to bind into your existing desktop workflow.

## How It Works

Papagaia uses a simple three-part model:

- `papagaia-daemon` stays alive in the background and does the actual work.
- `papagaia` is the tiny CLI you bind to shortcuts or run from the terminal.
- `papagaia-overlay` is a small HUD used for recording and status feedback.

For text transformation, the daemon follows a pragmatic Wayland path:

1. Trigger copy in the focused app.
2. Read the clipboard.
3. Render the selected text into a saved or ad-hoc prompt.
4. Run the configured engine command.
5. Paste or type the result back into the focused app.

For dictation, the daemon records audio, runs your configured speech-to-text command, and types the final text into the focused application. Optional post-processing can clean up punctuation, capitalization, filler words, and formatting.

## Build / Install

Build the whole workspace first:

```bash
cargo build --release
```

That gives you these binaries in `target/release/`:

- `papagaia`
- `papagaia-daemon`
- `papagaia-overlay`

For a local development flow, running through `cargo run` is also fine. For a more permanent setup, make sure the binaries are available on your `PATH` or live together in the same directory so `papagaia init` can find `papagaia-daemon`.

## Quick Start

1. Build the workspace.

```bash
cargo build --release
```

2. Generate a starter config.

```bash
./target/release/papagaia init
```

`papagaia init` writes `~/.config/papagaia/config.toml`, seeds a default prompt set, chooses sensible starter commands, tries to detect a usable engine, and attempts to install and start a systemd user service for `papagaia-daemon`.

3. Check your environment.

```bash
./target/release/papagaia doctor
```

This is the fastest way to see whether Papagaia can find your configured clipboard tools, typing backend, engine command, speech-to-text command, model path, and daemon service.

4. Confirm the daemon is available.

```bash
./target/release/papagaia status
```

If systemd setup was skipped or failed, start the daemon manually in another terminal:

```bash
./target/release/papagaia-daemon
```

5. Try a saved prompt.

```bash
./target/release/papagaia prompt list
./target/release/papagaia prompt run fix-grammar
```

6. Try dictation.

```bash
./target/release/papagaia dictate toggle
```

At this point, the usual next step is to bind these commands in your compositor.

## Everyday Usage

### Saved Prompts

List your configured prompt templates:

```bash
papagaia prompt list
```

Run one against the current selection:

```bash
papagaia prompt run shorten
papagaia prompt run fix-grammar
```

If a prompt runs while text is selected, Papagaia captures the selection, sends it through the configured engine, and replaces the selection with the result.

### Ad-hoc Prompts

Run a one-off prompt from the command line:

```bash
papagaia prompt raw --text 'Rewrite this more clearly: {{text}}'
papagaia prompt raw --text 'Summarize this in one sentence: {{text}}'
```

You can also pipe the prompt template over stdin:

```bash
printf 'Fix grammar and return only the corrected text: {{text}}' | papagaia prompt raw --stdin
```

If an ad-hoc prompt does not include `{{text}}` or `{{selection}}`, Papagaia automatically appends the selected text to the prompt.

### Prompt Picker

Open the overlay picker:

```bash
papagaia prompt pick
```

You can select a saved prompt from the list, or type plain text to run an ad-hoc prompt directly from the picker.

### Streaming Output

Some prompts work better when output is typed into the target app as it is generated:

```bash
papagaia prompt raw \
  --text 'Fix grammar and return only the corrected text: {{text}}' \
  --stream-output \
  --strip-markdown-fences false
```

Streaming uses the configured `type_command` instead of the clipboard paste path.

Important constraints:

- Streaming works best with engines that flush stdout progressively.
- Streaming prompts cannot use `strip_markdown_fences = true`.
- While streaming is active, focus stays in the target application so text can be typed incrementally.

### Dictation

Papagaia supports both toggle-style and explicit start/stop dictation:

```bash
papagaia dictate toggle
papagaia dictate start
papagaia dictate stop
```

By default, Papagaia transcribes audio with the command configured in `[whisper]` and types the final text into the focused application with the command configured in `[tools].type_command`. If `[dictation].post_process = true`, the transcript is also refined through your configured engine before it is typed.

Those defaults are not special. If you prefer a different speech-to-text CLI or different typing/input commands, you can replace them in the config.

## Compositor Integration

Papagaia is designed for compositors that can spawn commands from keybindings. Niri is a particularly good fit, but the overall model is generic.

Example Niri bindings:

```kdl
binds {
    Mod+Shift+S { spawn "papagaia" "prompt" "run" "shorten"; }
    Mod+Shift+G { spawn "papagaia" "prompt" "run" "fix-grammar"; }
    Mod+Shift+D { spawn "papagaia" "dictate" "toggle"; }
}
```

If your compositor supports press/release bindings, you can use push-to-talk semantics with:

- `papagaia dictate start` on key press
- `papagaia dictate stop` on key release

The same command model should work with any Wayland compositor that can launch shell commands from shortcuts.

## Configuration Overview

The config file lives at:

```text
~/.config/papagaia/config.toml
```

Print the exact path on your machine with:

```bash
papagaia config-path
```

The main sections are:

- `[tools]`: commands for reading the clipboard, writing the clipboard, simulating copy/paste, typing text, and clipboard timing.
- `[whisper]`: the speech-to-text command and model-related arguments used for dictation. The starter config uses `whisper.cpp`, but this section is intentionally generic.
- `[dictation]`: transcript post-processing, streaming behavior, focused-window context capture, and audio-debugging options.
- `[engine]`: the CLI command used for text transformation and optional dictation post-processing.
- `[[prompts]]`: saved prompt templates and their output-cleanup options.

`papagaia init` tries to generate sensible defaults for your machine. It can auto-detect engines such as `codex`, `claude`, GitHub Copilot through `gh copilot`, `llama.cpp`, and `gemini` when those tools are installed.

The important design point is that Papagaia is an orchestrator, not a closed AI stack. You can bring your own:

- speech-to-text CLI
- LLM CLI
- clipboard tools
- copy/paste injection commands
- text typing backend

## Troubleshooting

If something feels off, start with:

```bash
papagaia doctor
```

It reports missing commands, model paths, daemon state, and systemd service status.

Common issues:

- `wl-copy` or `wl-paste` missing: install `wl-clipboard`.
- `wtype` missing: install `wtype`, or configure `[tools]` to use `ydotool` instead.
- `ydotool` configured but not working: make sure `ydotoold` is installed and running.
- `whisper-cli` missing: install `whisper.cpp`, or replace `[whisper].argv` with the speech-to-text command you actually want to use.
- Whisper model path missing or wrong: update `[whisper].model` to point at a local model file if your chosen speech pipeline expects one.
- Engine command missing: install the configured CLI tool or update `[engine].argv`.
- `papagaia status` shows `stopped`: start `papagaia-daemon` manually or enable the systemd user service.
- `papagaia init` skips systemd setup: make sure `papagaia-daemon` is built and discoverable on your `PATH`, then run `papagaia init --force` again if needed.

If you are using the systemd user service and change your setup, you can restart it with:

```bash
papagaia restart
```