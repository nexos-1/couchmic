//! Tray icon (notification area) and Windows notifications.
//!
//! The tray icon lives in its own thread with a Win32 message loop (tray-icon and muda require
//! that). Status changes arrive through a channel; a WM_APP message wakes the loop. Menu clicks
//! go back to the server as `TrayCommand`.
//!
//! Icon: grey dot = waiting, green dot = iPad connected, yellow dot = connected, but trouble
//! (loss/underruns within the last minute).

use crate::i18n::Texts;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG, WM_APP,
};

#[derive(Debug, Clone, PartialEq)]
pub enum TrayState {
    Idle,
    Connected {
        path: String,
        clients: u32,
        trouble: bool,
    },
}

#[derive(Debug, Clone)]
pub enum TrayCommand {
    /// Automatic default microphone switching on/off
    SetSwitching(bool),
    Quit,
}

enum Update {
    State(TrayState, String),
    Quit,
}

pub struct TrayHandle {
    tx: Sender<Update>,
    thread_id: u32,
    last: Mutex<Option<(TrayState, String)>>,
}

impl TrayHandle {
    /// Set status and tooltip. The tray thread is only woken on change.
    pub fn set_state(&self, state: TrayState, tooltip: String) {
        let mut last = self.last.lock().unwrap();
        if last.as_ref().map(|(s, t)| s == &state && t == &tooltip) == Some(true) {
            return;
        }
        *last = Some((state.clone(), tooltip.clone()));
        let _ = self.tx.send(Update::State(state, tooltip));
        self.wake();
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(Update::Quit);
        self.wake();
    }

    fn wake(&self) {
        unsafe {
            let _ = PostThreadMessageW(
                self.thread_id,
                WM_APP,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
    }
}

fn dot_icon(rgb: [u8; 3]) -> Icon {
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let c = (size as f32 - 1.0) / 2.0;
    let r = size as f32 / 2.0 - 3.0;
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            // 1 px soft edge
            let a = ((r - d + 0.5).clamp(0.0, 1.0) * 255.0) as u8;
            let i = ((y * size + x) * 4) as usize;
            rgba[i] = rgb[0];
            rgba[i + 1] = rgb[1];
            rgba[i + 2] = rgb[2];
            rgba[i + 3] = a;
        }
    }
    Icon::from_rgba(rgba, size, size).expect("icon")
}

fn open_in_shell(target: &str) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(target)
        .spawn();
}

/// Starts the tray thread. `ui_url` opens on a click on the icon.
pub fn start(
    ui_url: String,
    log_dir: std::path::PathBuf,
    switching_enabled: bool,
    texts: Arc<Texts>,
    commands: tokio::sync::mpsc::UnboundedSender<TrayCommand>,
) -> Result<TrayHandle, String> {
    let (tx, rx): (Sender<Update>, Receiver<Update>) = channel();
    let (ready_tx, ready_rx) = channel::<Result<u32, String>>();

    std::thread::Builder::new()
        .name("glass-mic-tray".into())
        .spawn(move || {
            let thread_id = unsafe { GetCurrentThreadId() };
            let icon_idle = dot_icon([0x94, 0xa0, 0xad]);
            let icon_on = dot_icon([0x34, 0xc4, 0xa4]);
            let icon_warn = dot_icon([0xe0, 0xa6, 0x3a]);

            let menu = Menu::new();
            let status = MenuItem::new(texts.waiting(), false, None);
            let switching =
                CheckMenuItem::new(texts.menu_auto_switch(), true, switching_enabled, None);
            let open_ui = MenuItem::new(texts.menu_open_ui(), true, None);
            let open_logs = MenuItem::new(texts.menu_open_logs(), true, None);
            let quit = MenuItem::new(texts.menu_quit(), true, None);
            let built = menu.append_items(&[
                &status,
                &PredefinedMenuItem::separator(),
                &switching,
                &open_ui,
                &open_logs,
                &PredefinedMenuItem::separator(),
                &quit,
            ]);
            if let Err(e) = built {
                let _ = ready_tx.send(Err(e.to_string()));
                return;
            }

            let tray = match TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip(texts.waiting())
                .with_icon(icon_idle.clone())
                .build()
            {
                Ok(t) => t,
                Err(e) => {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };

            let ids = (
                switching.id().clone(),
                open_ui.id().clone(),
                open_logs.id().clone(),
                quit.id().clone(),
            );
            let logs = log_dir.to_string_lossy().to_string();
            // Menu and click events arrive through channels; they are produced during
            // DispatchMessageW and picked up right after (same thread).
            let menu_rx = MenuEvent::receiver();
            let tray_rx = TrayIconEvent::receiver();
            let handle_events = |switching: &CheckMenuItem| {
                while let Ok(ev) = menu_rx.try_recv() {
                    if ev.id == ids.0 {
                        let _ = commands.send(TrayCommand::SetSwitching(switching.is_checked()));
                    } else if ev.id == ids.1 {
                        open_in_shell(&ui_url);
                    } else if ev.id == ids.2 {
                        open_in_shell(&logs);
                    } else if ev.id == ids.3 {
                        let _ = commands.send(TrayCommand::Quit);
                    }
                }
                while let Ok(ev) = tray_rx.try_recv() {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = ev
                    {
                        open_in_shell(&ui_url);
                    }
                }
            };

            let _ = ready_tx.send(Ok(thread_id));

            let mut msg = MSG::default();
            unsafe {
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_APP {
                        while let Ok(u) = rx.try_recv() {
                            match u {
                                Update::State(state, tooltip) => {
                                    let (icon, text) = match &state {
                                        TrayState::Idle => {
                                            (&icon_idle, texts.waiting().to_string())
                                        }
                                        TrayState::Connected {
                                            path,
                                            clients,
                                            trouble,
                                        } => (
                                            if *trouble { &icon_warn } else { &icon_on },
                                            texts.status_connected(path, *clients),
                                        ),
                                    };
                                    let _ = tray.set_icon(Some(icon.clone()));
                                    let _ = tray.set_tooltip(Some(&tooltip));
                                    status.set_text(text);
                                }
                                Update::Quit => {
                                    drop(tray);
                                    return;
                                }
                            }
                        }
                        continue;
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                    handle_events(&switching);
                }
            }
        })
        .map_err(|e| e.to_string())?;

    let thread_id = ready_rx
        .recv()
        .map_err(|_| "tray thread died".to_string())??;
    Ok(TrayHandle {
        tx,
        thread_id,
        last: Mutex::new(None),
    })
}

/// Own app id for toasts. Without a registered id they showed up as "Windows PowerShell".
const TOAST_APP_ID: &str = "GlassMic";

/// Chosen app id: our own once registration worked, otherwise PowerShell's.
static TOAST_ID: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Registers the app id under HKCU\Software\Classes\AppUserModelId\GlassMic with display name and
/// icon, the way Windows expects it for unpackaged apps without a Start menu shortcut.
/// Idempotent, no admin rights needed. The icon is written as PNG into `dir`.
pub fn register_toast_app_id(icon_png: &[u8], dir: &std::path::Path) {
    let result = (|| -> Result<(), String> {
        use windows::core::PCWSTR;
        use windows::Win32::System::Registry::{
            RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
            REG_OPTION_NON_VOLATILE, REG_SZ,
        };
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let icon = dir.join("toast-icon.png");
        std::fs::write(&icon, icon_png).map_err(|e| e.to_string())?;
        let subkey = to_wide(&format!(r"Software\Classes\AppUserModelId\{TOAST_APP_ID}"));
        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
            .ok()
            .map_err(|e| format!("RegCreateKeyExW: {e}"))?;
        }
        let set = |name: &str, value: &str| -> Result<(), String> {
            let name = to_wide(name);
            let value = to_wide(value);
            let bytes: Vec<u8> = value.iter().flat_map(|u| u.to_le_bytes()).collect();
            unsafe {
                RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(&bytes))
                    .ok()
                    .map_err(|e| format!("RegSetValueExW: {e}"))
            }
        };
        let res =
            set("DisplayName", "Glass Mic").and_then(|_| set("IconUri", &icon.to_string_lossy()));
        unsafe {
            let _ = RegCloseKey(key);
        }
        res
    })();
    match result {
        Ok(()) => {
            let _ = TOAST_ID.set(TOAST_APP_ID);
        }
        Err(e) => {
            tracing::warn!("toast app id not registered, toasts appear as PowerShell: {e}");
            let _ = TOAST_ID.set(tauri_winrt_notification::Toast::POWERSHELL_APP_ID);
        }
    }
}

/// Windows notification (toast). Blocks, so call it from spawn_blocking.
pub fn toast(title: &str, body: &str) {
    use tauri_winrt_notification::{Duration, Toast};
    let app_id = TOAST_ID.get().copied().unwrap_or(Toast::POWERSHELL_APP_ID);
    let res = Toast::new(app_id)
        .title(title)
        .text1(body)
        .duration(Duration::Short)
        .show();
    if let Err(e) = res {
        tracing::debug!("toast failed: {e}");
    }
}
