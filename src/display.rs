//! Borderless fullscreen ("windowed fullscreen").
//!
//! miniquad's `set_fullscreen` already makes a borderless popup window, but it always covers the
//! *primary* monitor and, when leaving, restores the window to the top-left corner at the wrong
//! size on high-DPI screens. On Windows we fix both with a few user32 calls (system DLL, no extra
//! crates): cover whichever monitor the window is on, and remember the windowed rectangle so it
//! comes back exactly where it was. Other platforms use miniquad's behaviour as is.

use macroquad::window::set_fullscreen;

#[cfg(windows)]
mod win {
    use std::sync::Mutex;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct Rect {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }

    #[repr(C)]
    struct MonitorInfo {
        cb_size: u32,
        rc_monitor: Rect,
        rc_work: Rect,
        flags: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn GetActiveWindow() -> isize;
        fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
        fn MonitorFromWindow(hwnd: isize, flags: u32) -> isize;
        fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
        fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    }

    const MONITOR_DEFAULTTONEAREST: u32 = 2;
    const SWP_NOZORDER: u32 = 0x0004;
    const SWP_FRAMECHANGED: u32 = 0x0020;
    const SWP_SHOWWINDOW: u32 = 0x0040;

    /// Windowed rectangle saved when entering fullscreen.
    static SAVED: Mutex<Option<Rect>> = Mutex::new(None);

    /// Our window. macroquad runs on the main thread, which owns the window, so the calling
    /// thread's active window is ours. 0 if there is none yet.
    fn hwnd() -> isize {
        unsafe { GetActiveWindow() }
    }

    /// Call before miniquad switches the style: remember where the window is.
    pub fn before_enter() {
        let h = hwnd();
        if h == 0 {
            return;
        }
        let mut r = Rect::default();
        if unsafe { GetWindowRect(h, &mut r) } != 0 && r.right > r.left && r.bottom > r.top {
            *SAVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
        }
    }

    /// Call after miniquad made the window borderless: cover the monitor it is on.
    pub fn after_enter() {
        let h = hwnd();
        if h == 0 {
            return;
        }
        unsafe {
            let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
            let mut mi = MonitorInfo { cb_size: std::mem::size_of::<MonitorInfo>() as u32, rc_monitor: Rect::default(), rc_work: Rect::default(), flags: 0 };
            if mon != 0 && GetMonitorInfoW(mon, &mut mi) != 0 {
                let r = mi.rc_monitor;
                SetWindowPos(h, 0, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_FRAMECHANGED | SWP_SHOWWINDOW);
            }
        }
    }

    /// Call after miniquad restored the framed style: put the window back where it was.
    pub fn after_leave() {
        let h = hwnd();
        let saved = SAVED.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let (true, Some(r)) = (h != 0, saved) {
            unsafe {
                SetWindowPos(h, 0, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_FRAMECHANGED | SWP_SHOWWINDOW);
            }
        }
    }
}

/// Switch between borderless fullscreen and a normal window.
pub fn set_borderless(on: bool) {
    #[cfg(windows)]
    if on {
        win::before_enter();
    }
    set_fullscreen(on);
    #[cfg(windows)]
    if on {
        win::after_enter();
    } else {
        win::after_leave();
    }
}
