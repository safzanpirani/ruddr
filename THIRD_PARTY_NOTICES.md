# Third-party notices

The `ruddr` binary is built from this repository's Rust workspace. It links
the Rust crates listed below and embeds a prebuilt browser client for
`ruddr web`. Ruddr itself is MIT licensed; see `LICENSE`.

## Rust crates

The workspace declares these direct dependencies in the root `Cargo.toml`.
Versions are the ones `Cargo.lock` resolves.

| Crate | Version | License | Used by |
|---|---|---|---|
| serde, serde_json | 1.0.229, 1.0.151 | MIT OR Apache-2.0 | every crate |
| libc | 0.2.189 | MIT OR Apache-2.0 | Unix process, signal, and file handling |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 | Windows process and console handling |
| interprocess | 2.4.4 | 0BSD OR Apache-2.0 | the control channel (Unix socket, Windows named pipe) |
| getrandom | 0.4.3 | MIT OR Apache-2.0 | run IDs and the web access token |
| sha2 | 0.11.0 | MIT OR Apache-2.0 | registry keys and release checksums |
| ureq | 3.4.2 | MIT OR Apache-2.0 | release checks and `ruddr update` downloads |
| ratatui | 0.30.2 | MIT | `ruddr tui` |
| crossterm | 0.29.0 | MIT | `ruddr tui` |
| unicode-width | 0.2.0 | MIT OR Apache-2.0 | `ruddr tui` |
| axum | 0.8.9 | MIT | `ruddr web` |
| tokio, tokio-stream | 1.53.1, 0.1.19 | MIT | `ruddr web` |
| clap | 4.6.7 | MIT OR Apache-2.0 | declared by `ruddr-cli` |

The full resolved graph for the five release targets (macOS arm64 and x64,
Linux arm64 and x64 with musl, Windows x64) follows. It includes crates used
only at compile time, such as procedural macros (`serde_derive`,
`clap_derive`, `tokio-macros`, and similar) and their parsers (`syn`,
`quote`, `proc-macro2`); the binary does not contain their code. Each
crate's license text and copyright notice ship in its source package on
crates.io.

Two entries need a note. `ring` (Apache-2.0 AND ISC) includes code derived
from BoringSSL. `webpki-roots` (CDLA-Permissive-2.0) carries Mozilla's root
certificate data, which `ureq` uses to verify GitHub's TLS certificate.

| Crate | Version | License |
|---|---|---|
| adler2 | 2.0.1 | 0BSD OR MIT OR Apache-2.0 |
| allocator-api2 | 0.2.21 | MIT OR Apache-2.0 |
| anstream | 1.0.0 | MIT OR Apache-2.0 |
| anstyle | 1.0.14 | MIT OR Apache-2.0 |
| anstyle-parse | 1.0.0 | MIT OR Apache-2.0 |
| anstyle-query | 1.1.5 | MIT OR Apache-2.0 |
| anstyle-wincon | 3.0.11 | MIT OR Apache-2.0 |
| atomic-waker | 1.1.2 | Apache-2.0 OR MIT |
| axum | 0.8.9 | MIT |
| axum-core | 0.5.6 | MIT |
| base64 | 0.23.1 | MIT OR Apache-2.0 |
| bitflags | 2.13.2 | MIT OR Apache-2.0 |
| block-buffer | 0.12.1 | MIT OR Apache-2.0 |
| bytes | 1.12.1 | MIT |
| castaway | 0.2.4 | MIT |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 |
| clap | 4.6.7 | MIT OR Apache-2.0 |
| clap_builder | 4.6.7 | MIT OR Apache-2.0 |
| clap_derive | 4.6.7 | MIT OR Apache-2.0 |
| clap_lex | 1.1.1 | MIT OR Apache-2.0 |
| colorchoice | 1.0.5 | MIT OR Apache-2.0 |
| compact_str | 0.9.1 | MIT |
| const-oid | 0.10.2 | Apache-2.0 OR MIT |
| convert_case | 0.10.0 | MIT |
| cpufeatures | 0.3.1 | MIT OR Apache-2.0 |
| crc32fast | 1.5.2 | MIT OR Apache-2.0 |
| critical-section | 1.2.0 | MIT OR Apache-2.0 |
| crossterm | 0.29.0 | MIT |
| crossterm_winapi | 0.9.1 | MIT |
| crypto-common | 0.2.2 | MIT OR Apache-2.0 |
| darling | 0.24.1 | MIT |
| darling_core | 0.24.1 | MIT |
| darling_macro | 0.24.1 | MIT |
| deranged | 0.5.8 | MIT OR Apache-2.0 |
| derive_more | 2.1.1 | MIT |
| derive_more-impl | 2.1.1 | MIT |
| digest | 0.11.3 | MIT OR Apache-2.0 |
| doctest-file | 1.1.1 | 0BSD |
| document-features | 0.2.12 | MIT OR Apache-2.0 |
| either | 1.18.0 | MIT OR Apache-2.0 |
| equivalent | 1.0.2 | Apache-2.0 OR MIT |
| errno | 0.3.14 | MIT OR Apache-2.0 |
| flate2 | 1.1.10 | MIT OR Apache-2.0 |
| foldhash | 0.2.0 | Zlib |
| futures-channel | 0.3.34 | MIT OR Apache-2.0 |
| futures-core | 0.3.34 | MIT OR Apache-2.0 |
| futures-task | 0.3.34 | MIT OR Apache-2.0 |
| futures-util | 0.3.34 | MIT OR Apache-2.0 |
| getrandom | 0.2.17 | MIT OR Apache-2.0 |
| getrandom | 0.4.3 | MIT OR Apache-2.0 |
| hashbrown | 0.16.1 | MIT OR Apache-2.0 |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 |
| heck | 0.5.0 | MIT OR Apache-2.0 |
| http | 1.5.0 | MIT OR Apache-2.0 |
| http-body | 1.1.0 | MIT |
| http-body-util | 0.1.5 | MIT |
| httparse | 1.10.1 | MIT OR Apache-2.0 |
| httpdate | 1.0.3 | MIT OR Apache-2.0 |
| hybrid-array | 0.4.15 | MIT OR Apache-2.0 |
| hyper | 1.11.1 | MIT |
| hyper-util | 0.1.21 | MIT |
| ident_case | 1.0.1 | MIT/Apache-2.0 |
| indoc | 2.0.7 | MIT OR Apache-2.0 |
| instability | 0.3.14 | MIT |
| interprocess | 2.4.4 | 0BSD OR Apache-2.0 |
| is_terminal_polyfill | 1.70.2 | MIT OR Apache-2.0 |
| itertools | 0.14.0 | MIT OR Apache-2.0 |
| itoa | 1.0.18 | MIT OR Apache-2.0 |
| kasuari | 0.4.12 | MIT OR Apache-2.0 |
| libc | 0.2.189 | MIT OR Apache-2.0 |
| line-clipping | 0.3.8 | MIT OR Apache-2.0 |
| linux-raw-sys | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| litrs | 1.0.0 | MIT OR Apache-2.0 |
| lock_api | 0.4.14 | MIT OR Apache-2.0 |
| log | 0.4.34 | MIT OR Apache-2.0 |
| lru | 0.18.5 | MIT |
| matchit | 0.8.4 | MIT AND BSD-3-Clause |
| memchr | 2.8.3 | Unlicense OR MIT |
| mime | 0.3.17 | MIT OR Apache-2.0 |
| miniz_oxide | 0.9.1 | MIT OR Zlib OR Apache-2.0 |
| mio | 1.2.3 | MIT |
| num-conv | 0.2.2 | MIT OR Apache-2.0 |
| num_threads | 0.1.7 | MIT OR Apache-2.0 |
| once_cell | 1.21.4 | MIT OR Apache-2.0 |
| once_cell_polyfill | 1.70.2 | MIT OR Apache-2.0 |
| parking_lot | 0.12.5 | MIT OR Apache-2.0 |
| parking_lot_core | 0.9.12 | MIT OR Apache-2.0 |
| percent-encoding | 2.3.2 | MIT OR Apache-2.0 |
| pin-project-lite | 0.2.17 | Apache-2.0 OR MIT |
| powerfmt | 0.2.0 | MIT OR Apache-2.0 |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 |
| quote | 1.0.47 | MIT OR Apache-2.0 |
| ratatui | 0.30.2 | MIT |
| ratatui-core | 0.1.2 | MIT |
| ratatui-crossterm | 0.1.2 | MIT |
| ratatui-macros | 0.7.2 | MIT |
| ratatui-widgets | 0.3.2 | MIT |
| recvmsg | 1.0.0 | 0BSD |
| ring | 0.17.14 | Apache-2.0 AND ISC |
| rustix | 1.1.5 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| rustls | 0.23.45 | Apache-2.0 OR ISC OR MIT |
| rustls-pki-types | 1.15.1 | MIT OR Apache-2.0 |
| rustls-webpki | 0.103.15 | ISC |
| rustversion | 1.0.23 | MIT OR Apache-2.0 |
| ryu | 1.0.23 | Apache-2.0 OR BSL-1.0 |
| scopeguard | 1.2.0 | MIT OR Apache-2.0 |
| serde | 1.0.229 | MIT OR Apache-2.0 |
| serde_core | 1.0.229 | MIT OR Apache-2.0 |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 |
| serde_json | 1.0.151 | MIT OR Apache-2.0 |
| sha2 | 0.11.0 | MIT OR Apache-2.0 |
| signal-hook | 0.3.18 | Apache-2.0/MIT |
| signal-hook-mio | 0.2.5 | MIT OR Apache-2.0 |
| signal-hook-registry | 1.4.8 | MIT OR Apache-2.0 |
| simd-adler32 | 0.3.10 | MIT |
| slab | 0.4.12 | MIT |
| smallvec | 1.16.2 | MIT OR Apache-2.0 |
| socket2 | 0.6.5 | MIT OR Apache-2.0 |
| static_assertions | 1.1.0 | MIT OR Apache-2.0 |
| strsim | 0.11.1 | MIT |
| strum | 0.28.0 | MIT |
| strum_macros | 0.28.0 | MIT |
| subtle | 2.6.1 | BSD-3-Clause |
| syn | 2.0.119 | MIT OR Apache-2.0 |
| syn | 3.0.6 | MIT OR Apache-2.0 |
| sync_wrapper | 1.0.2 | Apache-2.0 |
| thiserror | 2.0.21 | MIT OR Apache-2.0 |
| thiserror-impl | 2.0.21 | MIT OR Apache-2.0 |
| time | 0.3.55 | MIT OR Apache-2.0 |
| time-core | 0.1.9 | MIT OR Apache-2.0 |
| tokio | 1.53.1 | MIT |
| tokio-macros | 2.7.2 | MIT |
| tokio-stream | 0.1.19 | MIT |
| tower | 0.5.3 | MIT |
| tower-layer | 0.3.3 | MIT |
| tower-service | 0.3.3 | MIT |
| typenum | 1.20.1 | MIT OR Apache-2.0 |
| unicode-ident | 1.0.26 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| unicode-segmentation | 1.13.3 | MIT OR Apache-2.0 |
| unicode-truncate | 2.0.1 | MIT OR Apache-2.0 |
| unicode-width | 0.2.0 | MIT OR Apache-2.0 |
| untrusted | 0.9.0 | ISC |
| ureq | 3.4.2 | MIT OR Apache-2.0 |
| ureq-proto | 0.6.4 | MIT OR Apache-2.0 |
| utf8-zero | 0.8.1 | MIT OR Apache-2.0 |
| utf8parse | 0.2.2 | Apache-2.0 OR MIT |
| webpki-roots | 1.0.9 | CDLA-Permissive-2.0 |
| widestring | 1.2.1 | MIT OR Apache-2.0 |
| winapi | 0.3.9 | MIT/Apache-2.0 |
| windows-link | 0.2.1 | MIT OR Apache-2.0 |
| windows-sys | 0.61.2 | MIT OR Apache-2.0 |
| zeroize | 1.9.0 | Apache-2.0 OR MIT |
| zmij | 1.0.23 | MIT |

## Browser client

`ruddr web` embeds a browser client bundled from `web/client` by
`scripts/build-web.ts`. The bundle draws on these npm packages: the two
`@pierre` packages the client imports and their runtime dependencies, as
`bun.lock` resolves them. The bundler leaves out code the client never
reaches.

| Package | Version | License |
|---|---|---|
| @pierre/diffs | 1.5.1 | Apache-2.0 |
| @pierre/trees | 1.0.0-beta.6 | Apache-2.0 |
| @pierre/theme | 2.0.0 | Apache-2.0 |
| @pierre/theming | 1.0.0, 1.0.1 | Apache-2.0 |
| shiki and @shikijs/* | 4.5.0 | MIT |
| @shikijs/vscode-textmate | 10.0.2 | MIT |
| oniguruma-to-es | 4.3.6 | MIT |
| regex, regex-recursion | 6.1.0, 6.0.2 | MIT |
| diff | 9.0.0 | BSD-3-Clause |
| hast-util-to-html | 9.0.5 | MIT |
| lru_map | 0.4.1 | MIT |
| preact | 11.0.0-beta.0 | MIT |
| preact-render-to-string | 6.6.5 | MIT |

`@pierre/diffs` and `@pierre/trees` are by The Pierre Computer Company,
under the Apache License 2.0: https://www.apache.org/licenses/LICENSE-2.0.

`@pierre/diffs` highlights code with Shiki. The bundle includes Shiki's
TextMate language grammars and color themes as separate chunks under
`crates/ruddr-web/assets/client/chunks`. Shiki takes them from the
tm-grammars and tm-themes collections, and each grammar and theme keeps the
license of its upstream project, which those collections record.

## Claude Code protocol

The Claude adapter speaks the `claude` CLI's stream-json protocol the way
`@anthropic-ai/claude-agent-sdk` 0.3.245 does. Ruddr does not include or
link the SDK. Use of Claude Code is subject to Anthropic's legal agreements:
https://code.claude.com/docs/en/legal-and-compliance.

## OpenCode themes

The palettes in `crates/ruddr-core/src/themes.json` are derived from
OpenCode's built-in themes at commit
`b72b50006b24666da9f2088dbce907d6b24b6901`. The TUI and the web dashboard
share them.

MIT License

Copyright (c) 2025 opencode

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
