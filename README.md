# Zed

[![Zed:badge](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/kjanat/zed-editor/HEAD/assets/badge/v0.json)][zed]
[![CI:badge](https://github.com/kjanat/zed-editor/actions/workflows/fork_ci.yml/badge.svg)][CI:workflow]

Welcome to Zed, a high-performance, multiplayer code editor from the creators of
[Atom] and [Tree-sitter].

---

## Installation

On macOS, Linux, and Windows you can [download Zed directly][download] or
install Zed via your local package manager ([macOS]/[Linux]/[Windows]).

Other platforms are not yet available:

- Web ([tracking discussion])

## Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

## Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0
components where marked.

License information for third party dependencies must be correctly provided for
CI to pass.

We use [`cargo-about`] to automatically comply with open source licenses. If CI
is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If
  so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so,
  first determine what license the project has and whether this system is
  sufficient to comply with this license's requirements. If you're unsure, ask a
  lawyer. Once you've verified that this system is acceptable add the license's
  SPDX identifier to the `accepted` array in
  `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a
  clarification field at the end of `script/licenses/zed-licenses.toml`, as
  specified in the [cargo-about book].

## Sponsorship

THIS Zed is forked by [**@kjanat**][kjanat] from Zed Industries, Inc., and
developed by an individual.

If you’d like to financially support the fork, you can do so via GitHub
Sponsors. Sponsorships go directly to Kaj Kowalski and are used by him. There
are no perks or entitlements associated with sponsorship.

[kjanat]: https://github.com/kjanat
[zed]: https://zed.dev
[CI:workflow]: https://github.com/kjanat/zed-editor/actions/workflows/fork_ci.yml
[Atom]: https://github.com/atom/atom
[Tree-sitter]: https://github.com/tree-sitter/tree-sitter
[download]: https://zed.dev/download
[cargo-about book]: https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration
[`cargo-about`]: https://github.com/EmbarkStudios/cargo-about
[tracking discussion]: https://github.com/zed-industries/zed/discussions/26195
[macOS]: https://zed.dev/docs/installation#macos
[Linux]: https://zed.dev/docs/linux#installing-via-a-package-manager
[Windows]: https://zed.dev/docs/windows#package-managers
