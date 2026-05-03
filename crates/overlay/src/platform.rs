//! Platform-specific window setup for the GPU overlay path.
//!
//! ## Windows (offscreen path)
//! On Windows, DXGI swap chains don't support transparent compositing (only
//! `Opaque` alpha mode is available).  The GPU path therefore renders to an
//! offscreen texture, reads the pixels back to the CPU, and calls
//! `UpdateLayeredWindow` — the same WIN32 mechanism used by the GDI path.
//!
//! `setup_gpu_window` sets `WS_EX_LAYERED` (required for
//! `UpdateLayeredWindow`) and ensures the window is always-on-top.
//! `update_layered_window_from_pixels` takes a tightly-packed BGRA pixel slice
//! and calls `UpdateLayeredWindow` with pre-multiplied alpha blending.
//!
//! ## macOS
//! winit's `with_transparent(true)` calls `setOpaque(false)` internally; wgpu's
//! Metal backend handles per-pixel alpha.  No extra calls needed.
//!
//! ## Linux (Wayland / X11)
//! Wayland: winit handles transparency natively.
//! X11: winit selects an ARGB visual automatically (requires compositing WM).

// ─── Windows implementation ───────────────────────────────────────────────────

#[cfg(feature = "windows-glue")]
pub use windows_impl::*;

#[cfg(feature = "windows-glue")]
mod windows_impl {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::{BOOL, COLORREF, HANDLE, HWND, POINT, SIZE};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, HWND_TOPMOST,
        SWP_NOMOVE, SWP_NOSIZE, SetWindowPos, UpdateLayeredWindow, ULW_ALPHA, WS_EX_APPWINDOW,
        WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
        GetWindowThreadProcessId,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    pub fn get_hwnd(window: &winit::window::Window) -> Option<HWND> {
        if let Ok(handle) = window.window_handle() {
            if let RawWindowHandle::Win32(h) = handle.as_raw() {
                return Some(HWND(h.hwnd.get()));
            }
        }
        None
    }

    /// Ensure `WS_EX_LAYERED` is set so `UpdateLayeredWindow` works.
    /// Also pins the window to TOPMOST.
    pub fn setup_gpu_window(window: &winit::window::Window) {
        let Some(hwnd) = get_hwnd(window) else { return };
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED.0 as isize);
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        }
    }

    /// Set `WS_EX_TOOLWINDOW` and clear `WS_EX_APPWINDOW` so the overlay window
    /// does not appear in the taskbar or Alt-Tab switcher.
    /// Call after `setup_gpu_window`.
    pub fn hide_from_taskbar(window: &winit::window::Window) {
        let Some(hwnd) = get_hwnd(window) else { return };
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let new_ex = (ex | WS_EX_TOOLWINDOW.0 as isize)
                       & !(WS_EX_APPWINDOW.0 as isize);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
        }
    }

    /// Toggle click-through (`WS_EX_TRANSPARENT`) without touching `WS_EX_LAYERED`.
    pub fn set_click_through(window: &winit::window::Window, click_through: bool) {
        let Some(hwnd) = get_hwnd(window) else { return };
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let new_ex = if click_through {
                ex | WS_EX_LAYERED.0 as isize | WS_EX_TRANSPARENT.0 as isize
            } else {
                ex & !(WS_EX_TRANSPARENT.0 as isize)
            };
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        }
    }

    /// Returns `true` if the current foreground window belongs to `aces.exe`
    /// (the War Thunder process).
    pub fn is_warthunder_foreground() -> bool {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.0 == 0 { return false; }

            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 { return false; }

            let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                Ok(h) => h,
                Err(_) => return false,
            };

            let mut buf = [0u16; 260];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle, PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()), &mut len,
            );
            let _ = windows::Win32::Foundation::CloseHandle(handle);
            if !ok.as_bool() { return false; }

            let path: String = String::from_utf16_lossy(&buf[..len as usize]);
            path.to_lowercase().ends_with("aces.exe")
        }
    }

    /// Present GPU-rendered BGRA pixels to a `WS_EX_LAYERED` window via
    /// `UpdateLayeredWindow`.
    ///
    /// `pixels` must be tightly packed BGRA, pre-multiplied alpha,
    /// `width * height * 4` bytes.
    pub fn update_layered_window_from_pixels(
        hwnd: HWND,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) {
        use std::mem::size_of;
        unsafe {
            let screen_dc = GetDC(None);
            let mem_dc = CreateCompatibleDC(screen_dc);

            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize        = size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth       = width as i32;
            bmi.bmiHeader.biHeight      = -(height as i32); // top-down
            bmi.bmiHeader.biPlanes      = 1;
            bmi.bmiHeader.biBitCount    = 32;
            bmi.bmiHeader.biCompression = 0; // BI_RGB

            let mut bits_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let hbmp = match CreateDIBSection(
                screen_dc, &bmi, DIB_RGB_COLORS, &mut bits_ptr, HANDLE::default(), 0,
            ) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("[gpu] CreateDIBSection: {e:?}");
                    ReleaseDC(None, screen_dc);
                    return;
                }
            };
            let old_bmp = SelectObject(mem_dc, HGDIOBJ(hbmp.0));

            // Copy pixel data into the DIB.
            let n = (width * height * 4) as usize;
            assert_eq!(pixels.len(), n, "[gpu] pixel buffer size mismatch");
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits_ptr as *mut u8, n);

            let blend = BLENDFUNCTION {
                BlendOp: 0,             // AC_SRC_OVER
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: 1,         // AC_SRC_ALPHA
            };
            let src_pt = POINT::default();
            let win_sz = SIZE { cx: width as i32, cy: height as i32 };

            let _ = UpdateLayeredWindow(
                hwnd, screen_dc, None, Some(&win_sz),
                mem_dc, Some(&src_pt), COLORREF(0), Some(&blend), ULW_ALPHA,
            );

            SelectObject(mem_dc, old_bmp);
            DeleteObject(HGDIOBJ(hbmp.0));
            DeleteDC(mem_dc);
            ReleaseDC(None, screen_dc);
        }
    }
}

// ─── Non-Windows stubs ────────────────────────────────────────────────────────

#[cfg(not(feature = "windows-glue"))]
pub fn setup_gpu_window(_window: &winit::window::Window) {
    // Wayland / X11 / macOS: winit + wgpu handle transparency natively.
}

/// Hide the overlay window from the taskbar / dock window list.
///
/// - Windows: `WS_EX_TOOLWINDOW` is handled in the `windows-glue` module above.
/// - macOS: Set `NSWindowCollectionBehaviorTransient` so the window is excluded
///   from Mission Control and the Dock's window list.
/// - Linux: no-op (X11 `_NET_WM_WINDOW_TYPE_UTILITY` TODO).
#[cfg(not(feature = "windows-glue"))]
pub fn hide_from_taskbar(window: &winit::window::Window) {
    #[cfg(target_os = "macos")]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(handle) = window.window_handle() {
            if let RawWindowHandle::AppKit(h) = handle.as_raw() {
                unsafe {
                    extern "C" {
                        fn sel_registerName(name: *const std::ffi::c_char) -> *const std::ffi::c_void;
                        fn objc_msgSend();
                    }
                    #[allow(non_camel_case_types)]
                    type objc_msgSend_ptr_fn =
                        unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_void) -> *mut std::ffi::c_void;
                    #[allow(non_camel_case_types)]
                    type objc_msgSend_usize_fn =
                        unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_void, usize);

                    let send_ptr: objc_msgSend_ptr_fn = std::mem::transmute(objc_msgSend as *const ());
                    let send_usize: objc_msgSend_usize_fn = std::mem::transmute(objc_msgSend as *const ());

                    let sel_window = sel_registerName(b"window\0".as_ptr() as *const std::ffi::c_char);
                    let ns_window = send_ptr(h.ns_view.as_ptr() as *mut std::ffi::c_void, sel_window);
                    if ns_window.is_null() { return; }

                    // NSWindowCollectionBehaviorTransient = 1 << 7 (128)
                    // Excludes the window from the dock window list and Mission Control.
                    let sel_cb = sel_registerName(b"setCollectionBehavior:\0".as_ptr() as *const std::ffi::c_char);
                    send_usize(ns_window, sel_cb, 1 << 7);
                }
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = window;
}

#[cfg(not(feature = "windows-glue"))]
pub fn set_click_through(window: &winit::window::Window, click_through: bool) {
    #[cfg(target_os = "macos")]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(handle) = window.window_handle() {
            if let RawWindowHandle::AppKit(h) = handle.as_raw() {
                // SAFETY: ns_window is a valid *mut NSWindow obtained from winit.
                // setIgnoresMouseEvents: is a well-known NSWindow selector that has
                // been stable since macOS 10.0.  The ObjC runtime is already linked
                // transitively via winit/objc2.
                unsafe {
                    // objc_msgSend is variadic; we cast to the concrete signature we need.
                    #[allow(non_camel_case_types)]
                    type objc_msgSend_bool_fn =
                        unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_void, bool);

                    extern "C" {
                        fn sel_registerName(name: *const std::ffi::c_char) -> *const std::ffi::c_void;
                        fn objc_msgSend();
                    }

                    // AppKitWindowHandle only exposes ns_view; get NSWindow via [ns_view window].
                    #[allow(non_camel_case_types)]
                    type objc_msgSend_ptr_fn =
                        unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_void) -> *mut std::ffi::c_void;
                    let send_ptr: objc_msgSend_ptr_fn = std::mem::transmute(objc_msgSend as *const ());
                    let sel_window = sel_registerName(b"window\0".as_ptr() as *const std::ffi::c_char);
                    let ns_window = send_ptr(h.ns_view.as_ptr() as *mut std::ffi::c_void, sel_window);
                    if ns_window.is_null() { return; }

                    let sel = sel_registerName(b"setIgnoresMouseEvents:\0".as_ptr() as *const std::ffi::c_char);
                    let send: objc_msgSend_bool_fn = std::mem::transmute(objc_msgSend as *const ());
                    send(ns_window, sel, click_through);
                }
            }
        }
    }
    // Linux (X11/Wayland): no-op for now.
    // TODO: X11 xcb input shape; Wayland has no standard protocol yet.
    #[cfg(not(target_os = "macos"))]
    let _ = (window, click_through);
}

/// On non-Windows platforms there is no War Thunder foreground check;
/// always return `true` so overlay windows are visible.
#[cfg(not(feature = "windows-glue"))]
pub fn is_warthunder_foreground() -> bool {
    #[cfg(target_os = "linux")]
    { return linux::is_warthunder_foreground(); }

    #[cfg(target_os = "macos")]
    { return macos::is_warthunder_foreground(); }

    #[allow(unreachable_code)]
    true
}

// ─── Linux ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
mod linux {
    /// Returns `true` if War Thunder (`aces`) is the foreground app.
    ///
    /// - Under X11: checks the `_NET_ACTIVE_WINDOW` property to find the focused
    ///   window, reads its `_NET_WM_PID`, then checks `/proc/<pid>/comm`.
    /// - Under Wayland (or if X11 detection fails): checks whether any process
    ///   named `aces` is currently running, since Wayland does not expose focus
    ///   to other clients.
    pub fn is_warthunder_foreground() -> bool {
        // If WAYLAND_DISPLAY is set but DISPLAY is not, skip the X11 path.
        let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        let has_x11     = std::env::var_os("DISPLAY").is_some();

        if has_x11 {
            if let Some(result) = x11_foreground() {
                return result;
            }
        }

        if has_wayland || has_x11 {
            // X11 check failed or Wayland-only: fall back to process existence.
            return is_aces_running();
        }

        true
    }

    /// Check the `_NET_ACTIVE_WINDOW` → `_NET_WM_PID` → `/proc/<pid>/comm` chain.
    /// Returns `None` if X11 is unavailable or any step fails.
    fn x11_foreground() -> Option<bool> {
        use x11rb::connection::Connection;
        use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots[screen_num].root;

        let net_active_window = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW").ok()?.reply().ok()?.atom;
        let net_wm_pid = conn
            .intern_atom(false, b"_NET_WM_PID").ok()?.reply().ok()?.atom;

        // Get the active window ID.
        let prop = conn
            .get_property(false, root, net_active_window, AtomEnum::WINDOW, 0, 1)
            .ok()?.reply().ok()?;
        if prop.value.len() < 4 { return None; }
        let active_win = u32::from_ne_bytes(prop.value[..4].try_into().ok()?);
        if active_win == 0 { return None; }

        // Get _NET_WM_PID of the active window.
        let pid_prop = conn
            .get_property(false, active_win, net_wm_pid, AtomEnum::CARDINAL, 0, 1)
            .ok()?.reply().ok()?;
        if pid_prop.value.len() < 4 { return None; }
        let pid = u32::from_ne_bytes(pid_prop.value[..4].try_into().ok()?);

        // Check the process name.
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        Some(is_warthunder_name(comm.trim()))
    }

    /// Scan `/proc/*/comm` for any running process whose name looks like WT.
    fn is_aces_running() -> bool {
        let Ok(dir) = std::fs::read_dir("/proc") else { return false };
        dir.filter_map(|e| e.ok())
            .any(|e| {
                let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
                is_warthunder_name(comm.trim())
            })
    }

    fn is_warthunder_name(name: &str) -> bool {
        let low = name.to_lowercase();
        low == "aces" || low.starts_with("aces.")
            || low.contains("warthunder")
    }
}

// ─── macOS ────────────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::{c_char, c_void, CStr};

    extern "C" {
        fn objc_msgSend();
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *const c_void;
    }

    type MsgPtr  = unsafe extern "C" fn(*mut c_void, *const c_void) -> *mut c_void;
    type Utf8Fn  = unsafe extern "C" fn(*mut c_void, *const c_void) -> *const c_char;

    /// Returns `true` if the frontmost application is War Thunder (`aces`).
    pub fn is_warthunder_foreground() -> bool {
        unsafe {
            let send: MsgPtr = std::mem::transmute(objc_msgSend as *const ());

            // [NSWorkspace sharedWorkspace]
            let ws_class = objc_getClass(b"NSWorkspace\0".as_ptr() as _);
            if ws_class.is_null() { return false; }
            let workspace = send(ws_class, sel_registerName(b"sharedWorkspace\0".as_ptr() as _));
            if workspace.is_null() { return false; }

            // [workspace frontmostApplication]
            let app = send(workspace, sel_registerName(b"frontmostApplication\0".as_ptr() as _));
            if app.is_null() { return false; }

            // Try [app executableURL] → [url lastPathComponent] → UTF8String.
            let url = send(app, sel_registerName(b"executableURL\0".as_ptr() as _));
            if !url.is_null() {
                let component = send(url, sel_registerName(b"lastPathComponent\0".as_ptr() as _));
                if !component.is_null() {
                    let utf8: Utf8Fn = std::mem::transmute(objc_msgSend as *const ());
                    let cstr = utf8(component, sel_registerName(b"UTF8String\0".as_ptr() as _));
                    if !cstr.is_null() {
                        let name = CStr::from_ptr(cstr).to_string_lossy().to_lowercase();
                        return is_warthunder_name(&name);
                    }
                }
            }

            // Fall back to [app localizedName].
            let utf8: Utf8Fn = std::mem::transmute(objc_msgSend as *const ());
            let name_obj = send(app, sel_registerName(b"localizedName\0".as_ptr() as _));
            if name_obj.is_null() { return false; }
            let cstr = utf8(name_obj, sel_registerName(b"UTF8String\0".as_ptr() as _));
            if cstr.is_null() { return false; }
            let name = CStr::from_ptr(cstr).to_string_lossy().to_lowercase();
            is_warthunder_name(&name)
        }
    }

    fn is_warthunder_name(name: &str) -> bool {
        name == "aces" || name.starts_with("aces.")
            || name.contains("warthunder")
    }
}
