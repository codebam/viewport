# `crates/viewport-web` — In-Process WPE Engine & Web Trait

- **License**: MIT (kept separate from GPL `viewport` so DMA-BUF/engine abstractions stay MIT-reusable).
- **Role**: `WebEngine`, `WebHandler`, `Frame`, and `Plane` trait/types, `file_url` path encoder, plus the in-process WPE WebKit implementation behind `--features wpe`.

## Modules
- `lib.rs`: `Frame` (DMA-BUF planes + fourcc/modifier + optional `sync_file` fence `OwnedFd`), `WebHandler`, `WebEngine`, `file_url`, re-exported `BRIDGE_SHIM`. Always compiled with no WebKit dependency.
- `wpe.rs` (`#[cfg(feature = "wpe")]`): FFI wrapper around `shim/viewport-shim.c` (`WPEDisplay` + `WPEView` subclassing, `WPEBufferDMABuf` export, `FrameToken` generation tracking, `FrameDone`/`FrameRelease`).
- `webkit.rs` (`#[cfg(feature = "wpe")]`): `WebView` lifecycle, `UserContentManager` (`viewport` message handler + `SUBFRAME_GUARD`), crash termination sink, JS evaluation (`viewport_ipc::js::dispatch`).
- `build.rs`: probes `wpe-webkit-2.0` / `wpe-platform-2.0` via `pkg-config` and compiles `shim/viewport-shim.c` via `cc` **only** when `CARGO_FEATURE_WPE` is set.

## Build Note
- Default `cargo test --workspace` compiles `lib.rs` without `wpe`. Changes to `wpe.rs`, `webkit.rs`, or `shim/` require `cargo check -p viewport --features wpe` (and `.#wpe` / container).
