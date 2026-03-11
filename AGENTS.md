# Tabor Safety Guardrails

- Never launch Tabor on macOS from an unsigned bundle. This is a hard stop.
- Debug builds are not exempt. Never run `cargo run`, `cargo xtask run`, `cargo xtask app`, `cargo xtask install --launch`, `make run`, or open any `Tabor.app` bundle directly unless the exact bundle that will launch has already been signed.
- Treat ad-hoc or missing signatures as unsigned for safety-critical launch decisions. If signing cannot be completed first, do not run Tabor.
- Before any Tabor GUI launch, verify the bundle with `codesign --verify --deep --strict <path-to-Tabor.app>` and inspect `codesign -dvv <path-to-Tabor.app>`. If either command fails, stop.
- Prefer the canonical signed app path in `/Applications/Tabor.app`. Do not launch stray copies from `target/`, temporary staging directories, or other non-canonical locations.
- If a task does not require launching Tabor, do not launch it.
- When the user asks to tag a new Tabor version without specifying semver details, increment the minor version (the middle number), not the patch version, and keep the git tag plus repo/package metadata aligned to that version.
