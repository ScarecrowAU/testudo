# Testudo

Native GUI test runner that controls, monitors and reports on multiple concurrent test suites, with live views of the headless browsers they drive.

Current targets: Playwright, plus Vitest browser mode (which covers Storybook's `addon-vitest`). Planned: Jest, Rstest, then non-JS runners (Go, Rust).

## How it works

```
 testudo (Rust, egui)                       user's project (untouched)
┌──────────────────────────┐   spawn    ┌──────────────────────────────────────┐
│ Orchestrator ────────────┼──────────▶ │ playwright test / vitest run         │
│                          │  env:      │   reporter  ◀─ PW_TEST_REPORTER /    │
│ IngestServer (UDS) ◀─────┼─────┐      │               --reporter=<abs path>  │
│   │                      │     │      │   workers   ◀─ NODE_OPTIONS=--import │
│   ▼                      │     │      │               preload.mjs            │
│ Hub ──▶ bridge (JPEG     │     │      │     └ playwright-core instrumentation│
│         decode) ──▶ UI   │     └──────┤       page.screencast frames,        │
└──────────────────────────┘  frames +  │       network, console, navigation   │
                              events    └──────────────────────────────────────┘
```

Nothing is installed into or written to the user's project. Instrumentation is injected entirely through the runner's command line and environment:

| Piece | Injection | Provides |
|---|---|---|
| `adapters/js/playwright-reporter.mjs` | `PW_TEST_REPORTER` env (appended after configured reporters) | test/step lifecycle |
| `adapters/js/vitest-reporter.mjs` | `--reporter` CLI flags | test lifecycle |
| `adapters/js/preload.mjs` | `NODE_OPTIONS=--import` | live JPEG frames, network, console, per page, tagged with the Playwright test id |

The preload hooks `playwright-core` through its client instrumentation (`runAfterCreateBrowserContext`), the same hook Playwright Test's tracing uses, and falls back to method wrapping on older versions. It resolves every copy of playwright-core reachable from the project root (pnpm/monorepo safe). Both mechanisms are internal Playwright APIs: pin and test against supported versions.

## Layout

| Path | What |
|---|---|
| `crates/protocol` | Wire format: length-prefixed JSON events and binary JPEG frames. Mirrored in `adapters/js/transport.mjs`. |
| `crates/core` | Orchestrator (spawn, process-group cancel), ingest socket, hub. `testudo-headless` binary prints the stream. |
| `crates/app` | `testudo` GUI: egui + egui_tiles; panes appear/disappear as pages open/close. |
| `adapters/js` | Zero-dependency Node adapters shipped with Testudo. |
| `examples/` | Playwright and Vitest-browser demo projects for smoke testing. |

## Running

```sh
# demos need deps + browsers once
(cd examples/playwright-demo && npm install && npx playwright install chromium-headless-shell)
(cd examples/vitest-browser-demo && npm install)

cargo run -p testudo-app -- examples/playwright-demo --run playwright   # GUI, auto-start a run
cargo run -p testudo-core --bin testudo-headless -- vitest examples/vitest-browser-demo
```

`--run` is optional; the top bar starts runs of any kind against any directory, and several can run at once.

## Known gaps / next steps

- Unix only (UDS + process groups). Windows needs a named-pipe listener and Job objects.
- No persistence yet: recording (store timestamped JPEGs + events, export via ffmpeg) and SQLite run history.
- Vitest frames are per run, not per test (all tests share one page); correlate by reporter timestamps.
- Direct CDP attach from Rust for Chromium under non-JS runners (Go/chromedp etc).
- Jest, Rstest, `go test -json`, `cargo nextest` adapters.
- Open Playwright traces / HTML reports in the system browser.
