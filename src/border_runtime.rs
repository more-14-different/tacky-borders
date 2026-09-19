use anyhow::{Context, anyhow};
use std::collections::{HashMap, HashSet};
use std::ptr;
use std::sync::{LazyLock, RwLock};
use windows::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, TRUE, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, EnumWindows, GWLP_USERDATA,
    GetWindowLongPtrW, HWND_MESSAGE, KillTimer, RegisterClassExW, SetTimer, SetWindowLongPtrW,
    WM_APP, WM_CREATE, WM_NCDESTROY, WM_TIMER, WNDCLASSEXW,
};
use windows::core::{BOOL, w};

use crate::APP_STATE;
use crate::border_registry::{
    BorderLifecycleState, BorderRecord, WindowIdentity, plan_reconciliation,
};
use crate::config::EnableMode;
use crate::event_hook::handle_foreground_event;
use crate::utils::{
    LogIfErr, OwnedHWND, get_foreground_window, get_last_error, get_window_rule,
    has_filtered_style, is_window_cloaked, is_window_top_level, is_window_visible, post_message_w,
};
use crate::window_border::WindowBorder;

const WM_APP_RUNTIME_COMMAND: u32 = WM_APP + 100;
const ANIMATION_TIMER_ID: usize = 1;
const FOREGROUND_POLL_TIMER_ID: usize = 2;
const RECONCILE_TIMER_ID: usize = 3;
const FOREGROUND_POLL_INTERVAL_MS: u32 = 100;
const RECONCILE_INTERVAL_MS: u32 = 1000;
const MIN_ANIMATION_TIMER_INTERVAL_MS: u32 = 10;

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
    Create {
        identity: WindowIdentity,
    },
    Destroy {
        identity: WindowIdentity,
    },
    DestroyAll,
    MarkActive {
        identity: WindowIdentity,
    },
    SetAnimation {
        identity: WindowIdentity,
        fps: Option<u32>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnimationRegistration {
    identity: WindowIdentity,
    fps: u32,
}

#[derive(Debug, Default)]
struct BorderRuntime {
    // Box keeps every WindowBorder at a stable address because the border HWND stores a pointer to
    // the WindowBorder in GWLP_USERDATA. Moving the Box in this map does not move the allocation.
    borders: HashMap<isize, Box<WindowBorder>>,
    animated: HashMap<isize, AnimationRegistration>,
    dispatcher_hwnd: isize,
    animation_timer_interval_ms: Option<u32>,
}

pub struct BorderRuntimeHost {
    // dispatcher is deliberately dropped before _runtime so WM_NCDESTROY can still dereference the
    // runtime pointer while DestroyWindow runs from OwnedHWND::drop.
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

        runtime.dispatcher_hwnd = dispatcher.0 as isize;
        runtime.start_maintenance_timers();

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
        self._runtime.stop_all_timers();
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

fn post_runtime_command(command: BorderRuntimeCommand) {
    let handle = *RUNTIME_HANDLE.read().unwrap();
    match handle {
        Some(handle) => handle.post(command).log_if_err(),
        None => error!("border runtime is not initialized"),
    }
}

pub fn request_create_border(tracking_window: HWND) {
    let Some(identity) = WindowIdentity::capture(tracking_window) else {
        return;
    };
    post_runtime_command(BorderRuntimeCommand::Create { identity });
}

/// Compatibility entry point for callers that only have an HWND. The registry lookup captures the
/// already-known identity and the queued command carries that full identity from this point on.
pub fn request_destroy_border(tracking_window: HWND) {
    let identity = APP_STATE
        .border_registry
        .read()
        .unwrap()
        .get(tracking_window)
        .map(|record| record.tracking);

    if let Some(identity) = identity {
        request_destroy_border_identity(identity);
    }
}

pub fn request_destroy_border_identity(identity: WindowIdentity) {
    post_runtime_command(BorderRuntimeCommand::Destroy { identity });
}

pub fn request_destroy_all_borders() {
    post_runtime_command(BorderRuntimeCommand::DestroyAll);
}

pub fn request_mark_border_active(identity: WindowIdentity) {
    post_runtime_command(BorderRuntimeCommand::MarkActive { identity });
}

pub fn request_set_border_animation(identity: WindowIdentity, fps: Option<u32>) {
    post_runtime_command(BorderRuntimeCommand::SetAnimation { identity, fps });
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

        let runtime = unsafe { &mut *runtime_ptr };
        match message {
            WM_APP_RUNTIME_COMMAND => {
                let command = unsafe { Box::from_raw(lparam.0 as *mut BorderRuntimeCommand) };
                runtime.handle_command(*command);
                LRESULT(0)
            }
            WM_TIMER => {
                runtime.handle_timer(wparam.0);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                runtime.stop_all_timers();
                unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
    }

    fn handle_command(&mut self, command: BorderRuntimeCommand) {
        match command {
            BorderRuntimeCommand::Create { identity } => self.create_border(identity),
            BorderRuntimeCommand::Destroy { identity } => self.destroy_border(identity),
            BorderRuntimeCommand::DestroyAll => self.destroy_all_borders(),
            BorderRuntimeCommand::MarkActive { identity } => {
                let mut registry = APP_STATE.border_registry.write().unwrap();
                if registry
                    .get_by_key(identity.hwnd)
                    .map(|record| record.tracking)
                    == Some(identity)
                {
                    registry.set_state(identity, BorderLifecycleState::Active);
                }
            }
            BorderRuntimeCommand::SetAnimation { identity, fps } => {
                self.set_animation_registration(identity, fps)
            }
        }
    }

    fn handle_timer(&mut self, timer_id: usize) {
        match timer_id {
            ANIMATION_TIMER_ID => self.tick_animations(),
            FOREGROUND_POLL_TIMER_ID => self.reconcile_foreground(),
            RECONCILE_TIMER_ID => self.reconcile_windows().log_if_err(),
            _ => {}
        }
    }

    fn start_maintenance_timers(&self) {
        let dispatcher = HWND(self.dispatcher_hwnd as _);
        unsafe {
            SetTimer(
                Some(dispatcher),
                FOREGROUND_POLL_TIMER_ID,
                FOREGROUND_POLL_INTERVAL_MS,
                None,
            );
            SetTimer(
                Some(dispatcher),
                RECONCILE_TIMER_ID,
                RECONCILE_INTERVAL_MS,
                None,
            );
        }
    }

    fn stop_all_timers(&mut self) {
        if self.dispatcher_hwnd == 0 {
            return;
        }
        let dispatcher = HWND(self.dispatcher_hwnd as _);
        unsafe {
            KillTimer(Some(dispatcher), ANIMATION_TIMER_ID).log_if_err();
            KillTimer(Some(dispatcher), FOREGROUND_POLL_TIMER_ID).log_if_err();
            KillTimer(Some(dispatcher), RECONCILE_TIMER_ID).log_if_err();
        }
        self.animation_timer_interval_ms = None;
    }

    fn create_border(&mut self, identity: WindowIdentity) {
        // The identity was captured by the producer. Reject an HWND that was destroyed/reused while
        // the command sat in the queue.
        if !identity.still_matches() {
            return;
        }
        let tracking_window = identity.hwnd();

        let existing = {
            let registry = APP_STATE.border_registry.read().unwrap();
            registry.get(tracking_window).copied()
        };
        if let Some(existing) = existing {
            if existing.tracking == identity && self.borders.contains_key(&identity.hwnd) {
                return;
            }
            if existing.tracking == identity {
                // Registry survived but its runtime object did not. Drop the stale dispatch entry
                // and rebuild from the still-valid captured identity.
                APP_STATE
                    .border_registry
                    .write()
                    .unwrap()
                    .remove_by_key(identity.hwnd);
            } else {
                self.destroy_border(existing.tracking);
            }
        }

        if !should_create_border(tracking_window) {
            return;
        }

        let window_rule = get_window_rule(tracking_window);
        if !identity.still_matches() {
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

        // Register before init. A zero-delay init can fail/queue cleanup synchronously; cleanup must
        // be able to resolve the exact identity immediately instead of leaving an untracked HWND.
        APP_STATE
            .border_registry
            .write()
            .unwrap()
            .insert(BorderRecord {
                tracking: identity,
                border_hwnd: border_hwnd.0 as isize,
                state: BorderLifecycleState::Initializing,
            });

        if let Some(mut replaced) = self.borders.insert(identity.hwnd, border) {
            // Defensive: this should only be possible if runtime/registry state diverged.
            replaced.prepare_for_destroy();
            drop(replaced);
        }

        let init_result = self
            .borders
            .get_mut(&identity.hwnd)
            .expect("border inserted immediately above")
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

        // Stop global dispatch before DestroyWindow can synchronously re-enter a window procedure.
        self.remove_animation_registration(expected_identity);
        self.refresh_animation_timer();

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

        self.animated.clear();
        self.refresh_animation_timer();

        // Defensive cleanup for any runtime-owned object that somehow missed the registry.
        for (_, mut border) in self.borders.drain() {
            border.prepare_for_destroy();
            drop(border);
        }
    }

    fn set_animation_registration(&mut self, identity: WindowIdentity, fps: Option<u32>) {
        match fps {
            Some(fps) => {
                let current_identity = APP_STATE
                    .border_registry
                    .read()
                    .unwrap()
                    .get_by_key(identity.hwnd)
                    .map(|record| record.tracking);
                if current_identity != Some(identity) || !self.borders.contains_key(&identity.hwnd)
                {
                    return;
                }

                self.animated.insert(
                    identity.hwnd,
                    AnimationRegistration {
                        identity,
                        fps: fps.max(1),
                    },
                );
            }
            None => self.remove_animation_registration(identity),
        }
        self.refresh_animation_timer();
    }

    fn remove_animation_registration(&mut self, identity: WindowIdentity) {
        if self
            .animated
            .get(&identity.hwnd)
            .map(|registration| registration.identity)
            == Some(identity)
        {
            self.animated.remove(&identity.hwnd);
        }
    }

    fn refresh_animation_timer(&mut self) {
        if self.dispatcher_hwnd == 0 {
            return;
        }

        let desired_interval = self
            .animated
            .values()
            .map(|registration| animation_interval_ms(registration.fps))
            .min();
        if desired_interval == self.animation_timer_interval_ms {
            return;
        }

        let dispatcher = HWND(self.dispatcher_hwnd as _);
        if self.animation_timer_interval_ms.is_some() {
            unsafe { KillTimer(Some(dispatcher), ANIMATION_TIMER_ID) }.log_if_err();
        }

        if let Some(interval_ms) = desired_interval {
            unsafe { SetTimer(Some(dispatcher), ANIMATION_TIMER_ID, interval_ms, None) };
        }
        self.animation_timer_interval_ms = desired_interval;
    }

    fn tick_animations(&mut self) {
        let registrations: Vec<AnimationRegistration> = self.animated.values().copied().collect();
        let mut stale = Vec::new();

        for registration in registrations {
            let current_identity = APP_STATE
                .border_registry
                .read()
                .unwrap()
                .get_by_key(registration.identity.hwnd)
                .map(|record| record.tracking);
            if current_identity != Some(registration.identity) {
                stale.push(registration.identity);
                continue;
            }

            match self.borders.get_mut(&registration.identity.hwnd) {
                Some(border) => border.animation_tick(),
                None => stale.push(registration.identity),
            }
        }

        if !stale.is_empty() {
            for identity in stale {
                self.remove_animation_registration(identity);
            }
            self.refresh_animation_timer();
        }
    }

    fn reconcile_foreground(&self) {
        let old_active_hwnd = HWND(*APP_STATE.active_window.lock().unwrap() as _);
        let new_active_hwnd = get_foreground_window();
        if new_active_hwnd != old_active_hwnd && !new_active_hwnd.is_invalid() {
            handle_foreground_event(new_active_hwnd, old_active_hwnd);
        }
    }

    fn reconcile_runtime_invariants(&mut self) {
        // Registry entry with no runtime object: remove dispatch first so the next reconcile can
        // recreate it if the tracking identity is still present/eligible.
        let records = APP_STATE.border_registry.read().unwrap().records();
        for record in records {
            let runtime_identity = self
                .borders
                .get(&record.tracking.hwnd)
                .and_then(|border| border.tracked_identity());
            if runtime_identity != Some(record.tracking) {
                // If the HWND key exists but the runtime object belongs to another identity, remove
                // both sides and let the OS snapshot recreate the correct object below.
                self.destroy_border(record.tracking);
            }
        }

        // Runtime object with no registry entry: it is unreachable by normal dispatch, so destroy it
        // on the owning UI thread before comparing the OS snapshot.
        let registered: HashSet<isize> = APP_STATE
            .border_registry
            .read()
            .unwrap()
            .records()
            .into_iter()
            .map(|record| record.tracking.hwnd)
            .collect();
        let orphaned: Vec<isize> = self
            .borders
            .keys()
            .copied()
            .filter(|key| !registered.contains(key))
            .collect();
        for key in orphaned {
            self.animated.remove(&key);
            if let Some(mut border) = self.borders.remove(&key) {
                border.prepare_for_destroy();
                drop(border);
            }
        }
        self.refresh_animation_timer();
    }

    fn reconcile_windows(&mut self) -> anyhow::Result<()> {
        self.reconcile_runtime_invariants();

        let snapshot = WindowSnapshot::collect()?;
        let current = APP_STATE.border_registry.read().unwrap().records();
        let plan = plan_reconciliation(&current, &snapshot.present, &snapshot.creatable);

        // Destroy first. This is required for the HWND-reuse case where one numeric HWND maps to a
        // different PID/TID in the same snapshot.
        for identity in plan.destroy {
            self.destroy_border(identity);
        }
        for identity in plan.create {
            self.create_border(identity);
        }

        Ok(())
    }
}

fn animation_interval_ms(fps: u32) -> u32 {
    (1000 / fps.max(1)).max(MIN_ANIMATION_TIMER_INTERVAL_MS)
}

fn should_create_border(hwnd: HWND) -> bool {
    if !is_window_top_level(hwnd) || !is_window_visible(hwnd) || is_window_cloaked(hwnd) {
        return false;
    }

    let window_rule = get_window_rule(hwnd);
    if window_rule.enabled == Some(EnableMode::Bool(false)) {
        return false;
    }
    window_rule.enabled == Some(EnableMode::Bool(true)) || !has_filtered_style(hwnd)
}

#[derive(Debug, Default)]
struct WindowSnapshot {
    // All currently-present top-level identities. Hidden/minimized/cloaked windows remain here so
    // they are not churned out of the registry merely because their border is temporarily hidden.
    present: HashMap<isize, WindowIdentity>,
    // Subset that is eligible for a newly-created border right now.
    creatable: HashSet<isize>,
}

impl WindowSnapshot {
    fn collect() -> anyhow::Result<Self> {
        let mut snapshot = Self::default();
        unsafe {
            EnumWindows(
                Some(enum_windows_snapshot_callback),
                LPARAM(ptr::addr_of_mut!(snapshot) as isize),
            )
        }
        .context("could not enumerate windows for border reconciliation")?;
        Ok(snapshot)
    }
}

unsafe extern "system" fn enum_windows_snapshot_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if !is_window_top_level(hwnd) {
        return TRUE;
    }

    let Some(identity) = WindowIdentity::capture(hwnd) else {
        return TRUE;
    };

    let snapshot = unsafe { &mut *(lparam.0 as *mut WindowSnapshot) };
    snapshot.present.insert(identity.hwnd, identity);
    if should_create_border(hwnd) {
        snapshot.creatable.insert(identity.hwnd);
    }

    TRUE
}

#[cfg(test)]
mod tests {
    use super::animation_interval_ms;

    #[test]
    fn animation_interval_tracks_fastest_reasonable_timer_rate() {
        assert_eq!(animation_interval_ms(60), 16);
        assert_eq!(animation_interval_ms(30), 33);
        assert_eq!(animation_interval_ms(120), 10); // SetTimer is not useful below ~10 ms here.
        assert_eq!(animation_interval_ms(0), 1000);
    }
}
