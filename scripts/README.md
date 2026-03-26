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
cargo test -p tabor --test web_e2e agent_fixture_smoke -- --exact --nocapture
cargo test -p tabor --test web_e2e agent_wait_smoke -- --exact --nocapture
cargo test -p tabor --test web_e2e agent_artifacts_smoke -- --exact --nocapture
cargo test -p tabor --test web_e2e agent_events_smoke -- --exact --nocapture
```

Use `cargo xtask` as the primary macOS entrypoint; commands always replace the canonical app bundle at `/Applications/Tabor.app`.

Primary commands:

```sh
cargo xtask app
cargo xtask run
cargo xtask install --release --launch
```

`cargo xtask run-raw -- ...` is disabled on macOS because raw-binary launches bypass signed `Tabor.app` verification.

## Passkey-Enabled macOS Build Mode

Default macOS runs/builds intentionally skip the restricted passkey entitlement and disable WebAuthn so passkey pages cannot trigger system passkey flows unless you opt into passkey mode.

macOS signing defaults to Tiny Mile US, Corp (Team ID `7A5AR5N85X`) and fails fast if that identity is unavailable. Unsigned or ad-hoc-signed macOS Tabor builds are not supported, including debug builds.

Enable passkey mode explicitly:

```sh
cargo xtask run --passkey
```

For app signing with passkey entitlement, provide a provisioning profile:

```sh
TABOR_CODESIGN_PROVISIONING_PROFILE=/path/to/profile.mobileprovision cargo xtask app --passkey --release
```

Without `TABOR_CODESIGN_PROVISIONING_PROFILE`, passkey signing fails fast by design.

## Mac App Store Build Mode

Mac App Store builds use a separate sandboxed distribution lane. They are staged under `target/<profile>/mas/Tabor.app` and are never installed to `/Applications` by `cargo xtask`.

Stage the review build:

```sh
TABOR_MAC_APP_STORE_CODESIGN_IDENTITY="3rd Party Mac Developer Application: Tiny Mile US, Corp (TEAMID)" \
TABOR_CODESIGN_PROVISIONING_PROFILE=/path/to/Tabor-mas.provisionprofile \
cargo xtask app --mac-app-store --release
```

Package the App Store submission artifact:

```sh
TABOR_MAC_APP_STORE_CODESIGN_IDENTITY="3rd Party Mac Developer Application: Tiny Mile US, Corp (TEAMID)" \
TABOR_MAC_APP_STORE_INSTALLER_IDENTITY="3rd Party Mac Developer Installer: Tiny Mile US, Corp (TEAMID)" \
TABOR_CODESIGN_PROVISIONING_PROFILE=/path/to/Tabor-mas.provisionprofile \
cargo xtask package --mac-app-store --release
```

The stage-1 Mac App Store lane intentionally rejects `--passkey`. Passkey/WebAuthn work stays in the direct-distribution lane until Apple approves the restricted browser credential entitlement for the store build.

## Notarized macOS Release

For outside-App-Store distribution, use Developer ID signing with hardened runtime + timestamp, then notarize and staple.

Quick path (Apple ID + app-specific password):

```sh
TABOR_CODESIGN_IDENTITY="Developer ID Application: Tiny Mile US, Corp (7A5AR5N85X)" \
TABOR_CODESIGN_HARDENED_RUNTIME=1 \
TABOR_CODESIGN_TIMESTAMP=1 \
TABOR_NOTARY_APPLE_ID="your-apple-id@example.com" \
TABOR_NOTARY_APP_SPECIFIC_PASSWORD="xxxx-xxxx-xxxx-xxxx" \
TABOR_NOTARY_TEAM_ID="7A5AR5N85X" \
make notarize-dmg-universal
```

Keychain profile path (recommended for CI/local reuse):

```sh
xcrun notarytool store-credentials tabor \
  --apple-id "your-apple-id@example.com" \
  --team-id "7A5AR5N85X"

TABOR_CODESIGN_IDENTITY="Developer ID Application: Tiny Mile US, Corp (7A5AR5N85X)" \
TABOR_CODESIGN_HARDENED_RUNTIME=1 \
TABOR_CODESIGN_TIMESTAMP=1 \
TABOR_NOTARY_KEYCHAIN_PROFILE=tabor \
make notarize-dmg-universal
```

Underlying helper: `scripts/notarize-macos-app.sh`. It supports credentials via `TABOR_NOTARY_KEYCHAIN_PROFILE`, App Store Connect API key (`TABOR_NOTARY_API_*`), or Apple ID/password/team (`TABOR_NOTARY_APPLE_ID`, `TABOR_NOTARY_APP_SPECIFIC_PASSWORD`, `TABOR_NOTARY_TEAM_ID`).
