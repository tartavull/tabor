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

Runs a popup smoke test using IPC to verify `window.open` creates a new web tab
and `window.opener.postMessage` reaches the opener. The popup is created from
`about:blank` with a `link[rel="icon"]` so the script also checks that a favicon
request hits the local HTTP server. Requires macOS and `python3`. If
`./scripts/run.sh` is available it is used automatically. Set `TABOR_BIN` to
override.

```sh
./web-popup-smoke.sh
```

Environment overrides:
- `TABOR_BIN` to point at a custom Tabor binary.
- `PYTHON_BIN` to use a different Python executable.
