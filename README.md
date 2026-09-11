# microtun-telnet

`microtun-telnet` is an interactive Telnet client with YMODEM file upload capabilities.

## Build

A Rust 1.85 or newer toolchain is required.

```bash
cargo build --release
```

The resulting executable is `target/release/microtun-telnet` (or
`microtun-telnet.exe` on Windows).

For development:

```bash
cargo run -- 192.168.7.1
```

## Connect

Pass a device IP address or hostname:

```bash
microtun-telnet 192.168.7.1
microtun-telnet microtun-device.local
```

The TCP port defaults to Telnet port `23`. Override it with `--port`:

```bash
microtun-telnet 100.64.0.3 --port 2323
```

Use `--timeout <SECONDS>` to change the connection and YMODEM transfer timeout. The
client requires an interactive terminal; redirected stdin/stdout and a separate plain
text mode are intentionally not supported.

## Terminal UI

The remote Telnet session is rendered by `tui-term`/`vt100`, preserving ANSI colors,
cursor movement, screen clearing, and other terminal control sequences. Special keys
such as arrows, Home/End, Page Up/Down, Delete, and F1-F12 are translated to terminal
escape sequences and sent to the peer.

The layout follows Minicom: the remote terminal owns the screen and a status line sits
at the bottom. Local commands use Minicom's default `Ctrl-A` prefix:

```text
Ctrl-A Z   Command Summary / help
Ctrl-A S   Send file (YMODEM)
Ctrl-A C   Clear screen
Ctrl-A Q   Quit
Ctrl-A X   Exit
Ctrl-A Ctrl-A   Send a literal Ctrl-A to the remote peer
```

Choosing `S` opens an in-terminal file picker rooted at the current working directory.
Use the arrow keys to select an entry, `Enter`/Right to enter a directory or send the
selected file, Backspace/Left to move to the parent directory, Home/End and Page
Up/Down for faster navigation, `Ctrl-R` to refresh, and `Esc` to cancel. YMODEM progress
is shown in a temporary popup while the transfer runs.

## Binary releases

Pushing a `v<version>` tag that matches the version in `Cargo.toml` builds and publishes a GitHub Release containing:

- Linux amd64 (`x86_64-unknown-linux-musl`)
- Linux arm64 (`aarch64-unknown-linux-musl`)
- macOS amd64 (`x86_64-apple-darwin`)
- macOS arm64 (`aarch64-apple-darwin`)
- Windows amd64 (`x86_64-pc-windows-gnu`)

Linux and Windows are cross-compiled from an Ubuntu runner with
[`cross`](https://github.com/cross-rs/cross). macOS artifacts are both built on one
Apple Silicon GitHub runner using the macOS SDK; the amd64 build is cross-compiled.
Release archives include the executable, README, and both license files, plus the
release contains a `SHA256SUMS` file.

## License

Licensed under either Apache-2.0 or MIT at your option.
