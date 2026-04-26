//! Tempix — ultra-light Windows HUD overlay.
//!
//! - Always-on-top, layered, click-through, frameless, hidden from taskbar.
//! - Renders with Direct2D + DirectWrite to a DIB section, then blits via
//!   `UpdateLayeredWindow`. No swap chain, no DXGI present loop.
//! - One `WM_TIMER` per second; renderer is only invoked when at least
//!   one displayed value changes.
//! - Tray icon with Toggle / Quit menu. Optional auto-start.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod render;
mod stats;

use std::cell::RefCell;
use std::mem::size_of;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use render::{HudView, Renderer, HUD_H, HUD_W};
use stats::Stats;

const WM_TRAY: u32 = WM_USER + 1;
const TIMER_ID: usize = 1;
const TICK_MS: u32 = 2000;

const ID_TOGGLE: usize = 100;
const ID_AUTOSTART: usize = 101;
const ID_QUIT: usize = 102;

struct App {
    renderer: Renderer,
    stats: Stats,
    pos: POINT,
    visible: bool,
    /// Last rendered snapshot. Rendering only happens when this changes.
    cached: HudView,
    /// Tick counter used to periodically trim the working set.
    ticks: u32,
}

impl App {
    fn new(hwnd: HWND) -> Result<Self> {
        let renderer = Renderer::new(hwnd)?;
        let stats = Stats::new();
        let pos = top_left_pos();
        Ok(Self {
            renderer,
            stats,
            pos,
            visible: true,
            cached: HudView::default(),
            ticks: 0,
        })
    }

    /// Refresh stats; render only if any displayed value changed.
    fn tick(&mut self) {
        self.stats.refresh();
        let view = self.build_view();

        if view != self.cached {
            self.cached = view;
            if self.visible {
                let _ = self.renderer.draw(&self.cached, self.pos);
            }
        }

        // Every ~60 seconds (TICK_MS=2000 → 30 ticks) ask the OS to release
        // unreferenced pages back to the system. This keeps idle working
        // set close to the real working footprint instead of growing with
        // transient allocations.
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks % 30 == 0 {
            unsafe {
                let _ = EmptyWorkingSet(GetCurrentProcess());
            }
        }
    }

    fn build_view(&self) -> HudView {
        // RAM in tenths of a GB (1 GB = 1024 MB).
        let ram_used_gb_x10 = ((self.stats.mem_used_mb * 10) / 1024) as u16;
        let ram_total_gb_x10 = ((self.stats.mem_total_mb * 10) / 1024) as u16;

        HudView {
            cpu_pct: self.stats.cpu_pct.round().clamp(0.0, 999.0) as u16,
            cpu_temp_c: self
                .stats
                .cpu_temp_c
                .map(|t| t.round().clamp(0.0, 999.0) as u16),
            gpu_pct: self.stats.gpu_pct.map(|v| v.min(999) as u16),
            gpu_temp_c: self.stats.gpu_temp_c.map(|v| v.min(999) as u16),
            ram_used_gb_x10,
            ram_total_gb_x10,
            net_down: rate_unit(self.stats.net_down_bps),
            net_up: rate_unit(self.stats.net_up_bps),
        }
    }

    fn repaint(&mut self) {
        let _ = self.renderer.draw(&self.cached, self.pos);
    }
}

/// Convert a byte/sec rate into (value*100, unit) where unit: 0=B, 1=KB, 2=MB.
fn rate_unit(bps: u64) -> (u32, u8) {
    if bps >= 1_048_576 {
        // MB/s with two decimal digits of resolution (we render one).
        let v = (bps as u128 * 100 / 1_048_576) as u32;
        (v.min(999_999), 2)
    } else if bps >= 1024 {
        let v = (bps as u128 * 100 / 1024) as u32;
        (v.min(999_999), 1)
    } else {
        ((bps as u32).min(999_999) * 100, 0)
    }
}
// Calculate the top-left position for the HUD based on the work area (screen minus taskbar) and a margin.
fn top_left_pos() -> POINT {
    unsafe {
        let mut wa = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut wa as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let margin = 8;
        POINT {
            x: wa.left + margin,
            y: wa.top + margin,
        }
    }
}

// ---- Win32 plumbing -------------------------------------------------------

thread_local! {
    static APP: RefCell<Option<Box<App>>> = RefCell::new(None);
}

fn main() -> Result<()> {
    unsafe {
        // Per-monitor DPI awareness so positioning is correct on hi-DPI.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let hinst: HINSTANCE = GetModuleHandleW(None)?.into();

        // Register window class.
        let class_name: Vec<u16> = "TempixHud\0".encode_utf16().collect();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            return Err(Error::from_win32());
        }

        // Layered + click-through + tool window (no taskbar / alt-tab).
        let ex_style = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW;
        let style = WS_POPUP;

        let hwnd = CreateWindowExW(
            ex_style,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(class_name.as_ptr()),
            style,
            0,
            0,
            HUD_W,
            HUD_H,
            None,
            None,
            hinst,
            None,
        )?;

        // Build app state. Renderer captures hwnd.
        let app = Box::new(App::new(hwnd)?);
        let raw = Box::into_raw(app);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);

        // Initial draw + show.
        if let Some(a) = (raw as *mut App).as_mut() {
            a.tick();
            // Force at least one paint even if everything is "0".
            a.repaint();
        }
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        // Tick timer.
        SetTimer(hwnd, TIMER_ID, TICK_MS, None);

        // Tray icon.
        install_tray(hwnd);
        migrate_legacy_autostart();

        // Trim startup working set: a lot of one-shot allocations from
        // initialization (Direct2D/DirectWrite/NVML/sysinfo) are no longer
        // needed in RAM. Let the OS reclaim them.
        let _ = EmptyWorkingSet(GetCurrentProcess());

        // Message loop.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Cleanup.
        remove_tray(hwnd);
        let raw = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut App;
        if !raw.is_null() {
            drop(Box::from_raw(raw));
        }
        Ok(())
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER if wp.0 == TIMER_ID => {
            with_app(hwnd, |a| a.tick());
            LRESULT(0)
        }
        WM_TRAY => {
            // lp low word = mouse event.
            let evt = (lp.0 & 0xFFFF) as u32;
            if evt == WM_RBUTTONUP || evt == WM_CONTEXTMENU {
                show_tray_menu(hwnd);
            } else if evt == WM_LBUTTONDBLCLK {
                toggle_visible(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wp.0 & 0xFFFF) as usize;
            match id {
                ID_TOGGLE => toggle_visible(hwnd),
                ID_QUIT => PostQuitMessage(0),
                ID_AUTOSTART => {
                    let _ = toggle_autostart();
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(hwnd, TIMER_ID);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn with_app<F: FnOnce(&mut App)>(hwnd: HWND, f: F) {
    unsafe {
        let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
        if let Some(a) = raw.as_mut() {
            f(a);
        }
    }
}

fn toggle_visible(hwnd: HWND) {
    with_app(hwnd, |a| {
        a.visible = !a.visible;
        unsafe {
            let _ = ShowWindow(
                hwnd,
                if a.visible {
                    SW_SHOWNOACTIVATE
                } else {
                    SW_HIDE
                },
            );
        }
        if a.visible {
            a.repaint();
        }
    });
}

// ---- Tray -----------------------------------------------------------------

fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    nid
}

fn install_tray(hwnd: HWND) {
    unsafe {
        let icon = load_tray_icon();
        let mut data = nid(hwnd);
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = icon;

        let tip: Vec<u16> = "Tempix HUD\0".encode_utf16().collect();
        let copy_len = tip.len().min(data.szTip.len());
        data.szTip[..copy_len].copy_from_slice(&tip[..copy_len]);

        let _ = Shell_NotifyIconW(NIM_ADD, &data);
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
    }
}

fn load_tray_icon() -> HICON {
    unsafe {
        if let Ok(exe) = std::env::current_exe() {
            let icon_path = exe.with_file_name("tempix.ico");
            let icon_path_w: Vec<u16> = icon_path
                .to_string_lossy()
                .encode_utf16()
                .chain([0])
                .collect();
            let h = LoadImageW(
                None,
                PCWSTR(icon_path_w.as_ptr()),
                IMAGE_ICON,
                0,
                0,
                LR_LOADFROMFILE | LR_DEFAULTSIZE,
            );
            if let Ok(h) = h {
                if !h.is_invalid() {
                    return HICON(h.0);
                }
            }
        }
        LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
    }
}

fn remove_tray(hwnd: HWND) {
    unsafe {
        let data = nid(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

fn show_tray_menu(hwnd: HWND) {
    unsafe {
        let menu = match CreatePopupMenu() {
            Ok(m) => m,
            Err(_) => return,
        };
        let toggle: Vec<u16> = "Toggle visibility\0".encode_utf16().collect();
        let auto: Vec<u16> = "Toggle auto-start\0".encode_utf16().collect();
        let quit: Vec<u16> = "Quit\0".encode_utf16().collect();

        let auto_flags = if is_autostart_enabled() {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };

        let _ = AppendMenuW(menu, MF_STRING, ID_TOGGLE, PCWSTR(toggle.as_ptr()));
        let _ = AppendMenuW(menu, auto_flags, ID_AUTOSTART, PCWSTR(auto.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, PCWSTR(quit.as_ptr()));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // SetForegroundWindow required for popup to dismiss correctly.
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
    }
}

// ---- Auto-start (Task Scheduler) -----------------------------------------

const TASK_NAME: &str = "Tempix";
const LEGACY_RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const LEGACY_RUN_VALUE: &str = "Tempix";
const CREATE_NO_WINDOW: u32 = 0x08000000;

fn hidden_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn schtasks() -> Command {
    hidden_command("schtasks.exe")
}

fn reg() -> Command {
    hidden_command("reg.exe")
}

fn run_schtasks(args: &[&str]) -> Result<()> {
    let status = schtasks()
        .args(args)
        .status()
        .map_err(|_| Error::from(E_FAIL))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::from(E_FAIL))
    }
}

fn is_autostart_enabled() -> bool {
    schtasks()
        .args(["/Query", "/TN", TASK_NAME])
        .status()
        .is_ok_and(|status| status.success())
}

fn create_autostart_task() -> Result<()> {
    let exe = std::env::current_exe().map_err(|_| Error::from(E_FAIL))?;
    let task_cmd = format!("\"{}\"", exe.to_string_lossy());
    let status = schtasks()
        .args([
            "/Create", "/TN", TASK_NAME, "/SC", "ONLOGON", "/RL", "HIGHEST", "/F", "/TR",
        ])
        .arg(task_cmd)
        .status()
        .map_err(|_| Error::from(E_FAIL))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::from(E_FAIL))
    }
}

fn is_legacy_autostart_enabled() -> bool {
    reg()
        .args(["query", LEGACY_RUN_KEY, "/v", LEGACY_RUN_VALUE])
        .status()
        .is_ok_and(|status| status.success())
}

fn delete_legacy_autostart() {
    let _ = reg()
        .args(["delete", LEGACY_RUN_KEY, "/v", LEGACY_RUN_VALUE, "/f"])
        .status();
}

fn migrate_legacy_autostart() {
    if is_legacy_autostart_enabled() {
        if !is_autostart_enabled() && create_autostart_task().is_err() {
            return;
        }
        delete_legacy_autostart();
    }
}

fn toggle_autostart() -> Result<()> {
    if is_autostart_enabled() {
        run_schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])?;
        delete_legacy_autostart();
        Ok(())
    } else {
        create_autostart_task()?;
        delete_legacy_autostart();
        Ok(())
    }
}
