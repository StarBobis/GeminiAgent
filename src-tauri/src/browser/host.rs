use super::Bounds;
use std::{
    io,
    path::{Path, PathBuf},
    ptr::null_mut,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, SetLastError, HWND, LPARAM, POINT, RECT},
    Graphics::Gdi::ClientToScreen,
    System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    },
    UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetClientRect, GetWindowLongPtrW, GetWindowRect,
        GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
        SetWindowLongPtrW, SetWindowPos, GWLP_HWNDPARENT, GWL_EXSTYLE, GWL_STYLE,
        SWP_ASYNCWINDOWPOS, SWP_FRAMECHANGED, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, WS_CAPTION, WS_CLIPSIBLINGS,
        WS_EX_APPWINDOW, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
    },
};

#[derive(Default)]
pub struct GeminiHost {
    state: Mutex<HostState>,
}

#[derive(Default)]
struct HostState {
    active: Option<HostedWindow>,
    previous: Option<isize>,
}

struct HostedWindow {
    id: isize,
    host: isize,
    owner: isize,
    style: isize,
    ex_style: isize,
    rect: RECT,
    bounds: Bounds,
}

struct WindowSearch<'a> {
    executable: &'a Path,
    matched: Vec<isize>,
}

fn process_matches(hwnd: HWND, executable: &Path) -> bool {
    let mut pid = 0;
    // The process handle is closed after the image path query.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return false;
        }
        let mut path = [0u16; 1024];
        let mut len = path.len() as u32;
        let matched = QueryFullProcessImageNameW(process, 0, path.as_mut_ptr(), &mut len) != 0;
        CloseHandle(process);
        matched
            && executable
                .to_string_lossy()
                .eq_ignore_ascii_case(&String::from_utf16_lossy(&path[..len as usize]))
    }
}

unsafe extern "system" fn collect_window(hwnd: HWND, data: LPARAM) -> i32 {
    let search = unsafe { &mut *(data as *mut WindowSearch<'_>) };
    // EnumWindows calls this synchronously, and hwnd is only inspected here.
    unsafe {
        if IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let mut class = [0u16; 64];
        let len = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
        if len > 0
            && String::from_utf16_lossy(&class[..len as usize]) == "Chrome_WidgetWin_1"
            && process_matches(hwnd, search.executable)
        {
            search.matched.push(hwnd as isize);
        }
    }
    1
}

pub fn browser_windows(executable: &Path) -> Vec<isize> {
    let mut search = WindowSearch {
        executable,
        matched: Vec::new(),
    };
    unsafe {
        EnumWindows(
            Some(collect_window),
            &mut search as *mut WindowSearch<'_> as LPARAM,
        )
    };
    search.matched
}

fn has_gemini_title(id: isize) -> bool {
    let mut title = [0u16; 256];
    let len = unsafe { GetWindowTextW(id as HWND, title.as_mut_ptr(), title.len() as i32) };
    len > 0
        && String::from_utf16_lossy(&title[..len as usize])
            .to_ascii_lowercase()
            .contains("gemini")
}

pub fn wait_for_browser(executable: PathBuf, before: Vec<isize>) -> Option<isize> {
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        if let Some(id) = browser_windows(&executable)
            .into_iter()
            .find(|id| !before.contains(id) && has_gemini_title(*id))
        {
            return Some(id);
        }
        thread::sleep(Duration::from_millis(125));
    }
    None
}

fn set_window_value(hwnd: HWND, index: i32, value: isize) -> io::Result<()> {
    unsafe {
        SetLastError(0);
        if SetWindowLongPtrW(hwnd, index, value) == 0 && GetLastError() != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl HostedWindow {
    fn attach(id: isize, host: isize, bounds: Bounds) -> io::Result<Self> {
        let hwnd = id as HWND;
        let (style, ex_style, owner, rect) = unsafe {
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect) == 0 {
                return Err(io::Error::last_os_error());
            }
            (
                GetWindowLongPtrW(hwnd, GWL_STYLE),
                GetWindowLongPtrW(hwnd, GWL_EXSTYLE),
                GetWindowLongPtrW(hwnd, GWLP_HWNDPARENT),
                rect,
            )
        };
        let hosted = Self {
            id,
            host,
            owner,
            style,
            ex_style,
            rect,
            bounds,
        };
        let decorations =
            (WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX) as isize;
        // Keep Chromium's top-level activation so it retains native keyboard focus and IME.
        set_window_value(hwnd, GWLP_HWNDPARENT, host)?;
        set_window_value(
            hwnd,
            GWL_STYLE,
            (style & !decorations) | (WS_POPUP | WS_CLIPSIBLINGS) as isize,
        )?;
        set_window_value(hwnd, GWL_EXSTYLE, ex_style & !(WS_EX_APPWINDOW as isize))?;
        hosted.position(SWP_FRAMECHANGED)?;
        Ok(hosted)
    }

    fn position(&self, extra_flags: u32) -> io::Result<()> {
        let host = self.host as HWND;
        let hwnd = self.id as HWND;
        let flags = extra_flags | SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER;
        let mut client = RECT::default();
        unsafe {
            if IsIconic(host) != 0 {
                return Ok(());
            }
            if GetClientRect(host, &mut client) == 0 {
                return Err(io::Error::last_os_error());
            }
            let bounds = self.bounds;
            let x = bounds.x.clamp(0, client.right);
            let y = bounds.y.clamp(0, client.bottom);
            let width = bounds.x.saturating_add(bounds.width).clamp(x, client.right) - x;
            let height = bounds
                .y
                .saturating_add(bounds.height)
                .clamp(y, client.bottom)
                - y;
            let mut origin = POINT { x, y };
            let placed = if width == 0 || height == 0 {
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    0,
                    0,
                    0,
                    0,
                    flags | SWP_HIDEWINDOW | SWP_NOMOVE | SWP_NOSIZE,
                )
            } else {
                if ClientToScreen(host, &mut origin) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut current = RECT::default();
                if extra_flags & SWP_FRAMECHANGED == 0
                    && IsWindowVisible(hwnd) != 0
                    && GetWindowRect(hwnd, &mut current) != 0
                    && current.left == origin.x
                    && current.top == origin.y
                    && current.right - current.left == width
                    && current.bottom - current.top == height
                {
                    return Ok(());
                }
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    origin.x,
                    origin.y,
                    width,
                    height,
                    flags | SWP_SHOWWINDOW,
                )
            };
            if placed == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

impl Drop for HostedWindow {
    fn drop(&mut self) {
        let hwnd = self.id as HWND;
        unsafe {
            if IsWindow(hwnd) == 0 {
                return;
            }
            SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, self.owner);
            SetWindowLongPtrW(hwnd, GWL_STYLE, self.style);
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, self.ex_style);
            SetWindowPos(
                hwnd,
                null_mut(),
                self.rect.left,
                self.rect.top,
                self.rect.right - self.rect.left,
                self.rect.bottom - self.rect.top,
                SWP_FRAMECHANGED
                    | SWP_NOACTIVATE
                    | SWP_SHOWWINDOW
                    | SWP_NOZORDER
                    | SWP_NOOWNERZORDER,
            );
        }
    }
}

impl GeminiHost {
    pub fn active_window(&self) -> Option<isize> {
        self.state
            .lock()
            .unwrap()
            .active
            .as_ref()
            .map(|window| window.id)
    }

    pub fn previous(&self, executable: &Path) -> Option<isize> {
        self.state.lock().unwrap().previous.filter(|id| {
            let hwnd = *id as HWND;
            (unsafe { IsWindow(hwnd) != 0 }) && process_matches(hwnd, executable)
        })
    }

    pub fn attach(&self, id: isize, host: isize, bounds: Bounds) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let window = HostedWindow::attach(id, host, bounds)?;
        state.active = Some(window);
        state.previous = Some(id);
        Ok(())
    }

    pub fn resize(&self, bounds: Bounds) -> io::Result<bool> {
        let mut state = self.state.lock().unwrap();
        if let Some(window) = state.active.as_mut() {
            if unsafe { IsWindow(window.id as HWND) } != 0 {
                window.bounds = bounds;
                window.position(SWP_ASYNCWINDOWPOS)?;
                return Ok(true);
            }
        }
        state.active = None;
        Ok(false)
    }

    pub fn sync_position(&self) {
        if let Some(window) = self.state.lock().unwrap().active.as_ref() {
            let _ = window.position(SWP_ASYNCWINDOWPOS);
        }
    }

    pub fn detach(&self) {
        self.state.lock().unwrap().active.take();
    }
}
