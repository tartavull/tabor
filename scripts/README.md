Scripts
=======

## Flamegraph

Run the release version of Tabor while recording call stacks. After the
Tabor process exits, a flamegraph will be generated and it's URI printed
as the only output to STDOUT.

```sh
./create-flamegraph.sh
```

Running this script depends on an installation of `perf`.

## ANSI Color Tests

We include a few scripts for testing the color of text inside a terminal. The
first shows various foreground and background variants. The second enumerates
all the colors of a standard terminal. The third enumerates the 24-bit colors.

```sh
./fg-bg.sh
./colors.sh
./24-bit-colors.sh
```

## Web popup smoke test (macOS)

Runs the Rust integration test `web_popup_smoke` from `tabor/tests/web_e2e.rs`.

```sh
./web-popup-smoke.sh
```

## Agent-browser verification

Runs the Rust integration test `agent_browser_fixture_smoke` from
`tabor/tests/web_e2e.rs`.

```sh
./verify-agent-browser.sh
```

## Browser Clipboard Shortcut Test (macOS)

Runs the Rust integration test `browser_clipboard_shortcut_smoke` from
`tabor/tests/web_e2e.rs` for `Meta+C` / `Meta+V` behavior in web tabs.

```sh
./verify-browser-clipboard-red.sh
```
