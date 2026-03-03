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

## Web E2E tests (macOS)

Run the integration test suite in `tabor/tests/web_e2e.rs` directly with Cargo:

```sh
cargo test -p tabor --test web_e2e -- --nocapture
```

Run a single smoke test:

```sh
cargo test -p tabor --test web_e2e web_popup_smoke -- --exact --nocapture
cargo test -p tabor --test web_e2e agent_browser_fixture_smoke -- --exact --nocapture
cargo test -p tabor --test web_e2e browser_clipboard_shortcut_smoke -- --exact --nocapture
```

Use `cargo xtask` as the primary macOS entrypoint; commands always replace the canonical app bundle at `/Applications/Tabor.app`.

Primary commands:

```sh
cargo xtask app
cargo xtask run
cargo xtask install --release --launch
```

`cargo xtask run-raw -- ...` is only for explicit raw-binary debugging and does not touch `/Applications/Tabor.app`.

## Passkey-Enabled macOS Build Mode

Default macOS runs/builds intentionally skip the restricted passkey entitlement so web tabs load reliably without an Apple-approved provisioning profile.

Enable passkey mode explicitly:

```sh
cargo xtask run --passkey
```

For app signing with passkey entitlement, provide a provisioning profile:

```sh
TABOR_CODESIGN_PROVISIONING_PROFILE=/path/to/profile.mobileprovision cargo xtask app --passkey --release
```

Without `TABOR_CODESIGN_PROVISIONING_PROFILE`, passkey signing fails fast by design.
