<p align="center">
    <img width="200" alt="Tabor Logo" src="https://raw.githubusercontent.com/tartavull/tabor/master/extra/logo/compat/tabor-term%2Bscanlines.png">
</p>

<h1 align="center">Tabor - a tabbed terminal forked from Alacritty</h1>

<p align="center">
  <img alt="Tabor - a tabbed terminal forked from Alacritty"
       src="https://raw.githubusercontent.com/tartavull/tabor/master/extra/promo/tabor-readme.png">
</p>

<p align="center">
  <a href="https://github.com/tartavull/tabor/releases/latest/download/Tabor.dmg">
    <img alt="Download Tabor.dmg" src="https://img.shields.io/badge/Download-black?style=for-the-badge&logo=apple">
  </a>
</p>

## About

Tabor is a tabbed terminal forked from Alacritty that comes with sensible
defaults, but allows for extensive [configuration](#configuration). By
integrating with other applications, rather than reimplementing their
functionality, it manages to provide a flexible set of [features](./docs/features.md)
with high performance. The supported platforms currently consist of BSD, Linux,
macOS and Windows.

The software is considered to be at a **beta** level of readiness; there are
a few missing features and bugs to be fixed, but it is already used by many as
a daily driver.

Precompiled binaries are available from the [Tabor GitHub releases page](https://github.com/tartavull/tabor/releases).

Join [`#tabor`] on libera.chat if you have questions or looking for a quick help.

[`#tabor`]: https://web.libera.chat/gamja/?channels=#tabor

## Features

You can find an overview over the features available in Tabor [here](./docs/features.md).

## Further information

- [Announcing Alacritty, a GPU-Accelerated Terminal Emulator](https://jwilm.io/blog/announcing-alacritty/) January 6, 2017
- [A talk about Alacritty at the Rust Meetup January 2017](https://www.youtube.com/watch?v=qHOdYO3WUTk) January 19, 2017
- [Alacritty Lands Scrollback, Publishes Benchmarks](https://jwilm.io/blog/alacritty-lands-scrollback/) September 17, 2018

## Installation

Tabor can be installed by using various package managers on Linux, BSD,
macOS and Windows.

Prebuilt binaries for macOS and Windows can also be downloaded from the
[Tabor GitHub releases page](https://github.com/tartavull/tabor/releases).

For everyone else, the detailed instructions to install Tabor can be found
[here](INSTALL.md).

### Requirements

- At least OpenGL ES 2.0
- [Windows] ConPTY support (Windows 10 version 1809 or higher)

## Configuration

You can find the documentation for Tabor's configuration in `man 5
tabor`, or by looking at [the manpage source] if you do not have the manpages
installed.

[the manpage source]: extra/man/tabor.5.scd

Tabor doesn't create the config file for you, but it looks for one in the
following locations:

1. `$XDG_CONFIG_HOME/tabor/tabor.toml`
2. `$XDG_CONFIG_HOME/tabor.toml`
3. `$HOME/.config/tabor/tabor.toml`
4. `$HOME/.tabor.toml`
5. `/etc/tabor/tabor.toml`

On Windows, the config file will be looked for in:

* `%APPDATA%\tabor\tabor.toml`

## Contributing

A guideline about contributing to Tabor can be found in the
[`CONTRIBUTING.md`](CONTRIBUTING.md) file.

## FAQ

**_Is it really the fastest terminal emulator?_**

Benchmarking terminal emulators is complicated. Tabor uses
[vtebench](https://github.com/alacritty/vtebench) to quantify terminal emulator
throughput and manages to consistently score better than the competition using
it. If you have found an example where this is not the case, please report a
bug.

Other aspects like latency or framerate and frame consistency are more difficult
to quantify. Some terminal emulators also intentionally slow down to save
resources, which might be preferred by some users.

If you have doubts about Tabor's performance or usability, the best way to
quantify terminal emulators is always to test them with **your** specific
usecases.

**_Why isn't feature X implemented?_**

Tabor has many great features, but not every feature from every other
terminal. This could be for a number of reasons, but sometimes it's just not a
good fit for Tabor. This means you won't find things like splits
(which are best left to a window manager or [terminal multiplexer][tmux]) nor
niceties like a GUI config editor.

[tmux]: https://github.com/tmux/tmux

## License

Tabor is released under the [Apache License, Version 2.0].

[Apache License, Version 2.0]: LICENSE-APACHE
