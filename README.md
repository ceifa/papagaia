# papagaia

Write with your voice. Rewrite with a shortcut.

A lightweight voice-writing and text-rewriting tool for Linux Wayland desktops, inspired by [Wispr Flow](https://wisprflow.com). Speak naturally, fix grammar, shorten text, or rewrite selections, all without leaving your current app.

- **Dictation**: record speech, transcribe locally, type the result into the focused app
- **Text transformation**: copy selection, run it through an LLM, paste the result back
- **Compositor-native**: designed around keybindings, small CLI commands, and Wayland tooling
- **BYO tooling**: plug in your own speech-to-text CLI, LLM CLI, clipboard tools, and typing backend

## Quick Start

```bash
cargo build --release
./target/release/papagaia init      # generates config, installs systemd service
./target/release/papagaia doctor    # checks your environment
./target/release/papagaia status    # confirms daemon is running
```

If systemd setup was skipped, start the daemon manually: `./target/release/papagaia-daemon`

## Usage

### Prompts

```bash
papagaia prompt list                  # list saved prompts
papagaia prompt run fix-grammar       # run a prompt on selected text
papagaia prompt run shorten
papagaia prompt pick                  # open the overlay picker
papagaia prompt raw --text 'Rewrite clearly: {{text}}'   # ad-hoc prompt
```

Ad-hoc prompts without `{{text}}` automatically append the selection. You can also pipe via `--stdin`.

Add `--stream-output` to type results incrementally into the target app.

### Dictation

```bash
papagaia dictate toggle     # toggle recording
papagaia dictate start      # explicit start
papagaia dictate stop       # explicit stop
```

Set `[dictation].post_process = true` in config to refine transcripts through your engine before typing.

## Compositor Bindings

Example for Niri:

```kdl
binds {
    Mod+Shift+S { spawn "papagaia" "prompt" "run" "shorten"; }
    Mod+Shift+G { spawn "papagaia" "prompt" "run" "fix-grammar"; }
    Mod+Shift+D { spawn "papagaia" "dictate" "toggle"; }
}
```

For push-to-talk, bind `dictate start` on key press and `dictate stop` on key release. Works with any Wayland compositor that can launch shell commands from shortcuts.

## Configuration

Config lives at `~/.config/papagaia/config.toml` (run `papagaia config-path` to confirm).

| Section | Purpose |
|---|---|
| `[tools]` | Clipboard read/write, copy/paste simulation, text typing commands |
| `[whisper]` | Speech-to-text command and model path |
| `[dictation]` | Post-processing, streaming, context capture, audio debug |
| `[engine]` | LLM CLI for text transformation |
| `[[prompts]]` | Saved prompt templates and cleanup options |

## Troubleshooting

Run `papagaia doctor` to diagnose issues. Common fixes: