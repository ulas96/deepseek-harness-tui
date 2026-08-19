# tub

**tub** is an interactive Rust terminal client for [DeepSeek Harness](https://github.com/deepseek-harness/deepseek-harness).
It spawns a harness SDK runtime as a subprocess, drives it over the JSON-RPC stdio protocol, renders live session activity
(streaming assistant text, tool calls, diffs, subagents, todos) in your terminal, and owns the runtime lifecycle:
spawn -> initialize -> prompt -> collect-to-idle -> shutdown -> reap.

This is a new product frontend for DeepSeek Harness, complementary to the Web GUI, ACP, JSON-RPC, and one-shot CLI
entry points. It exists because the harness removed its TypeScript TUI; tub deliberately satisfies the four conditions
for reintroducing a terminal frontend (see ARCHITECTURE.md for the mapping):

1. a named product or deployment - **tub**,
2. an explicit package boundary - a separate Rust repository (never a 'packages/' entry),
3. a concrete interaction provider - the SDK JSON-RPC stdio protocol (ACP delivers committed final text only;
   the SDK protocol streams full session-log envelopes),
4. assembled lifecycle and transcript acceptance - the keyless test below drives the real harness end to end.

## Demo

```
$ tub run --checkout ~/deepseek-harness --prompt "say hi"
-- tub - session-2f1a - deepseek-official/deepseek-v4-flash - /Users/you/project --
>> running
-- turn 1 --
you: say hi
assistant (turn 1, step 1):
  Hello! I'm tub's runtime assistant.
  [usage in=1769 out=24]
-- turn 1: completed
>> idle
----
final response: Hello! I'm tub's runtime assistant.
tub: turn took 2.3s
```

The interactive TUI renders the same turn live: streamed assistant text (markdown once committed), tool cards with
elapsed timing, fs diff cards, subagent cards, todo snapshots, a status bar carrying session id + running/idle +
provider/model + workspace/branch, scrollback, and multiple sessions on one connection.

## Prerequisites

- Rust (stable toolchain)
- Node ^22.19 or >=24, and a DeepSeek Harness checkout with 'pnpm install' (v1 launches the runtime from a checkout;
  a packaged single-executable runtime is documented future work)
- DEEPSEEK_API_KEY (and optionally DEEPSEEK_BASE_URL) in the environment for live runs - tub passes the parent
  environment through to the runtime verbatim
- Keyless runs (no key, no network) work through the harness's llm-replay snapshot overlay

## Build and install

```sh
cargo build --release           # from this repository root
cargo install --path crates/tub # installs the tub binary
```

## Usage

```sh
tub                              # interactive TUI (default)
tub tui --checkout ~/deepseek-harness
tub run --prompt "say hi"        # one headless turn
tub run --file prompt.md --session my-session --json
```

Configuration (CLI flags win over env):

| Flag | Env | Default | Meaning |
|---|---|---|---|
| --checkout | TUB_CHECKOUT, DSH_CHECKOUT | required | DeepSeek Harness checkout path |
| --config | TUB_CORDIS_CONFIG | ./cordis.yml, else the shipped one | runtime cordis.yml (tub ships one modeled on examples/jsonrpc-agent/cordis.yml) |
| --runtime-mode | DSH_EXAMPLE_MODE | src | 'src' boots the jsonrpc-demo bin through tsx; 'lib' boots its built lib |
| --provider | TUB_PROVIDER | deepseek-official | provider route for SDK-created agents |
| --model | TUB_MODEL | deepseek-v4-flash | model for SDK-created agents |
| --max-tokens | TUB_MAX_TOKENS | unset (adapter default) | output-token cap per request |
| --cwd | TUB_CWD | current directory | workspace recorded on every SDK-created session |

The runtime's own config discovery still applies inside the child: DSH_CORDIS_CONFIG env wins over the positional
config path tub passes. Credentials (DEEPSEEK_API_KEY, DEEPSEEK_BASE_URL) reach the child through tub's
parent-environment pass-through.

### TUI keys

| Key | Action |
|---|---|
| Enter | send the prompt (Alt+Enter inserts a newline) |
| Tab / Ctrl+N | switch to the next session / mint a fresh session |
| Shift+Up/Shift+Down, PgUp/PgDn | scroll the transcript |
| Ctrl+C, q, Esc | quit (orderly runtime teardown) |

While a turn is running, Ctrl+C asks for confirmation first: quitting abandons the turn by tearing down the
runtime - the wire has **no mid-turn cancel**. A prompt typed while the agent is running is queued and sent when the
agent next idles (shown as a [+1 queued] badge).

## Keyless testing

No key or network needed. Unit + fake-runtime tests always run:

```sh
cargo test --workspace
```

The M1 integration test drives the REAL jsonrpc runtime from a checkout through the llm-replay overlay
(the same mechanics as examples/jsonrpc-agent/tests/sdk.snapshot.ts) and pins transcript, persisted JSONL, and a
clean exit-0 shutdown. It self-skips without DSH_CHECKOUT:

```sh
DSH_CHECKOUT=~/deepseek-harness cargo test -p tub --test keyless -- --include-ignored
```

The M2/M3 snapshot suite drives a scripted fake runtime through the real client + transport stack and pins ratatui
TestBackend frames (streaming text, tool cards with timing, diff cards, subagent cards, todos, status bar).

## Known limitations

- **No approval/permission dialogs** - the SDK wire does not carry approval requests; there is nothing to render.
- **No session resume/listing** - SDK sessions live until process shutdown; rehydration from the runtime's JSONL is future work.
- **No prompt-level status** - the wire has no per-prompt result; model errors and token-limit outcomes are rendered
  from the event stream, and tub's exit codes reflect transport health only.
- **stdout is the protocol** - the runtime's stdout carries only JSON-RPC frames. tub fails loudly on non-JSON stdout lines.
- **v1 needs a checkout** - the runtime launches from a DeepSeek Harness checkout (src or built lib mode). The packaged
  single-executable runtime is the documented production path.

## License

MIT, matching the harness repository.
