# tub architecture

tub is a Rust workspace with two crates:

- **dsh-harness-client** - the protocol client library (zero UI dependencies). The design twin of the harness's
  TypeScript '@deepseek-ai/dsh-sdk-client' and the Python 'deepseek-harness' SDK: same runtime peer, same wire,
  same layering.
- **tub** - the ratatui application (lib + bin).

## The runtime ownership model

tub owns the runtime subprocess for its whole life. Launch specs mirror the harness's example-launch resolver:
source mode boots 'packages/examples/jsonrpc-demo/src/bin.ts' through the tsx loader with
'TSX_TSCONFIG_PATH' pointed at the checkout tsconfig; built mode boots 'packages/examples/jsonrpc-demo/lib/bin.js'
under plain Node. The config path is passed positionally (argv[2]); the runtime's own discovery still applies
('DSH_CORDIS_CONFIG' env wins - the keyless tests exploit exactly that).

The child environment is the parent environment verbatim plus mode-specific entries: tub's credential policy is
pass-through, so DEEPSEEK_API_KEY / DEEPSEEK_BASE_URL reach the runtime. stdout stays reserved for JSON-RPC frames -
tub's shipped cordis.yml mounts no stdout logger, and the client records non-JSON stdout lines as protocol
violations that the UI renders loudly.

## Layering

dsh-harness-client:

- **error** - the typed error surface mirroring the TS client: WireError { code, data? } preserves the wire error
  response; TransportClosedError carries exit code + a bounded stderr tail; SdkProtocolError and RequestTimeoutError
  are distinct variants.
- **session** - the session-log vocabulary that IS the wire contract: SessionEvent / ContentBlock / StreamChunk /
  TurnEndReason / TodoItem / FileDiff tagged unions, ported from dsh-session + dsh-llm. The vocabulary is
  merge-extensible, so envelopes parse strictly but variant payloads stay raw JSON behind typed accessors; unknown
  event types never fail the stream.
- **protocol** - the named request/result/notification shapes from '@deepseek-ai/dsh-sdk-protocol/types'.
- **client** - HarnessClient: lazy spawn, line framing, request/response correlation (string ids, one compact JSON
  frame per newline-terminated line), notification fan-out to per-subscription queues, session-tree scoping from
  subagent.started lineage edges, reuse refusal after close, and the teardown ladder.
- **api** - DeepSeekHarness / HarnessSession: memoized initialize (a failed handshake reaps the runtime and swaps in
  a fresh client), and run() owning the receipt-to-idle interval. run_observed() adds the streaming observer the
  headless runner and tests use.
- **launch** - checkout-aware launch-spec resolution (src via tsx, lib via plain Node).
- **fake_runtime** - a scripted JSON-RPC stdio peer for keyless tests (the Rust port of the TS client's
  fake-runtime.ts): per-method scripted handlers, error/hang/exit behaviors, EOF/SIGTERM stubbornness flags for
  ladder coverage.

tub:

- **config** - runtime configuration resolution (checkout, cordis.yml, mode, route, cwd) with loud failures.
- **headless** - 'tub run': one turn through the owned interval with a live transcript printer.
- **eventmap** - the pure event-to-UI fold: every transcript item is a function of the event stream. Streaming
  assistant text accumulates per (turn, step) until the committed assistant/message replaces it; tool cards open on
  tool/call and close on tool/result with elapsed timing; fs diff cards come from the tool/result meta; subagent
  cards from subagent.started/finished; todo snapshots replace previous snapshots.
- **ui** - ratatui rendering: transcript windowing (unicode-width wrapping + scroll), status bar (session id,
  running/idle, provider/model, cwd@branch, elapsed, queued badge), input pane. Pure function of UiState, so
  TestBackend frame snapshots pin it.
- **markdown** - pulldown-cmark via tui-markdown (a real parser; the archived TUI notes forbid regex-based markup
  fallbacks).
- **app** - the event loop: notification stream + keyboard thread + redraw tick; Enter enqueues (the fast RPC), the
  receipt-to-idle transition is observed from the stream; queued prompts fire on idle; Ctrl+C confirms mid-turn
  because quitting abandons the turn by tearing down the runtime.

## One turn is client-defined

Mirroring the TS client's DeepSeekHarness.run(): enqueue session/prompt -> wait until its messageId appears in the
durable 'agent/inbox/spliced' receipt -> collect events until the next whole-agent idle (session.status) -> the
final response is the last committed root-session assistant text in the interval. The result carries no prompt-level
status; steering, injected context, and other queued work may contribute before idle.

## The teardown ladder

close() requests protocol shutdown (bounded ~1s), then closes stdin (EOF: the runtime disposes immediately - keep
stdin open until the turn ends), then SIGTERM (~3s), then SIGKILL (~3s), always reaping the child. Idempotent; a
closed client refuses reuse. A stream-settle window lets the exit edge and the stderr reader settle before the
closed error freezes, so diagnostics carry the real exit code and stderr tail.

## Keyless testing

- M0 (unit): the fake runtime covers handshake, enqueue receipt, notification fan-out, error mapping, timeout
  abandonment, all three ladder tiers, stdin-EOF disposal, and reuse refusal.
- M1 (integration): 'tests/keyless.rs' drives the REAL jsonrpc runtime from a checkout through the llm-replay overlay
  (DSH_CORDIS_CONFIG -> cordis.snapshot.yml, DSH_SNAPSHOT_FILE -> fixture, mirroring
  examples/jsonrpc-agent/tests/sdk.snapshot.ts) and pins the transcript, the persisted JSONL session, and a clean
  exit-0 shutdown. It self-skips without DSH_CHECKOUT.
- M2/M3 (snapshots): a scripted fake runtime drives the real client + transport stack; the resulting items render to
  ratatui TestBackend frames whose exact contents are pinned (the old TUI's terminal-state snapshot idea).

Default CI needs no API key and runs the fake-runtime and snapshot suites. The M1 replay integration remains
opt-in because it requires a prepared DeepSeek Harness checkout. tub never runs the harness's own test suite - it
is a consumer, not the monorepo.

## Mapping to the removal note's four conditions

1. **A named product or deployment** - tub is the named product with its own repository, CLI, and shipped cordis.yml.
2. **An explicit package boundary** - a standalone Rust repository; nothing enters the harness monorepo's
   TypeScript 'packages/' tree.
3. **A concrete interaction provider** - the SDK JSON-RPC stdio protocol, not ACP: the wire streams full
   session-log envelopes plus whole-agent running/idle transitions, which is exactly what a live TUI renders.
4. **Assembled lifecycle and transcript acceptance for that frontend** - M1's keyless integration drives the real
   runtime composition (server + adapter replay + spine + tools + persistence) through spawn -> initialize ->
   prompt -> receipt-to-idle -> shutdown, pinning transcript and persisted JSONL.

## The UX backlog carried from the old TUI

- session identity stays visible during long conversations (status bar, every frame),
- elapsed timing and phase status attached to messages (tool cards carry elapsed ms; turn end carries its reason),
- workspace and branch context shown beside the prompt (status bar: cwd@branch),
- conservative XML-wrapper parsing for human-readable fallback output - tub renders committed assistant text
  through a real markdown parser (pulldown-cmark), never regex.
