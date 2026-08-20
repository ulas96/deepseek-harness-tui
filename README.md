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
provider/model/effort + workspace/branch, scrollback, and multiple sessions on one connection.

## Prerequisites

- Rust (stable toolchain)
- Node ^22.19 or >=24, and a DeepSeek Harness checkout with 'pnpm install'. tub uses a supported `node` on PATH or
  automatically selects one installed by NVM/Homebrew; `TUB_NODE` can override the executable.
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
| --checkout | TUB_CHECKOUT, DSH_CHECKOUT | auto-detected | DeepSeek Harness checkout path; searches the current tree and common home-directory locations |
| --config | TUB_CORDIS_CONFIG, DSH_CORDIS_CONFIG | ./cordis.yml, else embedded | runtime cordis.yml (tub embeds one modeled on examples/jsonrpc-agent/cordis.yml) |
| — | TUB_NODE | supported PATH/NVM/Homebrew Node | explicit Node executable override |
| --runtime-mode | DSH_EXAMPLE_MODE | src | 'src' boots the jsonrpc-demo bin through tsx; 'lib' boots its built lib |
| --provider | TUB_PROVIDER | deepseek-official | provider route for SDK-created agents |
| --model | TUB_MODEL | deepseek-v4-flash | model for SDK-created agents |
| --max-tokens | TUB_MAX_TOKENS | unset (adapter default) | output-token cap per request |
| --cwd | TUB_CWD | current directory | workspace recorded on every SDK-created session |

With a checkout in a common location such as `~/Documents/Github/deepseek-harness`, an installed binary starts with
just `tub`; no checkout or config exports are required. Explicit flags and environment variables always win, and an
invalid explicit path fails instead of silently falling back. Credentials (DEEPSEEK_API_KEY, DEEPSEEK_BASE_URL)
reach the child through tub's parent-environment pass-through.

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

### Slash commands

| Command | Action |
|---|---|
| `/model [provider/model]` | choose a model, or select an exact route directly |
| `/effort [default\|id]` | choose the reasoning effort for the active session |
| `/provider [route]` | choose a provider; dormant routes open onboarding |
| `/provider add` | discover and save a catalog or custom OpenAI-compatible provider |
| `/resume [session-id]` | reopen a previous root conversation from this exact workspace |
| `/init` | ask the agent to inspect the repo and create/update root `AGENTS.md` only |
| `//text` | send literal `/text` to the model instead of invoking a command |

Pickers use Up/Down, Enter, and Esc. Provider onboarding collects a route, optional display name, optional base URL,
protocol, credential reference, and masked key; it discovers models before saving. Profiles are stored in
`~/.dsh/settings.yaml`, while supplied keys go to the owner-only `~/.dsh/.credentials.yaml` credential store.
Model and effort changes are session-local and take effect at the next model step without discarding history.

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
- **Provider profiles are add-only in v1** - `/provider` can add catalog and custom OpenAI-compatible routes, but editing
  and removing existing profiles still belongs to the Harness settings surface.
- **No prompt-level status** - the wire has no per-prompt result; model errors and token-limit outcomes are rendered
  from the event stream, and tub's exit codes reflect transport health only.
- **stdout is the protocol** - the runtime's stdout carries only JSON-RPC frames. tub fails loudly on non-JSON stdout lines.
- **v1 needs a checkout** - the runtime launches from a DeepSeek Harness checkout (src or built lib mode). The packaged
  single-executable runtime is the documented production path.

## License

MIT, matching the harness repository.
