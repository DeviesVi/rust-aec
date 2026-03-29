// System tray icon with right-click context menu for device selection.

use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use crossbeam_channel::Sender;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::audio::device::{self, DeviceInfo};
use crate::autostart;
use crate::config;
use crate::engine::EngineCommand;

const WM_TRAYICON: u32 = WM_APP + 1;
const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
const WTS_CONSOLE_CONNECT: u32 = 0x1;
const WTS_SESSION_UNLOCK: u32 = 0x8;

// Menu item ID ranges.
const ID_MIC_BASE: u32 = 1000;
const ID_SPEAKER_BASE: u32 = 1100;
const ID_OUTPUT_BASE: u32 = 1200;
const ID_AUTOSTART: u32 = 2000;
const ID_EXIT: u32 = 9999;

pub struct TrayState {
    pub capture_devices: Vec<DeviceInfo>,
    pub render_devices: Vec<DeviceInfo>,
    pub current_mic_id: Option<String>,
    pub current_speaker_id: Option<String>,
    pub current_output_id: Option<String>,
}

struct TrayContext {
    state: Arc<Mutex<TrayState>>,
    cmd_tx: Sender<EngineCommand>,
}

static TRAY_CTX: AtomicPtr<TrayContext> = AtomicPtr::new(std::ptr::null_mut());

fn get_ctx() -> Option<&'static TrayContext> {
    let ptr = TRAY_CTX.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        unsafe { Some(&*ptr) }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn run_tray(state: Arc<Mutex<TrayState>>, cmd_tx: Sender<EngineCommand>) -> Result<()> {
    unsafe {
        let ctx = Box::new(TrayContext { state, cmd_tx });
        TRAY_CTX.store(Box::into_raw(ctx), Ordering::Release);

        let class_name = wide("RustAecTrayClass");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);

        let title = wide("RustAEC");
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            None,
            None,
        )?;

        // Load embedded icon (resource ID 1) or fall back to default.
        let hicon = {
            let hmodule = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .unwrap_or_default();
            // HMODULE and HINSTANCE are the same underlying type on Windows.
            let hinstance = windows::Win32::Foundation::HINSTANCE(hmodule.0);
            LoadIconW(hinstance, PCWSTR(1 as *const u16)).unwrap_or_else(|_| {
                LoadIconW(None, IDI_APPLICATION).unwrap()
            })
        };

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAYICON,
            hIcon: hicon,
            ..Default::default()
        };
        let tip = "Rust AEC - Echo Cancellation";
        let tip_wide: Vec<u16> = tip.encode_utf16().collect();
        let len = tip_wide.len().min(nid.szTip.len() - 1);
        nid.szTip[..len].copy_from_slice(&tip_wide[..len]);

        let _ = Shell_NotifyIconW(NIM_ADD, &nid);

        // Register for session change notifications (unlock, console connect).
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Cleanup.
        let _ = WTSUnRegisterSessionNotification(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        let ptr = TRAY_CTX.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !ptr.is_null() {
            drop(Box::from_raw(ptr));
        }
    }
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT { unsafe {
    match msg {
        WM_TRAYICON => {
            let event = (lparam.0 & 0xFFFF) as u32;
            if event == WM_RBUTTONUP {
                handle_right_click(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as u32;
            handle_menu_command(id);
            LRESULT(0)
        }
        WM_WTSSESSION_CHANGE => {
            let reason = wparam.0 as u32;
            if reason == WTS_CONSOLE_CONNECT || reason == WTS_SESSION_UNLOCK {
                if let Some(ctx) = get_ctx() {
                    let _ = ctx.cmd_tx.send(EngineCommand::RefreshDevices);
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}}

unsafe fn handle_right_click(hwnd: HWND) { unsafe {
    let ctx = match get_ctx() {
        Some(c) => c,
        None => return,
    };

    // Refresh device lists.
    if let (Ok(capture), Ok(render)) = (
        device::list_capture_devices(),
        device::list_render_devices(),
    ) {
        let mut st = ctx.state.lock().unwrap();
        st.capture_devices = capture;
        st.render_devices = render;
    }

    let st = ctx.state.lock().unwrap();
    let menu = CreatePopupMenu().unwrap();

    // Microphone submenu.
    let mic_menu = CreatePopupMenu().unwrap();
    if st.capture_devices.is_empty() {
        let mut label = wide("No devices found");
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_STATE,
            fState: MFS_DISABLED,
            dwTypeData: PWSTR(label.as_mut_ptr()),
            cch: label.len() as u32 - 1,
            ..Default::default()
        };
        let _ = InsertMenuItemW(mic_menu, 0, true, &mii);
    }
    for (i, dev) in st.capture_devices.iter().enumerate() {
        let mut label = wide(&dev.name);
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_ID | MIIM_STRING | MIIM_FTYPE | MIIM_STATE,
            fType: MFT_RADIOCHECK,
            fState: if Some(&dev.id) == st.current_mic_id.as_ref() {
                MFS_CHECKED
            } else {
                MFS_UNCHECKED
            },
            wID: ID_MIC_BASE + i as u32,
            dwTypeData: PWSTR(label.as_mut_ptr()),
            cch: label.len() as u32 - 1,
            ..Default::default()
        };
        let _ = InsertMenuItemW(mic_menu, i as u32, true, &mii);
    }
    {
        let mut label = wide("Microphone");
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_SUBMENU,
            hSubMenu: mic_menu,
            dwTypeData: PWSTR(label.as_mut_ptr()),
            cch: label.len() as u32 - 1,
            ..Default::default()
        };
        let _ = InsertMenuItemW(menu, 0, true, &mii);
    }

    // Speaker submenu.
    let speaker_menu = CreatePopupMenu().unwrap();
    if st.render_devices.is_empty() {
        let mut label = wide("No devices found");
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_STATE,
            fState: MFS_DISABLED,
            dwTypeData: PWSTR(label.as_mut_ptr()),
            cch: label.len() as u32 - 1,
            ..Default::default()
        };
        let _ = InsertMenuItemW(speaker_menu, 0, true, &mii);
    }
    for (i, dev) in st.render_devices.iter().enumerate() {
        let mut label = wide(&dev.name);
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_ID | MIIM_STRING | MIIM_FTYPE | MIIM_STATE,
            fType: MFT_RADIOCHECK,
            fState: if Some(&dev.id) == st.current_speaker_id.as_ref() {
                MFS_CHECKED
            } else {
                MFS_UNCHECKED
            },
            wID: ID_SPEAKER_BASE + i as u32,
            dwTypeData: PWSTR(label.as_mut_ptr()),
            cch: label.len() as u32 - 1,
            ..Default::default()
        };
        let _ = InsertMenuItemW(speaker_menu, i as u32, true, &mii);
    }
    {
        let mut label = wide("Speaker (Loopback)");
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_SUBMENU,
            hSubMenu: speaker_menu,
            dwTypeData: PWSTR(label.as_mut_ptr()),
            cch: label.len() as u32 - 1,
            ..Default::default()
        };
        let _ = InsertMenuItemW(menu, 1, true, &mii);
    }

    // Output (virtual cable) submenu.
    let output_menu = CreatePopupMenu().unwrap();
    if st.render_devices.is_empty() {
        let mut label = wide("No devices found");
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_STATE,
            fState: MFS_DISABLED,
            dwTypeData: PWSTR(label.as_mut_ptr()),
            cch: label.len() as u32 - 1,
            ..Default::default()
        };
        let _ = InsertMenuItemW(output_menu, 0, true, &mii);
    }
    for (i, dev) in st.render_devices.iter().enumerate() {
        let mut label = wide(&dev.name);
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_ID | MIIM_STRING | MIIM_FTYPE | MIIM_STATE,
            fType: MFT_RADIOCHECK,
            fState: if Some(&dev.id) == st.current_output_id.as_ref() {
                MFS_CHECKED
            } else {
                MFS_UNCHECKED
            },
            wID: ID_OUTPUT_BASE + i as u32,
            dwTypeData: PWSTR(label.as_mut_ptr()),
            cch: label.len() as u32 - 1,
            ..Default::default()
        };
        let _ = InsertMenuItemW(output_menu, i as u32, true, &mii);
    }
    {
        let mut label = wide("Output (Cable)");
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STRING | MIIM_SUBMENU,
            hSubMenu: output_menu,
            dwTypeData: PWSTR(label.as_mut_ptr()),
            cch: label.len() as u32 - 1,
            ..Default::default()
        };
        let _ = InsertMenuItemW(menu, 2, true, &mii);
    }

    // Separator.
    {
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE,
            fType: MFT_SEPARATOR,
            ..Default::default()
        };
        let _ = InsertMenuItemW(menu, 3, true, &mii);
    }

    // Autostart toggle.
    {
        let autostart_on = autostart::is_autostart_enabled();
        let mut label = wide("Start with Windows");
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_ID | MIIM_STRING | MIIM_STATE,
            fState: if autostart_on {
                MFS_CHECKED
            } else {
                MFS_UNCHECKED
            },
            wID: ID_AUTOSTART,
            dwTypeData: PWSTR(label.as_mut_ptr()),
            cch: label.len() as u32 - 1,
            ..Default::default()
        };
        let _ = InsertMenuItemW(menu, 4, true, &mii);
    }

    // Separator.
    {
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE,
            fType: MFT_SEPARATOR,
            ..Default::default()
        };
        let _ = InsertMenuItemW(menu, 5, true, &mii);
    }

    // Exit.
    {
        let mut label = wide("Exit");
        let mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_ID | MIIM_STRING,
            wID: ID_EXIT,
            dwTypeData: PWSTR(label.as_mut_ptr()),
            cch: label.len() as u32 - 1,
            ..Default::default()
        };
        let _ = InsertMenuItemW(menu, 6, true, &mii);
    }

    drop(st);

    // Show the popup menu.
    let mut pt = windows::Win32::Foundation::POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
    let _ = DestroyMenu(menu);
}}

unsafe fn handle_menu_command(id: u32) { unsafe {
    let ctx = match get_ctx() {
        Some(c) => c,
        None => return,
    };

    if id >= ID_MIC_BASE && id < ID_MIC_BASE + 100 {
        let idx = (id - ID_MIC_BASE) as usize;
        let st = ctx.state.lock().unwrap();
        if let Some(dev) = st.capture_devices.get(idx) {
            let new_id = dev.id.clone();
            config::save(Some(&new_id), st.current_speaker_id.as_deref(), st.current_output_id.as_deref());
            drop(st);
            let _ = ctx.cmd_tx.send(EngineCommand::SetMicDevice(new_id));
        }
    } else if id >= ID_SPEAKER_BASE && id < ID_SPEAKER_BASE + 100 {
        let idx = (id - ID_SPEAKER_BASE) as usize;
        let st = ctx.state.lock().unwrap();
        if let Some(dev) = st.render_devices.get(idx) {
            let new_id = dev.id.clone();
            config::save(st.current_mic_id.as_deref(), Some(&new_id), st.current_output_id.as_deref());
            drop(st);
            let _ = ctx.cmd_tx.send(EngineCommand::SetSpeakerDevice(new_id));
        }
    } else if id >= ID_OUTPUT_BASE && id < ID_OUTPUT_BASE + 100 {
        let idx = (id - ID_OUTPUT_BASE) as usize;
        let st = ctx.state.lock().unwrap();
        if let Some(dev) = st.render_devices.get(idx) {
            let new_id = dev.id.clone();
            config::save(st.current_mic_id.as_deref(), st.current_speaker_id.as_deref(), Some(&new_id));
            drop(st);
            let _ = ctx.cmd_tx.send(EngineCommand::SetOutputDevice(new_id));
        }
    } else if id == ID_AUTOSTART {
        if autostart::is_autostart_enabled() {
            let _ = autostart::disable_autostart();
        } else {
            let _ = autostart::enable_autostart();
        }
    } else if id == ID_EXIT {
        let _ = ctx.cmd_tx.send(EngineCommand::Shutdown);
        PostQuitMessage(0);
    }
}}
