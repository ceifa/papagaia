# papagaia

`papagaia` is a small Linux Wayland desktop utility for local dictation and
selection-based text transformation.

It is intentionally:

- Linux-only
- Wayland-native
- small and explicit
- tuned for compositor keybinds that spawn commands

It was designed around Niri, but the overall model is generic: keep a small
daemon running, then trigger actions from your compositor with a tiny CLI.

## Architecture

The workspace is split into three binaries:

- `papagaia-daemon`: long-lived Rust daemon for clipboard orchestration, subprocess execution, recording, whisper.cpp transcription, and overlay coordination
- `papagaia`: tiny CLI client that compositor keybinds can spawn
- `papagaia-overlay`: tiny GTK4 + layer-shell HUD for recording and loading feedback

## Runtime Dependencies

Core tools:

- `wl-clipboard`
- `wtype`

Dictation tools:

- `whisper.cpp` (`whisper-cli`)
- a local Whisper model file such as `ggml-base.bin`

Prompt engine:

- one CLI of your choice, configured in `[engine]`

Overlay build dependency:

- `gtk4-layer-shell` and the usual GTK4 development libraries

## Build

```bash
cargo build
```

## Setup

Generate a starter config based on what `papagaia` can detect on your machine:

```bash
cargo run -p papagaia-cli -- init
```

`init` tries to auto-select an engine from installed tools such as `gemini`,
`codex`, `claude`, GitHub Copilot (`gh copilot`), or `llama.cpp` (`llama-cli`).

If you want to overwrite an existing config:

```bash
cargo run -p papagaia-cli -- init --force
```

Then inspect the environment:

```bash
cargo run -p papagaia-cli -- doctor
```

The config lives at:

```text
~/.config/papagaia/config.toml
```

## First Run

Start the daemon:

```bash
cargo run -p papagaia-daemon
```

Then send commands from another shell:

```bash
cargo run -p papagaia-cli -- status
cargo run -p papagaia-cli -- prompt list
cargo run -p papagaia-cli -- prompt run fix-grammar
cargo run -p papagaia-cli -- prompt raw --text 'Rewrite this more clearly: {{text}}'
cargo run -p papagaia-cli -- dictate toggle
```

## Generic Wayland Usage

The intended interaction model is simple:

1. Start `papagaia-daemon` once in your session.
2. Bind compositor shortcuts that spawn `papagaia` commands.
3. Let `papagaia` do copy -> transform -> replace or record -> transcribe -> type.

Any compositor that can launch shell commands from keybinds should be able to
use this model.

## Niri Example

Niri is a particularly good fit because its keybinds naturally spawn commands:

```kdl
binds {
    Mod+Shift+S { spawn "papagaia" "prompt" "run" "shorten"; }
    Mod+Shift+G { spawn "papagaia" "prompt" "run" "fix-grammar"; }
    Mod+Shift+D { spawn "papagaia" "dictate" "toggle"; }
}
```

If you prefer press/release push-to-talk semantics, bind `dictate start` on key
press and `dictate stop` on release if your compositor supports that split.

## Prompt Commands

Use the prompt helper command when you want to inspect saved prompt templates or
run an ad-hoc one:

```bash
papagaia prompt list
papagaia prompt run shorten
papagaia prompt raw --text 'Refactor this code and return only the final code: {{text}}'
printf 'Summarize this in one sentence: {{text}}' | papagaia prompt raw --stdin
```

If an ad-hoc prompt does not contain `{{text}}` or `{{selection}}`, `papagaia`
appends the selected text automatically.

## Notes

- Replacement currently uses the pragmatic Wayland path: simulate copy, read the clipboard, run the transform, write the replacement to the clipboard, simulate paste.
- Dictation writes final text into the focused app with `wtype`.
- The transform setup is a single configurable `[engine]` plus prompt templates in TOML.
- `papagaia doctor` is the quickest way to see what a new machine is still missing.
- `papagaia init` is the quickest way to generate a reasonable first config for a new machine.
- `wtype` is the default text injection backend; `ydotool` can still be configured manually if you need it.
