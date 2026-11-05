# papagaia

Write with your voice. Rewrite with a shortcut.

A lightweight voice-writing and text-rewriting tool for Linux Wayland desktops, inspired by [Wispr Flow](https://wisprflow.com). Speak naturally, fix grammar, shorten text, or rewrite selections, all without leaving your current app.

- **Dictation**: record speech, transcribe locally, type the result into the focused app
- **Text transformation**: copy selection, run it through an LLM, paste the result back
- **Own hotkeys**: global keys watched via evdev — no compositor keybindings to set up
- **BYO tooling**: plug in your own speech-to-text CLI, LLM CLI, clipboard tools, and typing backend

## Install

On Arch Linux, install from the [AUR](https://aur.archlinux.org/packages/papagaia):

```bash
paru -S papagaia   # or: yay -S papagaia
```

Then set it up:

```bash
papagaia init      # generates config, installs systemd service
papagaia doctor    # checks your environment
papagaia status    # confirms daemon is running
```

## Build from source

```bash
cargo build --release
./target/release/papagaia init      # generates config, installs systemd service
./target/release/papagaia doctor    # checks your environment
./target/release/papagaia status    # confirms daemon is running
```

If systemd setup was skipped, start the daemon manually: `./target/release/papagaia-daemon`

## Performance

The daemon streams WAV uploads through a fixed 64 KiB buffer, so sending a long
recording does not allocate two additional copies of the audio. Cancelling a
request closes its connection immediately. The HUD keeps only the latest state,
updates mic levels at about 30 Hz, and skips repeated CSS changes; disabling it
also skips its tasks and mic-level calculations. Child commands and the HUD use
asynchronous process waits instead of dedicated waiting threads.

Local measurements against `cfccef9` (x86_64, Rust 1.94.1, release builds; median
of three runs with the same benchmark harness):

| Benchmark | Before | After |
|---|---:|---:|
| Peak process RSS during a 115.2 MB WAV upload | 223.3 MiB | 3.9 MiB |
| Upload to a loopback mock server | 52.5 ms | 34.2 ms |
| 20 commands whose descendants keep stdout/stderr open | 4.071 s | 2.049 s |

These isolate upload and subprocess overhead, excluding microphone capture,
Whisper model memory/inference, and desktop rendering. The upload fixture is a
sparse file sized like one hour of 16 kHz mono PCM; recording itself still stores
audio in memory. Results depend on the machine and filesystem.

Run each benchmark separately so its peak RSS belongs to that scenario:

```bash
cargo test -p papagaia-daemon --release stt::tests::benchmark_upload -- --exact --ignored --nocapture
cargo test -p papagaia-daemon --release clipboard::tests::benchmark_inherited_pipes -- --exact --ignored --nocapture
```

## Usage

### Prompts

```bash
papagaia prompt list                  # list saved prompts
papagaia prompt run fix-grammar       # run a prompt on the selection
papagaia prompt raw --text 'Rewrite clearly: {{text}}'   # ad-hoc prompt
```

The daemon grabs the current selection, runs it through the `[engine]`, and pastes the result.
Ad-hoc prompts without `{{text}}` append the selection; or pipe via `--stdin`. To open the
picker instead, bind a `pick` key (see [Keybinds](#keybinds)).

### Dictation

papagaia owns its hotkeys directly (see [Keybinds](#keybinds)) — you trigger dictation with a
key, not a CLI command. It's built for speed:

- **Warm transcription.** With `[whisper].backend = "server"` the daemon launches and
  supervises a `whisper-server` that keeps the model resident in memory, so each utterance
  skips the per-call model load. It falls back to `whisper-cli` automatically if the server
  isn't reachable. (`backend = "cli"` keeps the original per-call behaviour.)
- **Instant local cleanup, no LLM.** Fast rule-based polish on every transcript: voice commands
  ("new line"/"nova linha" → break, "period"/"ponto final" → `.`, …), word-repeat collapse,
  whitespace tidy-up, and capitalization. Configure under `[dictation.cleanup]`; filler removal
  is off by default (it can change meaning).

Short utterances aren't dropped, so quick commands work too. The recording HUD is a small
bottom-center pill with a live waveform.

Want an LLM to rewrite dictated text? Dictate it, then run a transform prompt on the
selection (`papagaia prompt run <name>`, or bind a `pick` key to choose from the picker) —
dictation itself stays fast and fully local.

## Keybinds

papagaia watches the keyboard directly (evdev) and owns all its hotkeys, so you configure
nothing in your compositor. Set them under `[keybinds]`:

```toml
[keybinds]
push_to_talk = "RightCtrl"   # hold to dictate, release to insert (Wispr Flow's default)
toggle = ""                  # tap to toggle hands-free dictation
pick = ""                    # tap to open the prompt picker
```

Each value is a key name (`RightCtrl`, `F13`, `Menu`, …) or a raw evdev keycode; empty means
no hotkey for that action. This needs read access to `/dev/input` — your user must be in the
`input` group (`papagaia doctor` checks this). Keys are *monitored*, not grabbed, so they still
pass through to the focused app; pick keys whose passthrough is harmless (a dead key like F13,
or RightCtrl).

To put a specific named prompt on a compositor shortcut, you can still bind
`papagaia prompt run <name>`.

## Configuration

Config lives at `~/.config/papagaia/config.toml` (run `papagaia config-path` to confirm).

| Section | Purpose |
|---|---|
| `[tools]` | Clipboard read/write and copy/paste key-injection commands |
| `[whisper]` | STT backend (`cli`/`server`), model path, and warm-server command |
| `[dictation.cleanup]` | Local rule-based transcript cleanup (voice commands, capitalization, …) |
| `[keybinds]` | Global hotkeys watched via evdev (push_to_talk / toggle / pick) |
| `[engine]` | LLM CLI for text transformation (the `prompt` commands) |
| `[[prompts]]` | Saved prompt templates |

## Troubleshooting

Run `papagaia doctor` to diagnose issues. It reports which required and optional
commands, models, and services are present, with a suggested fix for each one
that's missing.

Set `logging = true` in the config to get verbose daemon logs (visible via
`journalctl --user -u papagaia-daemon -f` when running under systemd).

To debug transcription quality, set `[dictation].recordings_dir` (empty by default,
so recordings are deleted as usual). Every recording is then kept there as
`<timestamp>.wav` alongside a `<timestamp>.txt` holding its raw transcript, up to
`recordings_keep` files. That gives you real utterances to replay against
different `[whisper]` settings instead of guessing at flags.