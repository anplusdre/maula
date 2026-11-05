//! The bits of Win32 maula needs, declared by hand: the shell's pseudo console (ConPTY), the clipboard, and
//! putting a finished frame on the window. Windows only.

use std::{
    ffi::{c_void, OsStr, OsString},
    fs::File,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, RawHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::OnceLock,
};

type Handle = isize;

#[repr(C)]
struct StartupInfoEx {
    cb: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_chars: u32,
    y_chars: u32,
    fill: u32,
    flags: u32,
    show: u16,
    reserved2_len: u16,
    reserved2: *mut u8,
    stdin: Handle,
    stdout: Handle,
    stderr: Handle,
    attrs: *mut c_void, // STARTUPINFOEXW's lpAttributeList
}

#[repr(C)]
struct ProcessInfo {
    process: Handle,
    thread: Handle,
    pid: u32,
    tid: u32,
}

#[repr(C)]
struct BitmapInfo {
    size: u32,
    width: i32,
    height: i32, // negative: rows run top-down
    planes: u16,
    bits: u16,
    compression: u32,
    image_size: u32,
    x_ppm: i32,
    y_ppm: i32,
    used: u32,
    important: u32,
    colors: [u32; 1],
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreatePipe(read: *mut Handle, write: *mut Handle, attrs: *const c_void, size: u32) -> i32;
    fn CloseHandle(h: Handle) -> i32;
    fn LoadLibraryW(name: *const u16) -> Handle;
    fn GetProcAddress(module: Handle, name: *const u8) -> *const c_void;
    fn InitializeProcThreadAttributeList(list: *mut c_void, count: u32, flags: u32, size: *mut usize) -> i32;
    fn UpdateProcThreadAttribute(list: *mut c_void, flags: u32, attr: usize, value: *const c_void, size: usize, prev: *mut c_void, ret: *mut usize) -> i32;
    fn DeleteProcThreadAttributeList(list: *mut c_void);
    fn CreateProcessW(app: *const u16, cmd: *mut u16, pa: *const c_void, ta: *const c_void, inherit: i32, flags: u32, env: *const c_void, cwd: *const u16, si: *const StartupInfoEx, pi: *mut ProcessInfo) -> i32;
    fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
    fn GlobalAlloc(flags: u32, bytes: usize) -> Handle;
    fn GlobalLock(h: Handle) -> *mut c_void;
    fn GlobalUnlock(h: Handle) -> i32;
    fn GlobalFree(h: Handle) -> Handle;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn OpenClipboard(owner: Handle) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn GetClipboardData(format: u32) -> Handle;
    fn SetClipboardData(format: u32, data: Handle) -> Handle;
    fn GetDC(hwnd: Handle) -> Handle;
    fn ReleaseDC(hwnd: Handle, dc: Handle) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn SetDIBitsToDevice(dc: Handle, x: i32, y: i32, w: u32, h: u32, sx: i32, sy: i32, start: u32, lines: u32, bits: *const c_void, bmi: *const BitmapInfo, usage: u32) -> i32;
}

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain([0]).collect()
}

// ---- pseudo console ----

#[repr(C)]
#[derive(Clone, Copy)]
struct Coord {
    x: i16,
    y: i16,
}

struct ConPty {
    create: unsafe extern "system" fn(Coord, Handle, Handle, u32, *mut Handle) -> i32,
    resize: unsafe extern "system" fn(Handle, Coord) -> i32,
    close: unsafe extern "system" fn(Handle),
}

/// The newer ConPTY (OpenConsole) when `conpty.dll` sits next to maula.exe (with `x64\OpenConsole.exe`), else the one
/// built into Windows.
fn conpty() -> &'static ConPty {
    static FNS: OnceLock<ConPty> = OnceLock::new();
    FNS.get_or_init(|| unsafe {
        let side = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join("conpty.dll"))).filter(|p| p.exists());
        let mut lib = side.map_or(0, |p| LoadLibraryW(wide(p).as_ptr())); // a full path: never searched for
        if lib == 0 {
            lib = LoadLibraryW(wide("kernel32.dll").as_ptr());
        }
        let f = |name: &[u8]| {
            let p = GetProcAddress(lib, name.as_ptr());
            assert!(!p.is_null(), "ConPTY needs Windows 10 1809 or later");
            p
        };
        ConPty {
            create: std::mem::transmute(f(b"CreatePseudoConsole\0")),
            resize: std::mem::transmute(f(b"ResizePseudoConsole\0")),
            close: std::mem::transmute(f(b"ClosePseudoConsole\0")),
        }
    })
}

/// A shell running in a pseudo console. Dropping it closes the console, which ends the shell.
pub struct Pty(Handle);

/// A process handle, waited on from another thread.
pub struct Process(Handle);

impl Process {
    /// Blocks until the process exits.
    pub fn wait(self) {
        unsafe { WaitForSingleObject(self.0, u32::MAX) };
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

impl Pty {
    pub fn resize(&self, cols: usize, rows: usize) {
        unsafe { (conpty().resize)(self.0, coord(cols, rows)) };
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe { (conpty().close)(self.0) };
    }
}

fn coord(cols: usize, rows: usize) -> Coord {
    Coord { x: cols.min(i16::MAX as usize) as i16, y: rows.min(i16::MAX as usize) as i16 }
}

/// Starts `exe args` in a new pseudo console of `cols` x `rows`, with `env` added to maula's environment.
/// Returns the console, its output (to read) and input (to write), and the process.
pub fn spawn(exe: &str, args: &[&str], env: &[(&str, String)], cwd: Option<&Path>, cols: usize, rows: usize) -> io::Result<(Pty, File, File, Process)> {
    let err = || io::Error::last_os_error();
    unsafe {
        let (mut in_r, mut in_w, mut out_r, mut out_w) = (0, 0, 0, 0);
        if CreatePipe(&mut in_r, &mut in_w, null(), 0) == 0 {
            return Err(err());
        }
        if CreatePipe(&mut out_r, &mut out_w, null(), 0) == 0 {
            CloseHandle(in_r);
            CloseHandle(in_w);
            return Err(err());
        }
        let (input, output) = (File::from_raw_handle(in_w as RawHandle), File::from_raw_handle(out_r as RawHandle));
        let mut hpc = 0;
        const INHERIT_CURSOR: u32 = 1;
        const RESIZE_QUIRK: u32 = 2;
        const WIN32_INPUT_MODE: u32 = 4;
        let hr = (conpty().create)(coord(cols, rows), in_r, out_w, INHERIT_CURSOR | RESIZE_QUIRK | WIN32_INPUT_MODE, &mut hpc);
        CloseHandle(in_r); // the console holds its own copies
        CloseHandle(out_w);
        if hr != 0 {
            return Err(io::Error::other(format!("CreatePseudoConsole failed: HRESULT {hr:#x}")));
        }
        let pty = Pty(hpc);

        let mut size = 0;
        InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
        let mut list = vec![0u8; size];
        let lp = list.as_mut_ptr() as *mut c_void;
        if InitializeProcThreadAttributeList(lp, 1, 0, &mut size) == 0 {
            return Err(err());
        }
        const PSEUDOCONSOLE: usize = 0x20016;
        let ok = UpdateProcThreadAttribute(lp, 0, PSEUDOCONSOLE, hpc as *const c_void, size_of::<Handle>(), null_mut(), null_mut());
        if ok == 0 {
            DeleteProcThreadAttributeList(lp);
            return Err(err());
        }
        const USESTDHANDLES: u32 = 0x100;
        let mut si: StartupInfoEx = std::mem::zeroed();
        si.cb = size_of::<StartupInfoEx>() as u32;
        si.flags = USESTDHANDLES; // invalid std handles: the shell must not inherit maula's own (a log file, the dev console)
        (si.stdin, si.stdout, si.stderr) = (-1, -1, -1);
        si.attrs = lp;

        let exe = find_exe(exe);
        let mut line = Vec::new();
        quote(exe.as_os_str(), &mut line);
        for a in args {
            line.push(' ' as u16);
            quote(OsStr::new(a), &mut line);
        }
        line.push(0);
        let block = env_block(env);
        let cwd = cwd.map(wide);
        let mut pi: ProcessInfo = std::mem::zeroed();
        const EXTENDED_STARTUPINFO: u32 = 0x80000;
        const UNICODE_ENVIRONMENT: u32 = 0x400;
        let ok = CreateProcessW(
            wide(&exe).as_ptr(),
            line.as_mut_ptr(),
            null(),
            null(),
            0,
            EXTENDED_STARTUPINFO | UNICODE_ENVIRONMENT,
            block.as_ptr() as *const c_void,
            cwd.as_ref().map_or(null(), |c| c.as_ptr()),
            &si,
            &mut pi,
        );
        DeleteProcThreadAttributeList(lp);
        if ok == 0 {
            return Err(err());
        }
        CloseHandle(pi.thread);
        Ok((pty, output, input, Process(pi.process)))
    }
}

/// `exe` as given when it names a path, else found on PATH (".exe" added when it has no extension). Searching only
/// PATH, unlike CreateProcess, keeps a stray powershell.exe in the current folder from running instead.
fn find_exe(exe: &str) -> PathBuf {
    let p = Path::new(exe);
    if p.components().count() > 1 {
        return p.into();
    }
    let name = if p.extension().is_some() { p.into() } else { p.with_extension("exe") };
    std::env::var_os("PATH")
        .and_then(|path| std::env::split_paths(&path).map(|d| d.join(&name)).find(|f| f.is_file()))
        .unwrap_or(name)
}

/// Appends `arg` to a command line, quoted the way programs split it back (CommandLineToArgvW rules).
fn quote(arg: &OsStr, out: &mut Vec<u16>) {
    let a: Vec<u16> = arg.encode_wide().collect();
    let (q, bs) = ('"' as u16, '\\' as u16);
    if !a.is_empty() && !a.iter().any(|&c| c == ' ' as u16 || c == '\t' as u16 || c == q) {
        return out.extend(a);
    }
    out.push(q);
    let mut slashes = 0;
    for c in a {
        if c == bs {
            slashes += 1;
            continue;
        }
        let n = if c == q { slashes * 2 + 1 } else { slashes }; // backslashes before a quote are doubled, plus one for it
        out.extend(std::iter::repeat_n(bs, n));
        out.push(c);
        slashes = 0;
    }
    out.extend(std::iter::repeat_n(bs, slashes * 2)); // ...and before the closing quote
    out.push(q);
}

/// maula's environment with `extra` set (names compare without case), sorted, as CreateProcess wants it.
fn env_block(extra: &[(&str, String)]) -> Vec<u16> {
    let mut vars: Vec<(OsString, OsString)> = std::env::vars_os().filter(|(k, _)| !extra.iter().any(|(e, _)| k.eq_ignore_ascii_case(e))).collect();
    vars.extend(extra.iter().map(|(k, v)| (k.into(), v.into())));
    vars.sort_by_key(|(k, _)| k.to_ascii_uppercase());
    let mut block = Vec::new();
    for (k, v) in vars {
        block.extend(k.encode_wide());
        block.push('=' as u16);
        block.extend(v.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

// ---- clipboard ----

const UNICODE_TEXT: u32 = 13;

/// Another program can hold the clipboard for a moment: try a few times.
fn open_clipboard(owner: Handle) -> bool {
    for _ in 0..5 {
        if unsafe { OpenClipboard(owner) } != 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

pub fn clipboard_get(hwnd: isize) -> Option<String> {
    if !open_clipboard(hwnd) {
        return None;
    }
    let text = unsafe {
        let h = GetClipboardData(UNICODE_TEXT);
        let p = if h == 0 { null_mut() } else { GlobalLock(h) as *const u16 };
        if p.is_null() {
            None
        } else {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
            GlobalUnlock(h);
            Some(s)
        }
    };
    unsafe { CloseClipboard() };
    text
}

/// `hwnd` becomes the clipboard's owner (Windows refuses the data without one).
pub fn clipboard_set(hwnd: isize, text: &str) {
    let w: Vec<u16> = text.encode_utf16().chain([0]).collect();
    if !open_clipboard(hwnd) {
        return;
    }
    unsafe {
        EmptyClipboard();
        const MOVEABLE: u32 = 2;
        let h = GlobalAlloc(MOVEABLE, w.len() * 2);
        let p = if h == 0 { null_mut() } else { GlobalLock(h) as *mut u16 };
        if !p.is_null() {
            p.copy_from_nonoverlapping(w.as_ptr(), w.len());
            GlobalUnlock(h);
            if SetClipboardData(UNICODE_TEXT, h) == 0 {
                GlobalFree(h); // not taken: still ours to free
            }
        } else if h != 0 {
            GlobalFree(h);
        }
        CloseClipboard();
    }
}

// ---- presenting ----

/// Copies the rect (x, y, w, h) of `frame` (`width` pixels per row, 0x00RRGGBB) onto the window.
pub fn present(hwnd: isize, frame: &[u32], width: i32, (x, y, w, h): (i32, i32, i32, i32)) {
    if w <= 0 || h <= 0 {
        return;
    }
    // the bitmap handed over is just the rows y..y+h, so the source row offset is 0 whichever way GDI counts rows
    let bmi = BitmapInfo { size: 40, width, height: -h, planes: 1, bits: 32, compression: 0, image_size: 0, x_ppm: 0, y_ppm: 0, used: 0, important: 0, colors: [0] };
    let rows = &frame[(y * width) as usize..((y + h) * width) as usize];
    unsafe {
        let dc = GetDC(hwnd);
        if dc != 0 {
            SetDIBitsToDevice(dc, x, y, w as u32, h as u32, x, 0, 0, h as u32, rows.as_ptr() as *const c_void, &bmi, 0);
            ReleaseDC(hwnd, dc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_round_trips_through_argv_rules() {
        let q = |s: &str| {
            let mut v = Vec::new();
            quote(OsStr::new(s), &mut v);
            String::from_utf16(&v).unwrap()
        };
        assert_eq!(q("plain"), "plain");
        assert_eq!(q(""), r#""""#);
        assert_eq!(q("a b"), r#""a b""#);
        assert_eq!(q(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(q(r"C:\dir\ x\"), r#""C:\dir\ x\\""#, "trailing backslashes doubled before the closing quote");
        assert_eq!(q(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(q(r"no\space"), r"no\space", "backslashes alone need nothing");
    }

    #[test]
    fn env_block_sets_and_overrides() {
        let b = env_block(&[("path", "X".into()), ("MAULA_T", "1".into())]);
        let s = String::from_utf16(&b).unwrap();
        assert!(s.ends_with("\0\0"));
        assert!(s.split('\0').any(|e| e == "MAULA_T=1"));
        assert_eq!(s.split('\0').filter(|e| e.to_ascii_uppercase().starts_with("PATH=")).count(), 1, "one PATH, ours");
    }
}
