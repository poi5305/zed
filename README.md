> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

## What this fork adds

Four dock panels, for working on a remote host the way you would work locally.

### Project Manager

Your projects in a list, opened in one click — local folders, and remote ones
behind `ssh://`, `wsl://` or `docker://`. Stored in `~/.config/zed/projects.json`
in the format of the VS Code "Project Manager" extension, so that extension's
file imports directly: paths are translated (`vscode-remote://ssh-remote+<host>`
becomes `ssh://<host>`, which is also how a Coder workspace is reached) and an
entry that cannot be translated is reported rather than failing the import.
Projects are grouped by tag, listed by name, and open in a window of their own.

### Forward Ports

The ports of the connection you are on, forwarded to this machine. A server
that starts on the remote host is noticed and offered, and accepting it records
the forward and opens the tunnel — including for a connection that has no
settings entry yet, which is written from the connection itself. Each forward
names its two ends, reports whether its tunnel is `Active` or `Failed`, and can
be connected, disconnected or deleted. Forwards the ssh transport carries
itself (`ssh -L`) are shown as `External` and left alone.

### Tmux Sessions

The tmux server's sessions and windows as a tree, local or remote, with a click
attaching the window in a terminal.

### Claude Sessions

The Claude Code sessions running on this project's host: what each is doing,
the context it is carrying and what it has cost. A session's conversation opens
as an editor tab — one tab per session — with its transcript rendered as
markdown, its sub-agents' conversations alongside it, its own tmux pane embedded
as a live terminal, and an input that replies to it. Liveness is the process
being alive, not a heartbeat, so a session sitting idle is still listed, with
how long it has been idle.

Everything below is Zed's own README.

# Zed

[![Zed](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/zed-industries/zed/main/assets/badge/v0.json)](https://zed.dev)
[![CI](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml/badge.svg)](https://github.com/zed-industries/zed/actions/workflows/run_tests.yml)

Welcome to Zed, a high-performance, multiplayer code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

---

### Installation

On macOS, Linux, and Windows you can [download Zed directly](https://zed.dev/download) or install Zed via your local package manager ([macOS](https://zed.dev/docs/installation#macos)/[Linux](https://zed.dev/docs/linux#installing-via-a-package-manager)/[Windows](https://zed.dev/docs/windows#package-managers)).

Other platforms are not yet available:

- Web ([tracking discussion](https://github.com/zed-industries/zed/discussions/26195))

### Developing Zed

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

### Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for ways you can contribute to Zed.

Also... we're hiring! Check out our [jobs](https://zed.dev/jobs) page for open roles.

### Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Sponsorship

Zed is developed by **Zed Industries, Inc.**, a for-profit company.

If you’d like to financially support the project, you can do so via GitHub Sponsors.
Sponsorships go directly to Zed Industries and are used as general company revenue.
There are no perks or entitlements associated with sponsorship.
