//! Layered-window renderer using Direct2D + DirectWrite.
//!
//! The window is a top-most, click-through, layered window. Rendering
//! goes to an offscreen 32-bit BGRA DIB section via an `ID2D1DCRenderTarget`,
//! then `UpdateLayeredWindow` blits the result to screen with per-pixel alpha.
//! No `WM_PAINT` cycle, no DXGI swap chain, no DComposition.
//!
//! All long-lived resources (factories, brushes, text formats, DIB) are
//! created once and reused for the lifetime of the window. The render
//! path only allocates a small `IDWriteTextLayout` per text segment, and
//! is only invoked when at least one displayed value changes.

use std::ffi::c_void;
use std::ptr::null_mut;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const HUD_W: i32 = 240;
pub const HUD_H: i32 = 140;

/// Snapshot of metrics the HUD renders. Equality on this struct tells the
/// app whether any displayed value changed since the previous tick.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub struct HudView {
    /// CPU usage in whole percent (0..=100).
    pub cpu_pct: u16,
    pub cpu_temp_c: Option<u16>,
    pub gpu_pct: Option<u16>,
    pub gpu_temp_c: Option<u16>,
    /// RAM in tenths of a GB.
    pub ram_used_gb_x10: u16,
    pub ram_total_gb_x10: u16,
    /// Network rates: (value*100, unit) where unit is 0=B, 1=KB, 2=MB.
    pub net_down: (u32, u8),
    pub net_up: (u32, u8),
}

pub struct Renderer {
    hwnd: HWND,
    width: i32,
    height: i32,

    // GDI side.
    mem_dc: HDC,
    dib: HBITMAP,
    old_bmp: HGDIOBJ,

    // D2D / DWrite (kept alive for the lifetime of the window).
    _d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    target: ID2D1RenderTarget,
    palettes: Palettes,

    // Text formats.
    label_fmt: IDWriteTextFormat, // labels (CPU:, GPU:, RAM:, Net:)
    big_fmt: IDWriteTextFormat,   // bold numeric values
    small_fmt: IDWriteTextFormat, // °C, %, GB, MB/KB units

    // Reusable UTF-16 scratch.
    utf16: Vec<u16>,
}

impl Renderer {
    pub fn new(hwnd: HWND) -> Result<Self> {
        unsafe {
            // ---- GDI: memory DC + 32-bit top-down DIB ------------------
            let screen_dc = GetDC(None);
            let mem_dc = CreateCompatibleDC(screen_dc);

            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = HUD_W;
            bmi.bmiHeader.biHeight = -HUD_H; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;

            let mut bits: *mut c_void = null_mut();
            let dib = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let old_bmp = SelectObject(mem_dc, dib);
            ReleaseDC(None, screen_dc);

            // ---- D2D factory + DC render target ------------------------
            let d2d: ID2D1Factory =
                D2D1CreateFactory::<ID2D1Factory>(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;

            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let dc_target: ID2D1DCRenderTarget = d2d.CreateDCRenderTarget(&props)?;
            let target: ID2D1RenderTarget = dc_target.cast()?;

            let bind_rect = RECT {
                left: 0,
                top: 0,
                right: HUD_W,
                bottom: HUD_H,
            };
            dc_target.BindDC(mem_dc, &bind_rect)?;

            // Grayscale AA (ClearType is incompatible with per-pixel alpha).
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

            // ---- DirectWrite -------------------------------------------
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            let label_fmt = make_format(&dwrite, "Segoe UI", DWRITE_FONT_WEIGHT_SEMI_BOLD, 16.0)?;
            let big_fmt = make_format(&dwrite, "Segoe UI", DWRITE_FONT_WEIGHT_BOLD, 20.0)?;
            let small_fmt = make_format(&dwrite, "Segoe UI", DWRITE_FONT_WEIGHT_SEMI_BOLD, 12.0)?;
            let palettes = Palettes::new(&target)?;

            Ok(Self {
                hwnd,
                width: HUD_W,
                height: HUD_H,
                mem_dc,
                dib,
                old_bmp,
                _d2d: d2d,
                dwrite,
                target,
                palettes,
                label_fmt,
                big_fmt,
                small_fmt,
                utf16: Vec::with_capacity(64),
            })
        }
    }

    /// Render the supplied snapshot and push to the layered window.
    pub fn draw(&mut self, v: &HudView, pos: POINT) -> Result<()> {
        // Destructure once so individual borrows don't conflict.
        let Self {
            hwnd,
            width,
            height,
            mem_dc,
            dwrite,
            target,
            palettes,
            label_fmt,
            big_fmt,
            small_fmt,
            utf16,
            ..
        } = self;

        unsafe {
            let tone = sample_backdrop_tone(pos, *width, *height);
            let palette = palettes.for_tone(tone);

            target.BeginDraw();
            target.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));

            let pad_x = 12.0f32;
            let row_h = 32.0f32;
            let label_baseline_offset = 4.0f32;
            let unit_baseline_offset = 4.0f32;

            // Buffers reused for itoa-style formatting.
            let mut buf = [0u8; 16];

            // -------- Row 1: CPU --------
            let mut y = 10.0f32;
            let mut x = pad_x;
            x = draw_seg(
                target,
                dwrite,
                utf16,
                "CPU:",
                x,
                y + label_baseline_offset,
                label_fmt,
                &palette.label,
            );
            x += 6.0;
            x = draw_temp(
                target,
                dwrite,
                utf16,
                &mut buf,
                x,
                y,
                v.cpu_temp_c,
                big_fmt,
                small_fmt,
                &palette.temp,
                &palette.dim,
            );
            x += 12.0;
            let _ = draw_pct(
                target,
                dwrite,
                utf16,
                &mut buf,
                x,
                y,
                Some(v.cpu_pct),
                big_fmt,
                small_fmt,
                &palette.cpu_pct,
                &palette.dim,
            );
            draw_separator(
                target,
                &palette.separator,
                pad_x,
                y + row_h - 2.0,
                *width as f32,
            );

            // -------- Row 2: GPU --------
            y += row_h;
            let mut x = pad_x;
            x = draw_seg(
                target,
                dwrite,
                utf16,
                "GPU:",
                x,
                y + label_baseline_offset,
                label_fmt,
                &palette.label,
            );
            x += 6.0;
            x = draw_temp(
                target,
                dwrite,
                utf16,
                &mut buf,
                x,
                y,
                v.gpu_temp_c,
                big_fmt,
                small_fmt,
                &palette.temp,
                &palette.dim,
            );
            x += 12.0;
            let _ = draw_pct(
                target,
                dwrite,
                utf16,
                &mut buf,
                x,
                y,
                v.gpu_pct,
                big_fmt,
                small_fmt,
                &palette.gpu_pct,
                &palette.dim,
            );
            draw_separator(
                target,
                &palette.separator,
                pad_x,
                y + row_h - 2.0,
                *width as f32,
            );

            // -------- Row 3: RAM --------
            y += row_h;
            let mut x = pad_x;
            x = draw_seg(
                target,
                dwrite,
                utf16,
                "RAM:",
                x,
                y + label_baseline_offset,
                label_fmt,
                &palette.label,
            );
            x += 6.0;
            // used.x GB
            let used = fmt_x10(&mut buf, v.ram_used_gb_x10);
            x = draw_seg(
                target,
                dwrite,
                utf16,
                used,
                x,
                y,
                big_fmt,
                &palette.ram_value,
            );
            x = draw_seg(
                target,
                dwrite,
                utf16,
                " GB / ",
                x,
                y + unit_baseline_offset,
                small_fmt,
                &palette.dim,
            );
            let total = fmt_x10(&mut buf, v.ram_total_gb_x10);
            x = draw_seg(
                target,
                dwrite,
                utf16,
                total,
                x,
                y,
                big_fmt,
                &palette.ram_value,
            );
            let _ = draw_seg(
                target,
                dwrite,
                utf16,
                " GB",
                x,
                y + unit_baseline_offset,
                small_fmt,
                &palette.dim,
            );
            draw_separator(
                target,
                &palette.separator,
                pad_x,
                y + row_h - 2.0,
                *width as f32,
            );

            // -------- Row 4: Net --------
            y += row_h;
            let mut x = pad_x;
            x = draw_seg(
                target,
                dwrite,
                utf16,
                "Net:",
                x,
                y + label_baseline_offset,
                label_fmt,
                &palette.label,
            );
            x += 6.0;
            x = draw_rate(
                target,
                dwrite,
                utf16,
                &mut buf,
                x,
                y,
                v.net_down,
                big_fmt,
                small_fmt,
                &palette.net_down,
                &palette.dim,
            );
            x = draw_seg(
                target,
                dwrite,
                utf16,
                " / ",
                x,
                y + unit_baseline_offset,
                small_fmt,
                &palette.dim,
            );
            let _ = draw_rate(
                target,
                dwrite,
                utf16,
                &mut buf,
                x,
                y,
                v.net_up,
                big_fmt,
                small_fmt,
                &palette.net_up,
                &palette.dim,
            );

            target.EndDraw(None, None)?;
            let _ = GdiFlush();

            // ---- Push to screen ----------------------------------------
            let screen_dc = GetDC(None);
            let size = SIZE {
                cx: *width,
                cy: *height,
            };
            let src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let r = UpdateLayeredWindow(
                *hwnd,
                screen_dc,
                Some(&pos),
                Some(&size),
                *mem_dc,
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            ReleaseDC(None, screen_dc);
            r
        }
    }
}

// ----------------------------------------------------------------------
//  Free helpers (no &mut self conflict).
// ----------------------------------------------------------------------

#[derive(Clone, Copy)]
enum BackdropTone {
    Dark,
    Light,
}

struct Palettes {
    dark: Palette,
    light: Palette,
}

impl Palettes {
    fn new(target: &ID2D1RenderTarget) -> Result<Self> {
        Ok(Self {
            dark: Palette::new(target, BackdropTone::Dark)?,
            light: Palette::new(target, BackdropTone::Light)?,
        })
    }

    fn for_tone(&self, tone: BackdropTone) -> &Palette {
        match tone {
            BackdropTone::Dark => &self.dark,
            BackdropTone::Light => &self.light,
        }
    }
}

struct Palette {
    label: ID2D1SolidColorBrush,
    dim: ID2D1SolidColorBrush,
    separator: ID2D1SolidColorBrush,
    temp: ID2D1SolidColorBrush,
    cpu_pct: ID2D1SolidColorBrush,
    gpu_pct: ID2D1SolidColorBrush,
    ram_value: ID2D1SolidColorBrush,
    net_down: ID2D1SolidColorBrush,
    net_up: ID2D1SolidColorBrush,
}

impl Palette {
    fn new(target: &ID2D1RenderTarget, tone: BackdropTone) -> Result<Self> {
        let colors = PaletteColors::for_tone(tone);
        Ok(Self {
            label: solid(
                target,
                colors.label.0,
                colors.label.1,
                colors.label.2,
                colors.label.3,
            )?,
            dim: solid(
                target,
                colors.dim.0,
                colors.dim.1,
                colors.dim.2,
                colors.dim.3,
            )?,
            separator: solid(
                target,
                colors.separator.0,
                colors.separator.1,
                colors.separator.2,
                colors.separator.3,
            )?,
            temp: solid(
                target,
                colors.temp.0,
                colors.temp.1,
                colors.temp.2,
                colors.temp.3,
            )?,
            cpu_pct: solid(
                target,
                colors.cpu_pct.0,
                colors.cpu_pct.1,
                colors.cpu_pct.2,
                colors.cpu_pct.3,
            )?,
            gpu_pct: solid(
                target,
                colors.gpu_pct.0,
                colors.gpu_pct.1,
                colors.gpu_pct.2,
                colors.gpu_pct.3,
            )?,
            ram_value: solid(
                target,
                colors.ram_value.0,
                colors.ram_value.1,
                colors.ram_value.2,
                colors.ram_value.3,
            )?,
            net_down: solid(
                target,
                colors.net_down.0,
                colors.net_down.1,
                colors.net_down.2,
                colors.net_down.3,
            )?,
            net_up: solid(
                target,
                colors.net_up.0,
                colors.net_up.1,
                colors.net_up.2,
                colors.net_up.3,
            )?,
        })
    }
}

struct PaletteColors {
    label: (f32, f32, f32, f32),
    dim: (f32, f32, f32, f32),
    separator: (f32, f32, f32, f32),
    temp: (f32, f32, f32, f32),
    cpu_pct: (f32, f32, f32, f32),
    gpu_pct: (f32, f32, f32, f32),
    ram_value: (f32, f32, f32, f32),
    net_down: (f32, f32, f32, f32),
    net_up: (f32, f32, f32, f32),
}

impl PaletteColors {
    fn for_tone(tone: BackdropTone) -> Self {
        match tone {
            BackdropTone::Dark => Self {
                label: (0.92, 0.95, 1.00, 1.00),
                dim: (0.86, 0.89, 1.00, 0.88),
                separator: (1.00, 1.00, 1.00, 0.22),
                temp: (1.00, 0.68, 0.16, 1.00),
                cpu_pct: (0.45, 0.78, 1.00, 1.00),
                gpu_pct: (0.38, 0.90, 0.55, 1.00),
                ram_value: (0.92, 0.96, 1.00, 1.00),
                net_down: (0.49, 0.95, 0.30, 1.00),
                net_up: (0.35, 0.78, 1.00, 1.00),
            },
            BackdropTone::Light => Self {
                label: (0.05, 0.07, 0.10, 1.00),
                dim: (0.18, 0.22, 0.30, 0.88),
                separator: (0.02, 0.04, 0.08, 0.24),
                temp: (0.72, 0.38, 0.00, 1.00),
                cpu_pct: (0.00, 0.26, 0.68, 1.00),
                gpu_pct: (0.00, 0.42, 0.18, 1.00),
                ram_value: (0.05, 0.07, 0.10, 1.00),
                net_down: (0.00, 0.42, 0.18, 1.00),
                net_up: (0.00, 0.26, 0.68, 1.00),
            },
        }
    }
}

fn sample_backdrop_tone(pos: POINT, width: i32, height: i32) -> BackdropTone {
    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.0.is_null() {
            return BackdropTone::Dark;
        }

        let xs = [8, width / 2, width.saturating_sub(8)];
        let ys = [
            8,
            height / 4,
            height / 2,
            height * 3 / 4,
            height.saturating_sub(8),
        ];
        let mut luma_sum = 0.0f32;
        let mut samples = 0.0f32;

        for y in ys {
            for x in xs {
                let pixel = GetPixel(screen_dc, pos.x + x, pos.y + y);
                if pixel.0 == u32::MAX {
                    continue;
                }

                let raw = pixel.0;
                let r = (raw & 0xff) as f32 / 255.0;
                let g = ((raw >> 8) & 0xff) as f32 / 255.0;
                let b = ((raw >> 16) & 0xff) as f32 / 255.0;
                luma_sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
                samples += 1.0;
            }
        }

        ReleaseDC(None, screen_dc);
        if samples > 0.0 && luma_sum / samples > 0.58 {
            BackdropTone::Light
        } else {
            BackdropTone::Dark
        }
    }
}

fn draw_separator(
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    x: f32,
    y: f32,
    width: f32,
) {
    let r = D2D_RECT_F {
        left: x,
        top: y,
        right: width - x,
        bottom: y + 1.0,
    };
    unsafe { target.FillRectangle(&r, brush) };
}

/// Draws "<int>°C" with coloured number + small dim "°C" suffix.
#[allow(clippy::too_many_arguments)]
fn draw_temp(
    target: &ID2D1RenderTarget,
    dwrite: &IDWriteFactory,
    utf16: &mut Vec<u16>,
    buf: &mut [u8; 16],
    x: f32,
    y: f32,
    temp: Option<u16>,
    big_fmt: &IDWriteTextFormat,
    small_fmt: &IDWriteTextFormat,
    big_brush: &ID2D1SolidColorBrush,
    dim_brush: &ID2D1SolidColorBrush,
) -> f32 {
    match temp {
        Some(t) => {
            let n = fmt_u16(buf, t);
            let nx = draw_seg(target, dwrite, utf16, n, x, y, big_fmt, big_brush);
            draw_seg(
                target,
                dwrite,
                utf16,
                "°C",
                nx,
                y + 4.0,
                small_fmt,
                dim_brush,
            )
        }
        None => draw_seg(target, dwrite, utf16, "--", x, y, big_fmt, dim_brush),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_pct(
    target: &ID2D1RenderTarget,
    dwrite: &IDWriteFactory,
    utf16: &mut Vec<u16>,
    buf: &mut [u8; 16],
    x: f32,
    y: f32,
    val: Option<u16>,
    big_fmt: &IDWriteTextFormat,
    small_fmt: &IDWriteTextFormat,
    big_brush: &ID2D1SolidColorBrush,
    dim_brush: &ID2D1SolidColorBrush,
) -> f32 {
    match val {
        Some(v) => {
            let n = fmt_u16(buf, v);
            let nx = draw_seg(target, dwrite, utf16, n, x, y, big_fmt, big_brush);
            draw_seg(
                target,
                dwrite,
                utf16,
                "%",
                nx,
                y + 4.0,
                small_fmt,
                dim_brush,
            )
        }
        None => draw_seg(
            target,
            dwrite,
            utf16,
            "n/a",
            x,
            y + 4.0,
            small_fmt,
            dim_brush,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_rate(
    target: &ID2D1RenderTarget,
    dwrite: &IDWriteFactory,
    utf16: &mut Vec<u16>,
    buf: &mut [u8; 16],
    x: f32,
    y: f32,
    rate: (u32, u8),
    big_fmt: &IDWriteTextFormat,
    small_fmt: &IDWriteTextFormat,
    big_brush: &ID2D1SolidColorBrush,
    dim_brush: &ID2D1SolidColorBrush,
) -> f32 {
    let unit = match rate.1 {
        2 => " MB",
        1 => " KB",
        _ => " B",
    };
    let n = if rate.1 == 0 {
        fmt_u32(buf, rate.0 / 100)
    } else {
        fmt_decimal(buf, rate.0)
    };
    let nx = draw_seg(target, dwrite, utf16, n, x, y, big_fmt, big_brush);
    draw_seg(
        target,
        dwrite,
        utf16,
        unit,
        nx,
        y + 4.0,
        small_fmt,
        dim_brush,
    )
}

/// Draws a single string segment at (x, y) and returns x + measured width.
fn draw_seg(
    target: &ID2D1RenderTarget,
    dwrite: &IDWriteFactory,
    utf16: &mut Vec<u16>,
    s: &str,
    x: f32,
    y: f32,
    fmt: &IDWriteTextFormat,
    brush: &ID2D1SolidColorBrush,
) -> f32 {
    utf16.clear();
    utf16.extend(s.encode_utf16());
    unsafe {
        let layout = match dwrite.CreateTextLayout(utf16, fmt, 4096.0, 200.0) {
            Ok(l) => l,
            Err(_) => return x,
        };
        let mut metrics = DWRITE_TEXT_METRICS::default();
        let _ = layout.GetMetrics(&mut metrics);
        let origin = D2D_POINT_2F { x, y };
        target.DrawTextLayout(origin, &layout, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        x + metrics.widthIncludingTrailingWhitespace
    }
}

fn make_format(
    dwrite: &IDWriteFactory,
    family: &str,
    weight: DWRITE_FONT_WEIGHT,
    size: f32,
) -> Result<IDWriteTextFormat> {
    let family: Vec<u16> = family.encode_utf16().chain([0]).collect();
    let locale: Vec<u16> = "en-us\0".encode_utf16().collect();
    unsafe {
        let f = dwrite.CreateTextFormat(
            PCWSTR(family.as_ptr()),
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            PCWSTR(locale.as_ptr()),
        )?;
        f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
        Ok(f)
    }
}

fn solid(
    target: &ID2D1RenderTarget,
    r: f32,
    g: f32,
    b: f32,
    a: f32,
) -> Result<ID2D1SolidColorBrush> {
    // Premultiply for layered-window alpha correctness.
    let c = D2D1_COLOR_F {
        r: r * a,
        g: g * a,
        b: b * a,
        a,
    };
    unsafe { target.CreateSolidColorBrush(&c, None) }
}

// ----- tiny no-alloc number formatters --------------------------------

fn fmt_u16(buf: &mut [u8; 16], v: u16) -> &str {
    fmt_u32(buf, v as u32)
}

fn fmt_u32(buf: &mut [u8; 16], mut v: u32) -> &str {
    if v == 0 {
        buf[0] = b'0';
        return std::str::from_utf8(&buf[..1]).unwrap();
    }
    let mut tmp = [0u8; 16];
    let mut i = 0;
    while v > 0 {
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    let mut j = 0;
    while j < i {
        buf[j] = tmp[i - 1 - j];
        j += 1;
    }
    std::str::from_utf8(&buf[..i]).unwrap()
}

/// Format `value*100` as "W.F" where W is the whole and F a single-digit
/// fraction (rounded toward zero). e.g. 523 -> "5.2".
fn fmt_decimal(buf: &mut [u8; 16], v_x100: u32) -> &str {
    let whole = v_x100 / 100;
    let frac = (v_x100 % 100) / 10;
    let mut tmp = [0u8; 16];
    let s = fmt_u32(&mut tmp, whole);
    let n = s.len();
    buf[..n].copy_from_slice(s.as_bytes());
    buf[n] = b'.';
    buf[n + 1] = b'0' + frac as u8;
    std::str::from_utf8(&buf[..n + 2]).unwrap()
}

/// Format `value*10` as "W.F" (single fraction digit). e.g. 65 -> "6.5".
fn fmt_x10(buf: &mut [u8; 16], v_x10: u16) -> &str {
    let whole = (v_x10 / 10) as u32;
    let frac = v_x10 % 10;
    let mut tmp = [0u8; 16];
    let s = fmt_u32(&mut tmp, whole);
    let n = s.len();
    buf[..n].copy_from_slice(s.as_bytes());
    buf[n] = b'.';
    buf[n + 1] = b'0' + frac as u8;
    std::str::from_utf8(&buf[..n + 2]).unwrap()
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mem_dc, self.old_bmp);
            let _ = DeleteObject(self.dib);
            let _ = DeleteDC(self.mem_dc);
        }
    }
}
