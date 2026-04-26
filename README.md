# Tempix — featherweight Windows HUD overlay

Tempix is a near-zero-overhead always-on-top desktop overlay for Windows
that shows CPU %, CPU °C, GPU %, GPU °C, RAM, and live network up/down
speeds. It's deliberately built without any UI framework — just raw
Win32, Direct2D and DirectWrite — so it stays well under the
**< 1 % CPU / < 30 MB RAM** budget defined by the spec.

## Build

```pwsh
# One-time: install the Rust toolchain (MSVC) if you don't have it.
# winget install Rustlang.Rustup
# rustup default stable-x86_64-pc-windows-msvc

cargo build --release
.\target\release\tempix.exe
```

The release binary is small (~1 MB after `strip`) and has no runtime
dependencies beyond shipped Windows DLLs (`d2d1.dll`, `dwrite.dll`,
`gdi32.dll`, `user32.dll`, `shell32.dll`, `advapi32.dll`).

## Tray menu

Right-click the tray icon for:

- **Toggle visibility** — hide / show the HUD without quitting.
- **Toggle auto-start** — adds/removes
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Tempix`.
- **Quit**.

Double-click the tray icon as a shortcut for *Toggle visibility*.

## Window behaviour

| Property              | How it's done                                              |
| --------------------- | ---------------------------------------------------------- |
| Always on top         | `WS_EX_TOPMOST`                                            |
| Per-pixel transparent | `WS_EX_LAYERED` + `UpdateLayeredWindow` w/ `AC_SRC_ALPHA`  |
| Click-through         | `WS_EX_TRANSPARENT` (requires `WS_EX_LAYERED`)             |
| Frameless             | `WS_POPUP`, no decoration                                  |
| Hidden from taskbar / Alt-Tab | `WS_EX_TOOLWINDOW`                                 |
| Fixed top-right       | `SystemParametersInfo(SPI_GETWORKAREA)` once at startup    |
| No background         | DIB cleared to `(0,0,0,0)` each frame                      |

## How resource usage is minimised

**No frameworks.** No winit, no egui, no Tauri, no Electron. The whole
app is the `windows` crate's raw bindings around Win32 + Direct2D +
DirectWrite. Cold start RSS is typically 8–15 MB.

**No paint loop.** There is no `WM_PAINT` redraw cycle and no DXGI swap
chain present loop. The window is layered, so pixels are pushed exactly
when we call `UpdateLayeredWindow` — never automatically.

**Render only on change.** Every tick the formatted lines are written
into stack-reused `String`s and compared against a cache. If *no* line
text changed (e.g. CPU rounded to the same 0.1 %), `UpdateLayeredWindow`
isn't called at all. On an idle desktop this means most ticks do zero
GPU/GDI work.

**One timer, low frequency.** A single `WM_TIMER` (1 Hz, configurable
via `TICK_MS`) drives stat collection. No threads, no async runtime, no
busy waits. Between ticks the process is parked in `GetMessageW` with
the kernel waking it on demand.

**Zero-allocation hot path.** After warm-up:

- `Stats` reuses its `System`, `Networks`, `Components`, NVML handle.
- `App` keeps two `[String; 5]` buffers and only `clear()` + `write!`s
  into them — no fresh `String` allocations.
- Renderer reuses one `Vec<u16>` UTF-16 scratch buffer for DirectWrite.
- All D2D resources (factory, DC render target, brushes, text format)
  and the DIB section are created once and reused for the process'
  lifetime.

**Cheap polling.** `sysinfo` is configured with the narrowest
`RefreshKind` possible (CPU usage + RAM + network + components only).
Disks and processes — by far the most expensive things to refresh —
are never touched. NVML is initialised lazily and degrades gracefully
to "n/a" on AMD/Intel systems.

**Compiled for size.** `Cargo.toml` sets `opt-level = "z"`,
`lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip =
"symbols"`. `sysinfo` is pulled in with `default-features = false` and
only the features actually used are enabled, keeping the binary small
and the dependency graph short.

**No console window.** `#![windows_subsystem = "windows"]` (release
only), so launching it from Explorer / auto-start doesn't flash a
console.

## Project layout

```
Cargo.toml      minimal deps + size-optimised release profile
build.rs        embeds a DPI-aware Win32 manifest
src/
  main.rs       window class, message loop, tray, auto-start
  render.rs     D2D + DirectWrite layered-window renderer
  stats.rs     sysinfo + nvml-wrapper polling
```

## Limitations

- **CPU temperature on Windows is best-effort.** Most consumer
  motherboards don't expose temps via the standard `Components` API
  without a vendor driver (HWiNFO / LibreHardwareMonitor). When no
  temperature is readable, the HUD just hides the °C suffix.
- **GPU stats are NVIDIA-only** (NVML). AMD and Intel are intentionally
  out of scope to keep the binary tiny; both vendors' SDKs are heavy.
- Single primary monitor only — the HUD anchors to
  `SPI_GETWORKAREA`. Multi-monitor anchoring would be a few extra lines
  via `MonitorFromPoint`.
