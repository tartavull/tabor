<p align="center">
  <a href="https://github.com/tartavull/tabor/releases/latest/download/Tabor.dmg">
    <img alt="Download Tabor.dmg" src="https://img.shields.io/badge/Download%20for%20macOS-Tabor.dmg-black?style=for-the-badge&logo=apple">
  </a>
</p>

<p align="center">
  <img width="200" alt="Tabor Logo" src="https://raw.githubusercontent.com/tartavull/tabor/master/extra/logo/compat/tabor-term%2Bscanlines.png">
</p>

<h1 align="center">Tabor</h1>

<p align="center">
  A tabbed terminal built for fast, multi-context workflows.
</p>

<p align="center">
  <img alt="Tabor terminal preview" src="https://raw.githubusercontent.com/tartavull/tabor/master/extra/promo/tabor-readme.png">
</p>

## About

Tabor is a high-performance tabbed terminal focused on practical daily-driver
workflows: quick context switching, predictable behavior, and extensive
configuration.

Supported platforms: macOS, Linux, BSD, and Windows.

## Download

- macOS DMG (recommended): [Download Tabor.dmg](https://github.com/tartavull/tabor/releases/latest/download/Tabor.dmg)
- All release artifacts: [GitHub Releases](https://github.com/tartavull/tabor/releases)

## Installation

Detailed installation instructions are in [INSTALL.md](INSTALL.md).

## Features

Feature overview: [docs/features.md](./docs/features.md).

## Configuration

Configuration reference is available in `man 5 tabor` and in the manpage source
at [extra/man/tabor.5.scd](extra/man/tabor.5.scd).

Tabor looks for `tabor.toml` in the following locations:

1. `$XDG_CONFIG_HOME/tabor/tabor.toml`
2. `$XDG_CONFIG_HOME/tabor.toml`
3. `$HOME/.config/tabor/tabor.toml`
4. `$HOME/.tabor.toml`
5. `/etc/tabor/tabor.toml`

On Windows, the config file is searched at:

- `%APPDATA%\\tabor\\tabor.toml`

## Contributing

Contribution guidelines: [CONTRIBUTING.md](CONTRIBUTING.md).

## Acknowledgements

Tabor builds on the Alacritty codebase.

## License

Tabor is released under the [Apache License, Version 2.0](LICENSE-APACHE).
