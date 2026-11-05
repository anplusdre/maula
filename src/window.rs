//! The window and its event loop, on Win32 directly: one borderless, resizable top-level window (maula draws its own
//! title bar), keyboard and mouse input, DPI, maximize and borderless fullscreen.
//! ponytail: one window per process (its state lives in thread-locals); no IME composition window placement, no
//! touch or pen, no file drops.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::null;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

type Hwnd = isize;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    l: i32,
    t: i32,
    r: i32,
    b: i32,
}

#[repr(C)]
struct Msg {
    hwnd: Hwnd,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt: Point,
}

#[repr(C)]
struct WndClass {
    size: u32,
    style: u32,
    proc_: extern "system" fn(Hwnd, u32, usize, isize) -> isize,
    cls_extra: i32,
    wnd_extra: i32,
    instance: isize,
    icon: isize,
    cursor: isize,
    background: isize,
    menu: *const u16,
    class: *const u16,
    icon_small: isize,
}

#[repr(C)]
struct MinMaxInfo {
    reserved: Point,
    max_size: Point,
    max_pos: Point,
    min_track: Point,
    max_track: Point,
}

#[repr(C)]
struct MonitorInfo {
    size: u32,
    monitor: Rect,
    work: Rect,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Placement {
    length: u32,
    flags: u32,
    show: u32,
    min_pos: Point,
    max_pos: Point,
    normal: Rect,
}

#[repr(C)]
struct Paint {
    dc: isize,
    erase: i32,
    rect: Rect,
    restore: i32,
    inc_update: i32,
    reserved: [u8; 32],
}

#[repr(C)]
struct FlashInfo {
    size: u32,
    hwnd: Hwnd,
    flags: u32,
    count: u32,
    timeout: u32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn RegisterClassExW(c: *const WndClass) -> u16;
    fn CreateWindowExW(ex: u32, class: *const u16, title: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: Hwnd, menu: isize, inst: isize, param: *const c_void) -> Hwnd;
    fn DefWindowProcW(h: Hwnd, m: u32, w: usize, l: isize) -> isize;
    fn ShowWindow(h: Hwnd, cmd: i32) -> i32;
    fn SetWindowPos(h: Hwnd, after: Hwnd, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn GetWindowLongPtrW(h: Hwnd, i: i32) -> isize;
    fn SetWindowLongPtrW(h: Hwnd, i: i32, v: isize) -> isize;
    fn PeekMessageW(m: *mut Msg, h: Hwnd, min: u32, max: u32, remove: u32) -> i32;
    fn TranslateMessage(m: *const Msg) -> i32;
    fn DispatchMessageW(m: *const Msg) -> isize;
    fn MsgWaitForMultipleObjectsEx(n: u32, handles: *const isize, ms: u32, wake: u32, flags: u32) -> u32;
    fn PostMessageW(h: Hwnd, m: u32, w: usize, l: isize) -> i32;
    fn GetClientRect(h: Hwnd, r: *mut Rect) -> i32;
    fn GetDpiForWindow(h: Hwnd) -> u32;
    fn GetDpiForSystem() -> u32;
    fn SetProcessDpiAwarenessContext(ctx: isize) -> i32;
    fn IsZoomed(h: Hwnd) -> i32;
    fn IsIconic(h: Hwnd) -> i32;
    fn MonitorFromWindow(h: Hwnd, flags: u32) -> isize;
    fn MonitorFromRect(r: *const Rect, flags: u32) -> isize;
    fn GetMonitorInfoW(m: isize, info: *mut MonitorInfo) -> i32;
    fn GetWindowPlacement(h: Hwnd, p: *mut Placement) -> i32;
    fn SetWindowPlacement(h: Hwnd, p: *const Placement) -> i32;
    fn LoadCursorW(inst: isize, name: usize) -> isize;
    fn SetCursor(c: isize) -> isize;
    fn LoadImageW(inst: isize, name: usize, kind: u32, cx: i32, cy: i32, flags: u32) -> isize;
    fn GetSystemMetrics(i: i32) -> i32;
    fn SetCapture(h: Hwnd) -> Hwnd;
    fn ReleaseCapture() -> i32;
    fn GetCursorPos(p: *mut Point) -> i32;
    fn GetKeyState(vk: i32) -> i16;
    fn ToUnicodeEx(vk: u32, scan: u32, state: *const u8, buf: *mut u16, len: i32, flags: u32, layout: isize) -> i32;
    fn GetKeyboardLayout(thread: u32) -> isize;
    fn BeginPaint(h: Hwnd, p: *mut Paint) -> isize;
    fn EndPaint(h: Hwnd, p: *const Paint) -> i32;
    fn InvalidateRect(h: Hwnd, r: *const Rect, erase: i32) -> i32;
    fn FlashWindowEx(f: *const FlashInfo) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(name: *const u16) -> isize;
}

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmSetWindowAttribute(h: Hwnd, attr: u32, value: *const c_void, size: u32) -> i32;
}

// messages
const WM_SIZE: u32 = 0x0005;
const WM_SETFOCUS: u32 = 0x0007;
const WM_KILLFOCUS: u32 = 0x0008;
const WM_PAINT: u32 = 0x000F;
const WM_CLOSE: u32 = 0x0010;
const WM_QUIT: u32 = 0x0012;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_SETCURSOR: u32 = 0x0020;
const WM_GETMINMAXINFO: u32 = 0x0024;
const WM_NCCALCSIZE: u32 = 0x0083;
const WM_NCACTIVATE: u32 = 0x0086;
const WM_NCLBUTTONDOWN: u32 = 0x00A1;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_CHAR: u32 = 0x0102;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;
const WM_SYSCHAR: u32 = 0x0106;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_MOUSEHWHEEL: u32 = 0x020E;
const WM_CAPTURECHANGED: u32 = 0x0215;
const WM_DPICHANGED: u32 = 0x02E0;
const WM_APP_WAKE: u32 = 0x8000 + 1;

const WS_CAPTION: u32 = 0x00C0_0000;
const WS_THICKFRAME: u32 = 0x0004_0000;
const GWL_STYLE: i32 = -16;
const SWP_NOZORDER: u32 = 0x4;
const SWP_NOACTIVATE: u32 = 0x10;
const SWP_FRAMECHANGED: u32 = 0x20;
const SWP_NOMOVE: u32 = 0x2;
const SWP_NOSIZE: u32 = 0x1;

// ---- what the app sees ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NamedKey {
    Enter,
    Backspace,
    Tab,
    Escape,
    Space,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Key {
    Named(NamedKey),
    /// The text the key types. With Ctrl or Alt held (not AltGr), the character the key would type without them,
    /// so Ctrl+C reads as "c".
    Character(String),
}

pub struct KeyEvent {
    pub logical_key: Key,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ModifiersState {
    shift: bool,
    ctrl: bool,
    alt: bool,
}

impl ModifiersState {
    pub fn empty() -> Self {
        Self::default()
    }
    pub fn shift_key(&self) -> bool {
        self.shift
    }
    pub fn control_key(&self) -> bool {
        self.ctrl
    }
    pub fn alt_key(&self) -> bool {
        self.alt
    }
    fn now() -> Self {
        let down = |vk: i32| unsafe { GetKeyState(vk) } < 0;
        ModifiersState { shift: down(0x10), ctrl: down(0x11), alt: down(0x12) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ElementState {
    Pressed,
    Released,
}

/// Wheel movement in lines (one notch = 1); precise touchpads give fractions.
#[derive(Clone, Copy, Debug)]
pub enum MouseScrollDelta {
    LineDelta(f32, f32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorIcon {
    Default,
    Pointer,
    Text,
    NsResize,
    EwResize,
    NwseResize,
    NeswResize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResizeDirection {
    North,
    South,
    East,
    West,
    NorthEast,
    NorthWest,
    SouthEast,
    SouthWest,
}

pub enum Event {
    Close,
    Focused(bool),
    /// The client area changed size (never sent while minimized).
    Resized,
    Redraw,
    Modifiers(ModifiersState),
    Key(KeyEvent),
    CursorMoved(i32, i32),
    Mouse(MouseButton, ElementState),
    Wheel(MouseScrollDelta),
    /// A `Waker` was woken: check for messages from other threads.
    Wake,
    /// Everything pending was handled; the answer says when to come back.
    AboutToWait,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Control {
    Wait,
    WaitUntil(Instant),
    Exit,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

// ---- state shared with the window procedure ----

type Handler = Box<dyn FnMut(Event) -> Control>;

thread_local! {
    static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
    static QUEUE: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) }; // raised while the handler ran
    static EXIT: Cell<bool> = const { Cell::new(false) };
    static CURSOR: Cell<isize> = const { Cell::new(0) };
    static FULLSCREEN: Cell<Option<(isize, Placement)>> = const { Cell::new(None) }; // the style and place to go back to
    static MIN_SIZE: Cell<(f32, f32)> = const { Cell::new((0.0, 0.0)) }; // logical px
    static HIGH_SURROGATE: Cell<u16> = const { Cell::new(0) };
    static WAKE_PENDING: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Hands `ev` to the app. If the app is busy (Windows called back into the window procedure from inside one of its
/// own calls, such as resizing the window), the event waits until that call returns.
fn dispatch(ev: Event) -> Control {
    let mut ev = Some(ev);
    let mut last = Control::Wait;
    loop {
        let ran = HANDLER.with(|h| {
            let Ok(mut h) = h.try_borrow_mut() else { return false };
            let Some(h) = h.as_mut() else {
                ev = None; // no app yet (the window is being created): nothing to tell
                return true;
            };
            let e = ev.take().or_else(|| QUEUE.with(|q| q.borrow_mut().pop_front()));
            if let Some(e) = e {
                last = h(e);
                if last == Control::Exit {
                    EXIT.set(true);
                }
            }
            true
        });
        if !ran {
            QUEUE.with(|q| q.borrow_mut().extend(ev.take()));
            return Control::Wait;
        }
        if ev.is_none() && QUEUE.with(|q| q.borrow().is_empty()) {
            return last;
        }
    }
}

// ---- the window ----

/// The window; cheap to copy.
#[derive(Clone, Copy)]
pub struct Window {
    pub hwnd: Hwnd,
}

/// Wakes the event loop from another thread (it then sends `Event::Wake`). Repeated wakes before the loop notices
/// cost one message.
#[derive(Clone)]
pub struct Waker {
    hwnd: Hwnd,
    pending: Arc<AtomicBool>,
}

impl Waker {
    pub fn wake(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            unsafe { PostMessageW(self.hwnd, WM_APP_WAKE, 0, 0) };
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn loword(v: isize) -> i32 {
    (v & 0xffff) as i16 as i32
}
fn hiword(v: isize) -> i32 {
    ((v >> 16) & 0xffff) as i16 as i32
}

impl Window {
    /// Creates the window, `w` x `h` logical px (no smaller than `min_w` x `min_h`), with the exe's icon resource 1.
    /// It is shown at once; events start flowing in `run`.
    pub fn new(title: &str, w: f32, h: f32, min_w: f32, min_h: f32) -> (Window, Waker) {
        unsafe {
            SetProcessDpiAwarenessContext(-4); // per-monitor v2: we draw at the monitor's real resolution
            let inst = GetModuleHandleW(null());
            let small = GetSystemMetrics(49); // SM_CXSMICON
            let class = wide("maula");
            let wc = WndClass {
                size: size_of::<WndClass>() as u32,
                style: 0,
                proc_: wndproc,
                cls_extra: 0,
                wnd_extra: 0,
                instance: inst,
                icon: LoadImageW(inst, 1, 1, 0, 0, 0x40), // resource 1, IMAGE_ICON, LR_DEFAULTSIZE
                cursor: 0,                                 // we set it on WM_SETCURSOR
                background: 0,                             // we paint everything
                menu: null(),
                class: class.as_ptr(),
                icon_small: LoadImageW(inst, 1, 1, small, small, 0),
            };
            RegisterClassExW(&wc);
            MIN_SIZE.set((min_w, min_h));
            CURSOR.set(LoadCursorW(0, 32512));
            let scale = GetDpiForSystem() as f32 / 96.0;
            // the styles a normal window has (Aero Snap, minimize/maximize animations); WM_NCCALCSIZE removes the frame
            const STYLE: u32 = WS_CAPTION | WS_THICKFRAME | 0x0008_0000 | 0x0002_0000 | 0x0001_0000 | 0x0400_0000; // + SYSMENU, MINIMIZEBOX, MAXIMIZEBOX, CLIPSIBLINGS
            const CW_USEDEFAULT: i32 = i32::MIN;
            let hwnd = CreateWindowExW(0, class.as_ptr(), wide(title).as_ptr(), STYLE, CW_USEDEFAULT, CW_USEDEFAULT, (w * scale) as i32, (h * scale) as i32, 0, 0, inst, null());
            assert!(hwnd != 0, "cannot create the window");
            // the system DPI may differ from the monitor the window landed on
            let s = GetDpiForWindow(hwnd) as f32 / 96.0;
            SetWindowPos(hwnd, 0, 0, 0, (w * s) as i32, (h * s) as i32, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
            ShowWindow(hwnd, 5); // SW_SHOW
            let pending = Arc::new(AtomicBool::new(false));
            WAKE_PENDING.with(|p| *p.borrow_mut() = Some(pending.clone()));
            (Window { hwnd }, Waker { hwnd, pending })
        }
    }

    pub fn inner_size(&self) -> Size {
        let mut r = Rect::default();
        unsafe { GetClientRect(self.hwnd, &mut r) };
        Size { width: (r.r - r.l).max(0) as u32, height: (r.b - r.t).max(0) as u32 }
    }

    pub fn scale_factor(&self) -> f64 {
        unsafe { GetDpiForWindow(self.hwnd) as f64 / 96.0 }
    }

    pub fn request_redraw(&self) {
        unsafe { InvalidateRect(self.hwnd, null(), 0) };
    }

    pub fn is_maximized(&self) -> bool {
        unsafe { IsZoomed(self.hwnd) != 0 }
    }

    pub fn is_fullscreen(&self) -> bool {
        FULLSCREEN.get().is_some()
    }

    pub fn set_minimized(&self) {
        unsafe { ShowWindow(self.hwnd, 6) }; // SW_MINIMIZE
    }

    pub fn set_maximized(&self, on: bool) {
        unsafe { ShowWindow(self.hwnd, if on { 3 } else { 9 }) }; // SW_MAXIMIZE / SW_RESTORE
    }

    /// Borderless fullscreen on the monitor the window is on (covering the taskbar), or back to where it was.
    pub fn set_fullscreen(&self, on: bool) {
        unsafe {
            match (on, FULLSCREEN.get()) {
                (true, None) => {
                    let mut place = Placement { length: size_of::<Placement>() as u32, flags: 0, show: 0, min_pos: Point::default(), max_pos: Point::default(), normal: Rect::default() };
                    GetWindowPlacement(self.hwnd, &mut place);
                    let style = GetWindowLongPtrW(self.hwnd, GWL_STYLE);
                    FULLSCREEN.set(Some((style, place)));
                    // without a caption and sizing frame, Windows treats a monitor-sized window as fullscreen
                    SetWindowLongPtrW(self.hwnd, GWL_STYLE, style & !((WS_CAPTION | WS_THICKFRAME) as isize));
                    let mut mi = MonitorInfo { size: size_of::<MonitorInfo>() as u32, monitor: Rect::default(), work: Rect::default(), flags: 0 };
                    GetMonitorInfoW(MonitorFromWindow(self.hwnd, 2), &mut mi); // MONITOR_DEFAULTTONEAREST
                    let m = mi.monitor;
                    SetWindowPos(self.hwnd, 0, m.l, m.t, m.r - m.l, m.b - m.t, SWP_NOZORDER | SWP_FRAMECHANGED);
                }
                (false, Some((style, place))) => {
                    FULLSCREEN.set(None);
                    SetWindowLongPtrW(self.hwnd, GWL_STYLE, style);
                    SetWindowPlacement(self.hwnd, &place);
                    SetWindowPos(self.hwnd, 0, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
                }
                _ => {}
            }
        }
    }

    /// Light or dark system parts of the window (its shadow, the snap preview).
    pub fn set_dark(&self, dark: bool) {
        let v: i32 = dark as i32;
        unsafe { DwmSetWindowAttribute(self.hwnd, 20, &v as *const i32 as *const c_void, 4) }; // DWMWA_USE_IMMERSIVE_DARK_MODE
    }

    /// Flashes the taskbar button until the window is focused.
    pub fn request_user_attention(&self) {
        let f = FlashInfo { size: size_of::<FlashInfo>() as u32, hwnd: self.hwnd, flags: 2 | 12, count: 0, timeout: 0 }; // TRAY | TIMERNOFG
        unsafe { FlashWindowEx(&f) };
    }

    pub fn set_cursor(&self, icon: CursorIcon) {
        let id = match icon {
            CursorIcon::Default => 32512,
            CursorIcon::Text => 32513,
            CursorIcon::Pointer => 32649,
            CursorIcon::NwseResize => 32642,
            CursorIcon::NeswResize => 32643,
            CursorIcon::EwResize => 32644,
            CursorIcon::NsResize => 32645,
        };
        let c = unsafe { LoadCursorW(0, id) };
        CURSOR.set(c);
        unsafe { SetCursor(c) };
    }

    /// Moves the window with the mouse until the button is released (with Aero Snap), as dragging a title bar does.
    pub fn drag_window(&self) {
        self.nc_drag(2); // HTCAPTION
    }

    pub fn drag_resize_window(&self, dir: ResizeDirection) {
        use ResizeDirection::*;
        self.nc_drag(match dir {
            West => 10,
            East => 11,
            North => 12,
            NorthWest => 13,
            NorthEast => 14,
            South => 15,
            SouthWest => 16,
            SouthEast => 17,
        });
    }

    /// Posted, not sent: Windows' move/size loop then runs from the message loop, where it can still hand us
    /// resize and paint events, so the window redraws while it is being dragged.
    fn nc_drag(&self, hit: usize) {
        unsafe {
            let mut p = Point::default();
            GetCursorPos(&mut p);
            ReleaseCapture();
            PostMessageW(self.hwnd, WM_NCLBUTTONDOWN, hit, ((p.y as isize & 0xffff) << 16) | (p.x as isize & 0xffff));
        }
    }
}

/// Runs the event loop until the app answers `Control::Exit`.
pub fn run(handler: impl FnMut(Event) -> Control + 'static) {
    HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(handler)));
    let mut msg = Msg { hwnd: 0, message: 0, wparam: 0, lparam: 0, time: 0, pt: Point::default() };
    loop {
        while unsafe { PeekMessageW(&mut msg, 0, 0, 0, 1) } != 0 {
            if msg.message == WM_QUIT {
                return;
            }
            if !(matches!(msg.message, WM_KEYDOWN | WM_SYSKEYDOWN) && key_down(&msg)) {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            if EXIT.get() {
                return;
            }
        }
        let wait = match dispatch(Event::AboutToWait) {
            _ if EXIT.get() => return,
            Control::Exit => return,
            Control::Wait => u32::MAX,
            Control::WaitUntil(t) => t.saturating_duration_since(Instant::now()).as_millis().min(u32::MAX as u128 - 1) as u32,
        };
        if wait > 0 {
            unsafe { MsgWaitForMultipleObjectsEx(0, null(), wait, 0x04FF, 0x4) }; // QS_ALLINPUT, MWMO_INPUTAVAILABLE
        }
    }
}

/// A key press, before Windows turns it into characters. Returns true when it was handled here (a named key, or a
/// Ctrl/Alt chord), so it must not also become a WM_CHAR. Everything else is typed text: Windows translates it,
/// with dead keys, AltGr and the IME, and it arrives as WM_CHAR.
fn key_down(msg: &Msg) -> bool {
    let vk = msg.wparam as u32;
    let mods = ModifiersState::now();
    dispatch(Event::Modifiers(mods));
    if vk == 0x73 && mods.alt {
        return false; // Alt+F4: Windows closes the window (WM_CLOSE)
    }
    let named = match vk {
        0x0D => Some(NamedKey::Enter),
        0x08 => Some(NamedKey::Backspace),
        0x09 => Some(NamedKey::Tab),
        0x1B => Some(NamedKey::Escape),
        0x20 => Some(NamedKey::Space),
        0x21 => Some(NamedKey::PageUp),
        0x22 => Some(NamedKey::PageDown),
        0x23 => Some(NamedKey::End),
        0x24 => Some(NamedKey::Home),
        0x25 => Some(NamedKey::ArrowLeft),
        0x26 => Some(NamedKey::ArrowUp),
        0x27 => Some(NamedKey::ArrowRight),
        0x28 => Some(NamedKey::ArrowDown),
        0x2D => Some(NamedKey::Insert),
        0x2E => Some(NamedKey::Delete),
        _ => None,
    };
    if let Some(k) = named {
        dispatch(Event::Key(KeyEvent { logical_key: Key::Named(k) }));
        return true;
    }
    if matches!(vk, 0x10 | 0x11 | 0x12 | 0xA0..=0xA5 | 0x5B | 0x5C) {
        return true; // a modifier on its own: reported above; nothing to type (and bare Alt must not open the system menu)
    }
    let chord = mods.ctrl != mods.alt; // Ctrl+Alt together is AltGr: that types characters
    if !chord {
        return false;
    }
    // the character this key types with Shift (and Caps Lock) only; flag 4 leaves dead-key state alone
    let mut state = [0u8; 256];
    state[0x10] = if mods.shift { 0x80 } else { 0 };
    state[0x14] = (unsafe { GetKeyState(0x14) } & 1) as u8;
    let mut buf = [0u16; 8];
    let scan = ((msg.lparam >> 16) & 0xff) as u32;
    let n = unsafe { ToUnicodeEx(vk, scan, state.as_ptr(), buf.as_mut_ptr(), buf.len() as i32, 4, GetKeyboardLayout(0)) };
    if n > 0 {
        let s = String::from_utf16_lossy(&buf[..n as usize]);
        if !s.chars().any(char::is_control) {
            dispatch(Event::Key(KeyEvent { logical_key: Key::Character(s) }));
        }
    }
    true
}

extern "system" fn wndproc(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize {
    unsafe {
        match msg {
            WM_NCCALCSIZE if wparam != 0 => {
                // the whole window is client area (no frame); maximized, it fits the monitor's work area, since a
                // maximized window is placed with its (now invisible) frame hanging off the screen
                if IsZoomed(hwnd) != 0 && FULLSCREEN.get().is_none() {
                    let r = &mut *(lparam as *mut Rect); // rgrc[0] of NCCALCSIZE_PARAMS
                    let mut mi = MonitorInfo { size: size_of::<MonitorInfo>() as u32, monitor: Rect::default(), work: Rect::default(), flags: 0 };
                    if GetMonitorInfoW(MonitorFromRect(r, 0), &mut mi) != 0 {
                        *r = mi.work;
                    }
                }
                0
            }
            WM_NCACTIVATE => DefWindowProcW(hwnd, msg, wparam, -1), // -1: don't paint a frame we don't have
            WM_GETMINMAXINFO => {
                let (w, h) = MIN_SIZE.get();
                let s = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
                let info = &mut *(lparam as *mut MinMaxInfo);
                info.min_track = Point { x: (w * s) as i32, y: (h * s) as i32 };
                0
            }
            WM_DPICHANGED => {
                let r = &*(lparam as *const Rect); // where Windows suggests the window goes at the new DPI
                SetWindowPos(hwnd, 0, r.l, r.t, r.r - r.l, r.b - r.t, SWP_NOZORDER | SWP_NOACTIVATE);
                0
            }
            WM_SIZE => {
                if IsIconic(hwnd) == 0 && lparam != 0 {
                    dispatch(Event::Resized);
                }
                0
            }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                let mut ps: Paint = std::mem::zeroed();
                BeginPaint(hwnd, &mut ps);
                EndPaint(hwnd, &ps);
                dispatch(Event::Redraw);
                0
            }
            WM_CLOSE => {
                dispatch(Event::Close);
                0
            }
            WM_SETFOCUS | WM_KILLFOCUS => {
                dispatch(Event::Modifiers(ModifiersState::now()));
                dispatch(Event::Focused(msg == WM_SETFOCUS));
                0
            }
            WM_KEYUP | WM_SYSKEYUP => {
                dispatch(Event::Modifiers(ModifiersState::now()));
                if msg == WM_SYSKEYUP { 0 } else { DefWindowProcW(hwnd, msg, wparam, lparam) } // Alt/F10 alone: no menu mode
            }
            WM_SYSKEYDOWN => DefWindowProcW(hwnd, msg, wparam, lparam), // only Alt+F4 gets here
            WM_SYSCHAR => 0,                                             // no beep for Alt+key
            WM_CHAR => {
                let u = wparam as u16;
                let s = if (0xD800..0xDC00).contains(&u) {
                    HIGH_SURROGATE.set(u); // first half of a character beyond the BMP (an emoji)
                    return 0;
                } else if (0xDC00..0xE000).contains(&u) {
                    String::from_utf16_lossy(&[HIGH_SURROGATE.replace(0), u])
                } else {
                    String::from_utf16_lossy(&[u])
                };
                if !s.chars().any(char::is_control) {
                    dispatch(Event::Key(KeyEvent { logical_key: Key::Character(s) }));
                }
                0
            }
            WM_MOUSEMOVE => {
                dispatch(Event::CursorMoved(loword(lparam), hiword(lparam)));
                0
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP => {
                let (button, state) = match msg {
                    WM_LBUTTONDOWN => (MouseButton::Left, ElementState::Pressed),
                    WM_LBUTTONUP => (MouseButton::Left, ElementState::Released),
                    WM_RBUTTONDOWN => (MouseButton::Right, ElementState::Pressed),
                    WM_RBUTTONUP => (MouseButton::Right, ElementState::Released),
                    WM_MBUTTONDOWN => (MouseButton::Middle, ElementState::Pressed),
                    _ => (MouseButton::Middle, ElementState::Released),
                };
                // keep getting mouse moves while a button is held outside the window (selecting text)
                if state == ElementState::Pressed {
                    SetCapture(hwnd);
                } else if wparam & 0x13 == 0 {
                    ReleaseCapture(); // MK_LBUTTON | MK_RBUTTON | MK_MBUTTON: none left down
                }
                dispatch(Event::Mouse(button, state));
                0
            }
            WM_CAPTURECHANGED => 0,
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let d = ((wparam >> 16) as u16 as i16) as f32 / 120.0;
                let delta = if msg == WM_MOUSEWHEEL { MouseScrollDelta::LineDelta(0.0, d) } else { MouseScrollDelta::LineDelta(d, 0.0) };
                dispatch(Event::Wheel(delta));
                0
            }
            WM_SETCURSOR if loword(lparam) == 1 => {
                SetCursor(CURSOR.get()); // HTCLIENT: the window's own cursor
                1
            }
            WM_APP_WAKE => {
                WAKE_PENDING.with(|p| p.borrow().as_ref().map(|p| p.store(false, Ordering::Release)));
                dispatch(Event::Wake);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
