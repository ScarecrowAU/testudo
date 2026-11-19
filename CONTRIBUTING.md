# Contributing to Testudo

Bug reports, runner requests and pull requests are welcome. For anything bigger than a small fix, open an issue first so we can agree on the approach before you spend time on it.

## Setting up

You need:

- A stable Rust toolchain ([rustup](https://rustup.rs)).
- Node 20 or newer, for the adapters and the example projects.
- On Linux, the libraries gpui links against. On Debian or Ubuntu:

  ```sh
  sudo apt install pkg-config libfontconfig-dev libfreetype-dev libwayland-dev \
    libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev libxcb1-dev libvulkan-dev
  ```

- Optionally ffmpeg, for MP4 export.

Then:

```sh
make dev                                         # rebuild on change, restart the GUI
make dev ARGS="~/dev/some-app --run playwright"  # args are passed to testudo
```

`make dev` only restarts the app after a successful build, so a compile error keeps the current window. Edits to `adapters/js` need no restart; every run loads them fresh. The app's log is in `target/dev/testudo.log`.

The projects in `examples/` are for pointing Testudo at:

- `acme-shop`: a small shop with a Playwright end-to-end suite and a Vitest unit suite. Playwright serves the shop's pages at `https://acme.shop/`, so the tests need no server or network. The README screenshots come from it.
- `playwright-demo`: three bare Playwright tests, one of them failing on purpose.
- `vitest-browser-demo`: Vitest browser mode, the setup Storybook's addon-vitest uses.

```sh
(cd examples/acme-shop && npm install && npx playwright install chromium-headless-shell)
(cd examples/playwright-demo && npm install && npx playwright install chromium-headless-shell)
(cd examples/vitest-browser-demo && npm install)
make dev ARGS="examples/acme-shop --run all"
```

To retake the README screenshots after a UI change, run `make screenshots`. To retake only some, name them, for example `scripts/screenshots.sh live focus`. The script needs Linux with KDE Plasma 6. It runs Testudo in a headless KWin session for each shot and captures the window with Spectacle. It works in a throwaway home folder, so the shots show no real paths or history. If the live shots catch the wrong moment on your machine, change `LIVE_WAIT` and `UNIT_WAIT`, which are in seconds.

## Before opening a pull request

```sh
make test   # cargo test --workspace
make lint   # clippy, warnings are errors
cargo fmt
```

CI runs the tests on Linux and macOS for every pull request that touches code.

For changes to recording, adapters or the UI, also run a real suite through Testudo, from an example project or your own, and check the result in the live view and in History. Include a screenshot or short recording for UI changes.

## Conventions

- **Tests** live next to the code they cover, in `#[cfg(test)]` modules. Add a regression test with a bug fix where the behaviour can be tested without a browser.
- **Formatting** comes from `cargo fmt`, configured in `rustfmt.toml`. CI fails on unformatted code.
- **Errors** use `anyhow::Result` with `.context(...)`. Failures in background threads are logged with `tracing::warn!`; they must never take down the app or a run.
- **Comments** explain why, not what. Modules start with a `//!` comment describing what they're for.
- **Adapters** in `adapters/js` are plain ESM with no dependencies, and must never write to or install anything in the user's project. They must do nothing when `TESTUDO_SOCKET` isn't set.
- **The protocol** in `crates/protocol` is mirrored by hand in `adapters/js/transport.mjs`. Change both together.
- **GPU memory** for an image stays in use until `window.drop_image` is called, because gpui keeps every image it has drawn. Code that replaces a frame must hand the old one to the retire queue in `crates/ui/src/model.rs`.
- **The README** describes how Testudo works now. Update it in the same pull request when behaviour, commands or settings change.

## Pull requests

- Keep each pull request to one change. Several small ones review faster than one big one.
- Title it in conventional-commit style: `fix: …`, `feat: …`, `docs: …`, `chore: …`, `ci: …`.
- Fill in the template: what and why, how you tested it, and the checklist.
- Pull requests need the `CI` check to pass and a maintainer's approval, and are squash-merged.

## Reporting bugs and requesting runners

Use the [issue forms](https://github.com/ScarecrowAU/testudo/issues/new/choose). For a bug, the Testudo version, runner version and OS matter most, plus the log if there is one. For a new runner, describe how you run it and whether it drives a browser.

## License

By contributing, you agree that your contributions are dual-licensed under the [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE) licenses, like the rest of Testudo.
