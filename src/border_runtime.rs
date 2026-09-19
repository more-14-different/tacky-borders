use anyhow::{Context, anyhow};
use std::collections::HashMap;
use std::ptr;
use std::sync::{LazyLock, RwLock};
use windows::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, GWLP_USERDATA,
    GetWindowLongPtrW, HWND_MESSAGE, RegisterClassExW, SetWindowLongPtrW, WM_APP, WM_CREATE,
    WM_NCDESTROY, WNDCLASSEXW,
};
use windows::core::w;

use crate::APP_STATE;
use crate::border_registry::{BorderLifecycleState, BorderRecord, WindowIdentity};
use crate::config::EnableMode;
use crate::utils::{
    OwnedHWND, get_last_error, get_window_rule, has_filtered_style, is_window_cloaked,
    is_window_top_level, is_window_visible, post_message_w,
};
use crate::window_border::WindowBorder;

const WM_APP_RUNTIME_COMMAND: u32 = WM_APP + 100;

static RUNTIME_HANDLE: LazyLock<RwLock<Option<BorderRuntimeHandle>>> =
    LazyLock::new(|| RwLock::new(None));

#[derive(Debug, Clone, Copy)]
pub struct BorderRuntimeHandle {
    dispatcher_hwnd: isize,
}

impl BorderRuntimeHandle {
    fn post(self, command: BorderRuntimeCommand) -> anyhow::Result<()> {
        let command_ptr = Box::into_raw(Box::new(command));
        let result = post_message_w(
            Some(HWND(self.dispatcher_hwnd as _)),
            WM_APP_RUNTIME_COMMAND,
            WPARAM(0),
            LPARAM(command_ptr as isize),
        );

        if let Err(err) = result {
            // PostMessage failed, so ownership never reached the runtime window.
            drop(unsafe { Box::from_raw(command_ptr) });
            return Err(err).context("could not post command to border runtime");
        }

        Ok(())
    }
}

#[derive(Debug)]
enum BorderRuntimeCommand {
    Create { tracking_hwnd: isize },
    Destroy { identity: WindowIdentity },
    DestroyAll,
    MarkActive { identity: WindowIdentity },
}

#[derive(Debug, Default)]
struct BorderRuntime {
    // Box keeps every WindowBorder at a stable address because the border HWND stores a pointer to
    // the WindowBorder in GWLP_USERDATA. Moving the Box in this map does not move the allocation.
    borders: HashMap<isize, Box<WindowBorder>>,
}

pub struct BorderRuntimeHost {
    // Destroy the dispatcher before the runtime object. Field drop order matters: DestroyWindow
    // may synchronously deliver WM_NCDESTROY and access the runtime pointer.
    _dispatcher: OwnedHWND,
    _runtime: Box<BorderRuntime>,
}

impl BorderRuntimeHost {
    pub fn new() -> anyhow::Result<Self> {
        register_runtime_window_class()?;

        let mut runtime = Box::<BorderRuntime>::default();
        let dispatcher = unsafe {
            CreateWindowExW(
                Default::default(),
                w!("tacky-borders-runtime"),
                w!("tacky-borders-runtime"),
                Default::default(),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                Some(HWND_MESSAGE),
                None,
                None,
                Some(ptr::addr_of_mut!(*runtime) as _),
            )
        }
        .context("could not create border runtime dispatcher window")?;

        let handle = BorderRuntimeHandle {
            dispatcher_hwnd: dispatcher.0 as isize,
        };
        *RUNTIME_HANDLE.write().unwrap() = Some(handle);

        Ok(Self {
            _dispatcher: OwnedHWND(dispatcher),
            _runtime: runtime,
        })
    }
}

impl Drop for BorderRuntimeHost {
    fn drop(&mut self) {
        *RUNTIME_HANDLE.write().unwrap() = None;
    }
}

fn register_runtime_window_class() -> anyhow::Result<()> {
    unsafe {
        let window_class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(BorderRuntime::s_wnd_proc),
            hInstance: GetModuleHandleW(None)?.into(),
            lpszClassName: w!("tacky-borders-runtime"),
            ..Default::default()
        };

        let result = RegisterClassExW(&window_class);
        if result == 0 {
            let last_error = get_last_error();
            if last_error != ERROR_CLASS_ALREADY_EXISTS {
                return Err(anyhow!(
                    "could not register border runtime window class: {last_error:?}"
                ));
            }
        }
    }

    Ok(())
}

fn post_runtime_command(command: BorderRuntimeCommand) -> bool {
    let handle = *RUNTIME_HANDLE.read().unwrap();
    match handle {
        Some(handle) => match handle.post(command) {
            Ok(()) => true,
            Err(err) => {
                error!("{err:#}");
                false
            }
        },
        None => {
            error!("border runtime is not initialized");
            false
        }
    }
}

pub fn request_create_border(tracking_window: HWND) {
    let _ = post_runtime_command(BorderRuntimeCommand::Create {
        tracking_hwnd: tracking_window.0 as isize,
    });
}

pub fn request_destroy_border(identity: WindowIdentity) -> bool {
    post_runtime_command(BorderRuntimeCommand::Destroy { identity })
}

pub fn request_destroy_all_borders() {
    let _ = post_runtime_command(BorderRuntimeCommand::DestroyAll);
}

pub fn request_mark_border_active(identity: WindowIdentity) {
    let _ = post_runtime_command(BorderRuntimeCommand::MarkActive { identity });
}

impl BorderRuntime {
    unsafe extern "system" fn s_wnd_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let mut runtime_ptr =
            unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut BorderRuntime;

        if runtime_ptr.is_null() && message == WM_CREATE {
            let create_struct = lparam.0 as *mut CREATESTRUCTW;
            runtime_ptr = unsafe { (*create_struct).lpCreateParams } as *mut BorderRuntime;
            unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, runtime_ptr as _) };
        }

        if runtime_ptr.is_null() {
            return unsafe { DefWindowProcW(window, message, wparam, lparam) };
        }

        match message {
            WM_APP_RUNTIME_COMMAND => {
                if lparam.0 == 0 {
                    return LRESULT(0);
                }
                let command = unsafe { Box::from_raw(lparam.0 as *mut BorderRuntimeCommand) };
                unsafe { &mut *runtime_ptr }.handle_command(*command);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
    }

    fn handle_command(&mut self, command: BorderRuntimeCommand) {
        match command {
            BorderRuntimeCommand::Create { tracking_hwnd } => {
                self.create_border(HWND(tracking_hwnd as _));
            }
            BorderRuntimeCommand::Destroy { identity } => self.destroy_border(identity),
            BorderRuntimeCommand::DestroyAll => self.destroy_all_borders(),
            BorderRuntimeCommand::MarkActive { identity } => {
                APP_STATE
                    .border_registry
                    .write()
                    .unwrap()
                    .set_state(identity, BorderLifecycleState::Active);
            }
        }
    }

    fn create_border(&mut self, tracking_window: HWND) {
        let Some(identity) = WindowIdentity::capture(tracking_window) else {
            return;
        };

        if let Some(existing) = APP_STATE
            .border_registry
            .read()
            .unwrap()
            .get(tracking_window)
            .copied()
        {
            if existing.tracking == identity {
                return;
            }

            // HWND has been reused. Remove the stale dispatch entry before destroying its HWND.
            self.destroy_border(existing.tracking);
        }

        if !is_window_top_level(tracking_window)
            || !is_window_visible(tracking_window)
            || is_window_cloaked(tracking_window)
        {
            return;
        }

        let window_rule = get_window_rule(tracking_window);
        if window_rule.enabled == Some(EnableMode::Bool(false)) {
            info!("border is disabled for {tracking_window:?}");
            return;
        }
        if window_rule.enabled != Some(EnableMode::Bool(true))
            && has_filtered_style(tracking_window)
        {
            return;
        }

        debug!("creating border on runtime thread for: {tracking_window:?}");
        let border = match WindowBorder::new_tracked(tracking_window, identity) {
            Ok(border) => border,
            Err(err) => {
                error!("could not create window border for {tracking_window:?}: {err:#}");
                return;
            }
        };
        let border_hwnd = border.border_window.0;
        APP_STATE
            .border_registry
            .write()
            .unwrap()
            .insert(BorderRecord {
                tracking: identity,
                border_hwnd: border_hwnd.0 as isize,
                state: BorderLifecycleState::Initializing,
            });
        self.borders.insert(identity.hwnd, border);

        let init_result = self
            .borders
            .get_mut(&identity.hwnd)
            .expect("newly inserted border must remain runtime-owned")
            .init(window_rule);
        if let Err(err) = init_result {
            error!("could not initialize border for {tracking_window:?}: {err:#}");
            self.destroy_border(identity);
        }
    }

    fn destroy_border(&mut self, expected_identity: WindowIdentity) {
        let current = APP_STATE
            .border_registry
            .read()
            .unwrap()
            .get_by_key(expected_identity.hwnd)
            .copied();

        // A delayed destroy event for an old HWND must never destroy a new window that reused the
        // same numeric handle.
        if current.map(|record| record.tracking) != Some(expected_identity) {
            return;
        }

        // This order is intentional: stop future dispatch before DestroyWindow can synchronously
        // re-enter a window procedure or before another producer sees the stale entry.
        if let Some(record) = APP_STATE
            .border_registry
            .write()
            .unwrap()
            .remove_by_key(expected_identity.hwnd)
        {
            debug!(
                "removing border registry entry: tracking={:?}, border={:?}, pid={}, tid={}",
                record.tracking.hwnd(),
                record.border_hwnd(),
                record.tracking.process_id,
                record.tracking.thread_id
            );
        }

        if let Some(mut border) = self.borders.remove(&expected_identity.hwnd) {
            border.prepare_for_destroy();
            // OwnedHWND::drop calls DestroyWindow. We are on the same UI thread that created it.
            drop(border);
        }
    }

    fn destroy_all_borders(&mut self) {
        let identities: Vec<WindowIdentity> = APP_STATE
            .border_registry
            .read()
            .unwrap()
            .records()
            .into_iter()
            .map(|record| record.tracking)
            .collect();
        for identity in identities {
            self.destroy_border(identity);
        }

        // Defensive cleanup for any runtime-owned object that somehow missed the registry.
        for (_, mut border) in self.borders.drain() {
            border.prepare_for_destroy();
            drop(border);
        }
    }
}
