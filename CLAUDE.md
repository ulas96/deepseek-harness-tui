# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

**tub** is a Rust terminal UI (TUI) client for [DeepSeek Harness](https://github.com/deepseek-harness/deepseek-harness). It spawns a harness SDK runtime as a subprocess, drives it over the SDK's newline-delimited JSON-RPC stdio protocol, and renders live streaming session activity (assistant text, tool calls, diffs, subagents, todos) in the terminal. It is a standalone repository — never add Rust code inside the deepseek-harness monorepo itself, and tub never runs the harness's own test suite (it's a consumer, not the monorepo).

Full protocol/design rationale lives in `ARCHITECTURE.md` and `README.md` — read those before making protocol or lifecycle changes; this file only covers what a coding agent needs operationally.

## Commands

```sh
cargo build --workspace                 # build everything
cargo test --workspace                  # unit + fake-runtime + snapshot tests (keyless, no network, always run)
cargo test -p tub <test_name>           # run a single test by name
cargo test -p dsh-harness-client <name>
DSH_CHECKOUT=~/deepseek-harness cargo test -p tub --test keyless -- --include-ignored
                                         # M1: drives the REAL jsonrpc runtime from a harness checkout via
                                         # llm-replay overlay; self-skips (ignored) without DSH_CHECKOUT
cargo install --path crates/tub         # install the `tub` binary
```

CI runs `cargo fmt --all -- --check` and `cargo test --workspace` on Ubuntu. There is no rustfmt.toml/clippy.toml, so use the standard `cargo fmt` / `cargo clippy` conventions.

Running the binary requires a DeepSeek Harness checkout with `pnpm install` done since v1 launches the runtime from source or built lib, not a packaged executable. Explicit `--checkout`/`TUB_CHECKOUT` settings win; otherwise tub searches the current directory tree and common home-directory locations. The runtime config is embedded as the final fallback, and a supported Node is selected from PATH, NVM, or Homebrew (`TUB_NODE` overrides it).

## Workspace layout

Two crates:

- **`crates/dsh-harness-client`** — protocol client library, zero UI dependencies. The Rust design twin of the harness's TypeScript `@deepseek-ai/dsh-sdk-client` and the Python SDK: same runtime peer, same wire, same layering.
- **`crates/tub`** — the ratatui application (lib + bin `tub`, plus a second bin `tub-fake-runtime` used by tests).

### `dsh-harness-client` internals (in dependency order)

- `error` — typed error surface mirroring the TS client: `WireError{code,data?}`, `TransportClosedError` (exit code + bounded stderr tail), `SdkProtocolError`, `RequestTimeoutError`.
- `session` — the session-log vocabulary that **is** the wire contract: `SessionEvent` / `ContentBlock` / `StreamChunk` / `TurnEndReason` / `TodoItem` / `FileDiff` tagged unions, ported from the harness's `dsh-session` + `dsh-llm`. Envelopes parse strictly; variant payloads stay raw JSON behind typed accessors so unknown event types never fail the stream.
- `protocol` — the named request/result/notification shapes from `@deepseek-ai/dsh-sdk-protocol/types`.
- `client` — `HarnessClient`: lazy spawn, line framing (one compact JSON frame per `\n`), request/response correlation via string ids, notification fan-out to per-subscription queues, session-tree scoping from `subagent.started` lineage edges, reuse refusal after close, the teardown ladder.
- `api` — `DeepSeekHarness` / `HarnessSession`: memoized `initialize` (a failed handshake reaps the runtime and swaps in a fresh client), `run()` owning the receipt-to-idle interval, `run_observed()` adding the streaming observer used by the headless runner and tests.
- `launch` — checkout-aware launch-spec resolution (src mode via tsx, lib mode via plain Node).
- `fake_runtime` — a scripted JSON-RPC stdio peer for keyless tests (Rust port of the TS client's `fake-runtime.ts`): per-method scripted handlers, error/hang/exit behaviors, EOF/SIGTERM stubbornness flags for ladder coverage. Exposed as bin `fake-runtime` too.

### `tub` internals

- `cli` — clap surface (`tub run`, `tub tui`, bare `tub` defaults to the TUI).
- `config` — runtime configuration resolution (checkout, cordis.yml, launch mode, provider/model route, cwd), fails loudly on missing config.
- `headless` — `tub run`: one turn through the owned interval with a live transcript printer.
- `eventmap` — the **pure** event-to-UI fold: every transcript item is a function of the event stream. Streaming assistant text accumulates per (turn, step) until the committed `assistant/message` replaces it; tool cards open on `tool/call` and close on `tool/result` with elapsed timing; diff cards come from `tool/result` meta; subagent cards from `subagent.started`/`finished`; todo snapshots replace previous snapshots.
- `ui` — ratatui rendering: transcript windowing (unicode-width wrapping + scroll), status bar (session id, running/idle, provider/model, cwd@branch, elapsed, queued badge), input pane. Pure function of `UiState`, which is what makes `TestBackend` frame snapshots possible.
- `markdown` — pulldown-cmark via `tui-markdown` for committed assistant text. **Never regex-based markup parsing** — this was an explicit lesson carried from the harness's archived TypeScript TUI (see below).
- `app` — the event loop: notification stream + keyboard thread + redraw tick. Enter enqueues via the fast RPC; the receipt-to-idle transition is observed from the event stream (not assumed from the RPC response); queued prompts fire on idle; Ctrl+C asks for confirmation mid-turn because quitting abandons the turn by tearing down the runtime.

## Protocol & lifecycle invariants (do not violate silently)

These are load-bearing design decisions, not incidental implementation details — deviating requires updating `ARCHITECTURE.md` and calling it out explicitly:

- **Transport is SDK JSON-RPC over stdio, not ACP.** ACP only delivers committed final text (no streaming/tool activity/status), which is insufficient for a live TUI. Stdout carries **only** protocol frames; diagnostics go to stderr. The shipped `cordis.yml` must never mount a stdout logger — non-JSON stdout lines are treated as protocol violations.
- **One "turn" is client-defined**: enqueue `session/prompt` → wait until its `messageId` appears in the durable `agent/inbox/spliced` session event → collect events until the next whole-agent `idle` (`session.status`) → final response is the last committed root-session assistant text in that interval. There is no prompt-level result on the wire.
- **No mid-turn cancel exists.** Ctrl+C during a running turn abandons it by tearing down and re-spawning the runtime — never pretend a cancel RPC exists.
- **Teardown ladder** (idempotent, always reaps the child): protocol `shutdown` (~1s bound) → close stdin (EOF disposes the runtime immediately — keep stdin open until the turn ends) → SIGTERM (~3s) → SIGKILL. A closed client refuses reuse.
- **Credential pass-through**: `DEEPSEEK_API_KEY` / `DEEPSEEK_BASE_URL` reach the child via parent-environment pass-through, mirroring the TS client's scrub base — don't add credential handling logic beyond that.
- Unknown `SessionEvent`/`ContentBlock` variants must never fail parsing — the vocabulary is merge-extensible by design.

## Testing model (keyless-first)

Three tiers, named in `ARCHITECTURE.md`:

- **M0 (unit)** — `dsh-harness-client/tests/m0.rs` against the fake runtime: handshake, enqueue receipt, notification fan-out, error mapping, timeout abandonment, all three teardown-ladder tiers, stdin-EOF disposal, reuse refusal.
- **M1 (integration)** — `tub/tests/keyless.rs`, `#[ignore]`d by default, self-skips without `DSH_CHECKOUT`. Drives the **real** jsonrpc runtime from a harness checkout through the llm-replay overlay (mirrors `examples/jsonrpc-agent/tests/sdk.snapshot.ts` in the harness repo) and pins transcript, persisted JSONL, clean exit-0 shutdown. Run explicitly with `--include-ignored` when a checkout is available.
- **M2/M3 (snapshots)** — `tub/tests/snapshots.rs`: a scripted fake runtime drives the real client + transport stack; resulting UI items render to ratatui `TestBackend` frames whose exact contents are pinned.

No test tier needs a DeepSeek API key or network access; `cargo test --workspace` always runs the full keyless set.

## History/context worth knowing

The harness repo previously shipped a TypeScript TUI (`@deepseek-ai/dsh-tui`), deleted 2026-08-04. tub is its deliberate replacement, satisfying four conditions the removal note set for reintroducing a terminal frontend (see `ARCHITECTURE.md` § "Mapping to the removal note's four conditions" for the full mapping): a named product, an explicit package boundary (this standalone repo, not the harness's TS `packages/` tree), a concrete interaction provider (SDK JSON-RPC, not ACP), and assembled lifecycle + transcript acceptance (the M1 test). The old TUI's proven UX choices (session identity always visible, elapsed timing on messages, workspace/branch shown beside the prompt, real-parser markdown — never regex) are carried forward deliberately, not re-derived.
