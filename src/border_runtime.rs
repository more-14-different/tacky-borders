use anyhow::{Context, anyhow};
use std::collections::{HashMap, HashSet};
use std::ptr;
use std::sync::{
    LazyLock, Mutex, RwLock,
    atomic::{AtomicBool, AtomicU8, Ordering},
    mpsc::{SyncSender, sync_channel},
};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    D2DERR_RECREATE_TARGET, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, EnumWindows, GWLP_USERDATA,
    GetWindowLongPtrW, HWND_MESSAGE, KillTimer, RegisterClassExW, SetTimer, SetWindowLongPtrW,
    WM_APP, WM_CREATE, WM_NCDESTROY, WM_TIMER, WNDCLASSEXW,
};
use windows::core::{BOOL, HRESULT, w};

use crate::border_registry::{
    BorderLifecycleState, BorderRecord, BorderRegistry, WindowIdentity, plan_reconciliation,
};
use crate::colors::ColorBrushConfig;
use crate::config::{EnableMode, OffsetConfig, RadiusConfig, WidthConfig};
use crate::render_backend::RenderBackendConfig;
use crate::utils::{
    LogIfErr, OwnedHWND, WindowsCompatibleResult, get_foreground_window, get_last_error,
    get_window_rule, has_filtered_style, is_current_process_elevated, is_process_elevated,
    is_window_cloaked, is_window_top_level, is_window_visible, post_message_w,
};
use crate::window_border::WindowBorder;
use crate::{APP_STATE, DirectXDevices};

const WM_APP_RUNTIME_COMMAND: u32 = WM_APP + 100;
const ANIMATION_TIMER_ID: usize = 1;
const FOREGROUND_POLL_TIMER_ID: usize = 2;
const RECONCILE_TIMER_ID: usize = 3;
const REORDER_TIMER_ID: usize = 4;
const LOCATION_TIMER_ID: usize = 5;
const FOREGROUND_POLL_INTERVAL_MS: u32 = 100;
const RECONCILE_INTERVAL_MS: u32 = 1000;
const MIN_ANIMATION_TIMER_INTERVAL_MS: u32 = 10;
const REORDER_DEBOUNCE_INTERVAL_MS: u64 = 16;
const LOCATION_COALESCE_INTERVAL_MS: u64 = 16;

static RUNTIME_HANDLE: LazyLock<RwLock<Option<BorderRuntimeHandle>>> =
    LazyLock::new(|| RwLock::new(None));
static REORDER_COMMAND_PENDING: AtomicBool = AtomicBool::new(false);
static LOCATION_COMMAND_GATE: LazyLock<Mutex<IdentityCommandGate>> =
    LazyLock::new(|| Mutex::new(IdentityCommandGate::default()));
const GRAPHICS_REFRESH_IDLE: u8 = 0;
const GRAPHICS_REFRESH_PENDING: u8 = 1;
const GRAPHICS_REFRESH_DIRTY: u8 = 2;
static GRAPHICS_REFRESH_STATE: AtomicU8 = AtomicU8::new(GRAPHICS_REFRESH_IDLE);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderWindowEvent {
    LocationChange,
    ShowUncloaked,
    HideCloaked,
    MinimizeStart,
    MinimizeEnd,
}

#[derive(Debug)]
pub enum BorderRuntimeUpdate {
    Colors {
        active: Option<ColorBrushConfig>,
        inactive: Option<ColorBrushConfig>,
    },
    Width(WidthConfig),
    Offset(OffsetConfig),
    Radius(RadiusConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BorderRuntimeSnapshot {
    pub active_window: isize,
    pub border_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphicsRecoveryReason {
    RenderTargetLost,
    DeviceLost,
}

enum BorderRuntimeCommand {
    Create {
        identity: WindowIdentity,
    },
    Destroy {
        identity: WindowIdentity,
    },
    DestroyObserved {
        hwnd: isize,
    },
    DestroyAll,
    MarkActive {
        identity: WindowIdentity,
    },
    SetAnimation {
        identity: WindowIdentity,
        fps: Option<u32>,
    },
    WindowEvent {
        identity: WindowIdentity,
        event: BorderWindowEvent,
    },
    Reorder,
    Foreground {
        tracking_hwnd: isize,
    },
    RefreshGraphics,
    ForceRecreateDrawers {
        exclude: Option<WindowIdentity>,
    },
    ReloadBorders,
    KomorebiRefresh {
        tracking_hwnds: Vec<isize>,
    },
    ApplyUpdate {
        update: BorderRuntimeUpdate,
        focused_only: bool,
    },
    Snapshot {
        reply: SyncSender<BorderRuntimeSnapshot>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnimationRegistration {
    identity: WindowIdentity,
    fps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReorderAction {
    FlushNow,
    ArmTimer(Duration),
    Coalesced,
}

#[derive(Debug, Default)]
struct ReorderCoalescer {
    last_flush: Option<Instant>,
    timer_armed: bool,
    pending: bool,
}

impl ReorderCoalescer {
    fn on_event(&mut self, now: Instant) -> ReorderAction {
        if self.timer_armed {
            self.pending = true;
            return ReorderAction::Coalesced;
        }

        if let Some(last_flush) = self.last_flush {
            let interval = Duration::from_millis(REORDER_DEBOUNCE_INTERVAL_MS);
            let elapsed = now.saturating_duration_since(last_flush);
            if elapsed < interval {
                self.pending = true;
                self.timer_armed = true;
                return ReorderAction::ArmTimer(interval - elapsed);
            }
        }

        self.last_flush = Some(now);
        ReorderAction::FlushNow
    }

    fn on_timer(&mut self, now: Instant) -> bool {
        self.timer_armed = false;
        if !self.pending {
            return false;
        }

        self.pending = false;
        self.last_flush = Some(now);
        true
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Default)]
struct IdentityCommandGate {
    pending: HashSet<WindowIdentity>,
}

impl IdentityCommandGate {
    fn acquire(&mut self, identity: WindowIdentity) -> bool {
        self.pending.insert(identity)
    }

    fn release(&mut self, identity: WindowIdentity) {
        self.pending.remove(&identity);
    }

    fn clear(&mut self) {
        self.pending.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocationAction {
    FlushNow,
    ArmTimer(Duration),
    Coalesced,
}

#[derive(Debug, Default)]
struct LocationCoalescer {
    last_flush: Option<Instant>,
    timer_armed: bool,
    dirty: HashSet<WindowIdentity>,
}

impl LocationCoalescer {
    fn on_event(&mut self, identity: WindowIdentity, now: Instant) -> LocationAction {
        if self.timer_armed {
            self.dirty.insert(identity);
            return LocationAction::Coalesced;
        }

        if let Some(last_flush) = self.last_flush {
            let interval = Duration::from_millis(LOCATION_COALESCE_INTERVAL_MS);
            let elapsed = now.saturating_duration_since(last_flush);
            if elapsed < interval {
                self.dirty.insert(identity);
                self.timer_armed = true;
                return LocationAction::ArmTimer(interval - elapsed);
            }
        }

        self.last_flush = Some(now);
        LocationAction::FlushNow
    }

    fn on_timer(&mut self, now: Instant) -> Vec<WindowIdentity> {
        self.timer_armed = false;
        if self.dirty.is_empty() {
            return Vec::new();
        }

        self.last_flush = Some(now);
        let mut dirty: Vec<WindowIdentity> = self.dirty.drain().collect();
        dirty.sort_by_key(|identity| (identity.hwnd, identity.process_id, identity.thread_id));
        dirty
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Default)]
struct BorderRuntime {
    registry: BorderRegistry,
    active_window: isize,
    current_process_elevated: bool,
    reorder: ReorderCoalescer,
    location: LocationCoalescer,
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

        let current_process_elevated = is_current_process_elevated()?;
        debug!("border runtime elevated: {current_process_elevated}");
        REORDER_COMMAND_PENDING.store(false, Ordering::Release);
        LOCATION_COMMAND_GATE.lock().unwrap().clear();
        GRAPHICS_REFRESH_STATE.store(GRAPHICS_REFRESH_IDLE, Ordering::Release);

        let mut runtime = Box::new(BorderRuntime {
            active_window: get_foreground_window().0 as isize,
            current_process_elevated,
            ..Default::default()
        });
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

fn runtime_handle() -> anyhow::Result<BorderRuntimeHandle> {
    RUNTIME_HANDLE
        .read()
        .unwrap()
        .as_ref()
        .copied()
        .context("border runtime is not initialized")
}

fn post_runtime_command(command: BorderRuntimeCommand) -> anyhow::Result<()> {
    runtime_handle()?.post(command)
}

fn post_runtime_command_logged(command: BorderRuntimeCommand) {
    post_runtime_command(command).log_if_err();
}

pub fn request_create_border(tracking_window: HWND) {
    let Some(identity) = WindowIdentity::capture(tracking_window) else {
        return;
    };
    post_runtime_command_logged(BorderRuntimeCommand::Create { identity });
}

/// Handles EVENT_OBJECT_DESTROY without exposing the registry to the WinEvent callback.
///
/// The runtime resolves the currently registered identity for this numeric HWND, then only destroys
/// it if that identity no longer matches the live OS window. A delayed destroy event therefore
/// cannot delete a replacement window that already reused the same HWND with a different PID/TID.
pub fn request_destroy_observed_window(tracking_window: HWND) {
    post_runtime_command_logged(BorderRuntimeCommand::DestroyObserved {
        hwnd: tracking_window.0 as isize,
    });
}

pub fn request_destroy_border_identity(identity: WindowIdentity) {
    post_runtime_command_logged(BorderRuntimeCommand::Destroy { identity });
}

pub fn request_destroy_all_borders() {
    post_runtime_command_logged(BorderRuntimeCommand::DestroyAll);
}

pub fn request_mark_border_active(identity: WindowIdentity) {
    post_runtime_command_logged(BorderRuntimeCommand::MarkActive { identity });
}

pub fn request_set_border_animation(identity: WindowIdentity, fps: Option<u32>) {
    post_runtime_command_logged(BorderRuntimeCommand::SetAnimation { identity, fps });
}

pub fn request_window_event(tracking_window: HWND, event: BorderWindowEvent) {
    let Some(identity) = WindowIdentity::capture(tracking_window) else {
        return;
    };

    if event == BorderWindowEvent::LocationChange {
        // LOCATIONCHANGE can arrive much faster than the runtime can usefully redraw. Keep at most
        // one queued command per full identity; the runtime holds this gate through any trailing
        // coalescing interval so HWND reuse cannot merge unrelated windows.
        if !LOCATION_COMMAND_GATE.lock().unwrap().acquire(identity) {
            return;
        }

        if let Err(err) =
            post_runtime_command(BorderRuntimeCommand::WindowEvent { identity, event })
        {
            LOCATION_COMMAND_GATE.lock().unwrap().release(identity);
            error!("could not post location-change command to border runtime: {err:#}");
        }
        return;
    }

    post_runtime_command_logged(BorderRuntimeCommand::WindowEvent { identity, event });
}

pub fn request_reorder_borders() {
    // EVENT_OBJECT_REORDER can arrive in bursts. Keep at most one Reorder command queued for the
    // runtime; the runtime performs its own 16 ms trailing coalescing after delivery.
    if REORDER_COMMAND_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }

    if let Err(err) = post_runtime_command(BorderRuntimeCommand::Reorder) {
        REORDER_COMMAND_PENDING.store(false, Ordering::Release);
        error!("could not post reorder command to border runtime: {err:#}");
    }
}

pub fn request_foreground_change(best_hwnd_guess: HWND, other_hwnd_guess: HWND) {
    let tracking_window = if !best_hwnd_guess.is_invalid() {
        best_hwnd_guess
    } else {
        other_hwnd_guess
    };
    if tracking_window.is_invalid() {
        return;
    }

    post_runtime_command_logged(BorderRuntimeCommand::Foreground {
        tracking_hwnd: tracking_window.0 as isize,
    });
}

pub fn request_graphics_refresh() {
    if !mark_graphics_refresh_requested(&GRAPHICS_REFRESH_STATE) {
        return;
    }

    if let Err(err) = post_runtime_command(BorderRuntimeCommand::RefreshGraphics) {
        GRAPHICS_REFRESH_STATE.store(GRAPHICS_REFRESH_IDLE, Ordering::Release);
        error!("could not post graphics refresh command to border runtime: {err:#}");
    }
}

pub(crate) fn request_force_recreate_drawers(exclude: Option<WindowIdentity>) {
    post_runtime_command_logged(BorderRuntimeCommand::ForceRecreateDrawers { exclude });
}

pub fn request_reload_borders() {
    post_runtime_command_logged(BorderRuntimeCommand::ReloadBorders);
}

/// Synchronous render-error recovery used by WindowBorder while already executing on the
/// runtime/UI thread. Target loss only needs the local drawer rebuilt; device loss must
/// replace the shared DirectX device even when the adapter LUID did not change.
pub(crate) fn recover_directx_devices_for_render_error(
    reason: GraphicsRecoveryReason,
) -> WindowsCompatibleResult<()> {
    match reason {
        GraphicsRecoveryReason::RenderTargetLost => Ok(()),
        GraphicsRecoveryReason::DeviceLost => force_recreate_directx_devices_with_config(),
    }
}

pub fn request_komorebi_refresh(mut tracking_hwnds: Vec<isize>) {
    if tracking_hwnds.is_empty() {
        return;
    }
    tracking_hwnds.sort_unstable();
    tracking_hwnds.dedup();
    post_runtime_command_logged(BorderRuntimeCommand::KomorebiRefresh { tracking_hwnds });
}

pub fn request_apply_border_update(update: BorderRuntimeUpdate, focused_only: bool) {
    post_runtime_command_logged(BorderRuntimeCommand::ApplyUpdate {
        update,
        focused_only,
    });
}

pub fn request_runtime_snapshot() -> anyhow::Result<BorderRuntimeSnapshot> {
    let (reply, response) = sync_channel(1);
    post_runtime_command(BorderRuntimeCommand::Snapshot { reply })?;
    response
        .recv_timeout(Duration::from_secs(2))
        .context("timed out waiting for border runtime snapshot")
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
            BorderRuntimeCommand::DestroyObserved { hwnd } => self.destroy_observed(hwnd),
            BorderRuntimeCommand::DestroyAll => self.destroy_all_borders(),
            BorderRuntimeCommand::MarkActive { identity } => {
                self.registry
                    .set_state(identity, BorderLifecycleState::Active);
            }
            BorderRuntimeCommand::SetAnimation { identity, fps } => {
                self.set_animation_registration(identity, fps)
            }
            BorderRuntimeCommand::WindowEvent { identity, event } => {
                if event == BorderWindowEvent::LocationChange {
                    self.handle_location_request(identity);
                } else {
                    self.handle_window_event(identity, event);
                }
            }
            BorderRuntimeCommand::Reorder => self.handle_reorder_request(),
            BorderRuntimeCommand::Foreground { tracking_hwnd } => {
                self.update_foreground(HWND(tracking_hwnd as _))
            }
            BorderRuntimeCommand::RefreshGraphics => self.handle_graphics_refresh(),
            BorderRuntimeCommand::ForceRecreateDrawers { exclude } => {
                self.force_recreate_drawers(exclude)
            }
            BorderRuntimeCommand::ReloadBorders => self.reload_borders(),
            BorderRuntimeCommand::KomorebiRefresh { tracking_hwnds } => {
                self.refresh_komorebi(tracking_hwnds)
            }
            BorderRuntimeCommand::ApplyUpdate {
                update,
                focused_only,
            } => self.apply_border_update(update, focused_only),
            BorderRuntimeCommand::Snapshot { reply } => {
                let _ = reply.send(self.snapshot());
            }
        }
    }

    fn handle_location_request(&mut self, identity: WindowIdentity) {
        match self.location.on_event(identity, Instant::now()) {
            LocationAction::FlushNow => {
                LOCATION_COMMAND_GATE.lock().unwrap().release(identity);
                self.handle_window_event(identity, BorderWindowEvent::LocationChange);
            }
            LocationAction::ArmTimer(delay) => self.arm_location_timer(delay),
            LocationAction::Coalesced => {}
        }
    }

    fn arm_location_timer(&mut self, delay: Duration) {
        if self.dispatcher_hwnd == 0 {
            self.location.reset();
            LOCATION_COMMAND_GATE.lock().unwrap().clear();
            return;
        }

        let delay_ms = delay.as_millis().clamp(1, u32::MAX as u128) as u32;
        let timer = unsafe {
            SetTimer(
                Some(HWND(self.dispatcher_hwnd as _)),
                LOCATION_TIMER_ID,
                delay_ms,
                None,
            )
        };
        if timer == 0 {
            error!("could not arm border runtime location timer; flushing immediately");
            let pending = self.location.on_timer(Instant::now());
            self.flush_location_identities(pending);
        }
    }

    fn handle_location_timer(&mut self) {
        if self.dispatcher_hwnd == 0 {
            self.location.reset();
            LOCATION_COMMAND_GATE.lock().unwrap().clear();
            return;
        }

        unsafe { KillTimer(Some(HWND(self.dispatcher_hwnd as _)), LOCATION_TIMER_ID) }.log_if_err();
        let pending = self.location.on_timer(Instant::now());
        self.flush_location_identities(pending);
    }

    fn flush_location_identities(&mut self, identities: Vec<WindowIdentity>) {
        for identity in identities {
            // Release before the direct call so a real move arriving during expensive rendering can
            // queue one trailing update instead of being lost behind the current dispatch.
            LOCATION_COMMAND_GATE.lock().unwrap().release(identity);
            self.handle_window_event(identity, BorderWindowEvent::LocationChange);
        }
    }

    fn reset_location_coalescing(&mut self) {
        if self.dispatcher_hwnd != 0 {
            unsafe { KillTimer(Some(HWND(self.dispatcher_hwnd as _)), LOCATION_TIMER_ID) }
                .log_if_err();
        }
        self.location.reset();
        LOCATION_COMMAND_GATE.lock().unwrap().clear();
    }

    fn handle_window_event(&mut self, identity: WindowIdentity, event: BorderWindowEvent) {
        // The producer captured the identity before posting. If the numeric HWND was destroyed or
        // reused while the command waited in the queue, do not dispatch the stale event.
        if !identity.still_matches() {
            return;
        }

        match self.registry.get_by_key(identity.hwnd).copied() {
            Some(record) if record.tracking == identity => {
                let Some(border) = self.borders.get_mut(&identity.hwnd) else {
                    // Registry/runtime divergence: remove dispatch first and let SHOW or periodic
                    // reconciliation reconstruct the runtime-owned object.
                    self.destroy_border(record.tracking);
                    if event == BorderWindowEvent::ShowUncloaked {
                        self.create_border(identity);
                    }
                    return;
                };

                match event {
                    BorderWindowEvent::LocationChange => border.handle_location_change(),
                    BorderWindowEvent::ShowUncloaked => border.handle_show_uncloaked(),
                    BorderWindowEvent::HideCloaked => border.handle_hide_cloaked(),
                    BorderWindowEvent::MinimizeStart => border.handle_minimize_start(),
                    BorderWindowEvent::MinimizeEnd => border.handle_minimize_end(),
                }
            }
            Some(record) => {
                // Same numeric HWND now represents a different identity. Remove the old dispatch
                // entry first. SHOW/UNCLOAK can immediately create the replacement; other events
                // can wait for SHOW or the periodic reconciliation pass.
                self.destroy_border(record.tracking);
                if event == BorderWindowEvent::ShowUncloaked {
                    self.create_border(identity);
                }
            }
            None if event == BorderWindowEvent::ShowUncloaked => self.create_border(identity),
            None => {}
        }
    }

    fn destroy_observed(&mut self, hwnd: isize) {
        let Some(record) = self.registry.get_by_key(hwnd).copied() else {
            return;
        };

        // EVENT_OBJECT_DESTROY is asynchronous. If the recorded identity still describes a live
        // window, this may be a late event for an older window that already reused the same HWND.
        if record.tracking.still_matches() {
            return;
        }
        self.destroy_border(record.tracking);
    }

    fn handle_reorder_request(&mut self) {
        match self.reorder.on_event(Instant::now()) {
            ReorderAction::FlushNow => {
                self.flush_reorder_borders();
                REORDER_COMMAND_PENDING.store(false, Ordering::Release);
            }
            ReorderAction::ArmTimer(delay) => self.arm_reorder_timer(delay),
            ReorderAction::Coalesced => {}
        }
    }

    fn arm_reorder_timer(&mut self, delay: Duration) {
        if self.dispatcher_hwnd == 0 {
            self.reorder.reset();
            REORDER_COMMAND_PENDING.store(false, Ordering::Release);
            return;
        }

        let delay_ms = delay.as_millis().clamp(1, u32::MAX as u128) as u32;
        let timer = unsafe {
            SetTimer(
                Some(HWND(self.dispatcher_hwnd as _)),
                REORDER_TIMER_ID,
                delay_ms,
                None,
            )
        };
        if timer == 0 {
            error!("could not arm border runtime reorder timer; flushing immediately");
            self.reorder.reset();
            self.flush_reorder_borders();
            REORDER_COMMAND_PENDING.store(false, Ordering::Release);
        }
    }

    fn handle_reorder_timer(&mut self) {
        if self.dispatcher_hwnd == 0 {
            return;
        }

        unsafe { KillTimer(Some(HWND(self.dispatcher_hwnd as _)), REORDER_TIMER_ID) }.log_if_err();
        if self.reorder.on_timer(Instant::now()) {
            self.flush_reorder_borders();
        }
        REORDER_COMMAND_PENDING.store(false, Ordering::Release);
    }

    fn flush_reorder_borders(&mut self) {
        // Registry records are copied first so a border can queue its own cleanup without holding
        // a registry borrow across the direct WindowBorder call.
        let records = self.registry.records();
        for record in records {
            let Some(border) = self.borders.get_mut(&record.tracking.hwnd) else {
                continue;
            };
            if border.tracked_identity() != Some(record.tracking)
                || !is_window_visible(border.border_window.0)
            {
                continue;
            }

            border.handle_reorder();
        }
    }

    fn update_foreground(&mut self, new_active_hwnd: HWND) {
        if new_active_hwnd.is_invalid() {
            return;
        }

        let old_active_hwnd = HWND(self.active_window as _);
        if old_active_hwnd == new_active_hwnd {
            return;
        }
        self.active_window = new_active_hwnd.0 as isize;

        for (tracking_window, is_active) in [(old_active_hwnd, false), (new_active_hwnd, true)] {
            if tracking_window.is_invalid() {
                continue;
            }
            if let Some(border) = self.borders.get_mut(&(tracking_window.0 as isize)) {
                border.handle_foreground_change(is_active);
            }
        }
    }

    fn recreate_drawers(&mut self) {
        let tracking_windows: Vec<isize> = self
            .registry
            .records()
            .into_iter()
            .map(|record| record.tracking.hwnd)
            .collect();
        for tracking in tracking_windows {
            if let Some(border) = self.borders.get_mut(&tracking) {
                border.handle_recreate_drawer();
            }
        }
    }

    fn force_recreate_drawers(&mut self, exclude: Option<WindowIdentity>) {
        let records = self.registry.records();
        for record in records {
            if record.state != BorderLifecycleState::Active || Some(record.tracking) == exclude {
                continue;
            }
            if let Some(border) = self.borders.get_mut(&record.tracking.hwnd) {
                border.handle_force_recreate_drawer();
            }
        }
    }

    fn handle_graphics_refresh(&mut self) {
        // Dirty requests observed before this command starts are consumed by this refresh. Requests
        // arriving while the refresh is running promote the state back to DIRTY and schedule one
        // trailing refresh after this pass completes.
        begin_graphics_refresh(&GRAPHICS_REFRESH_STATE);

        if let Err(err) = sync_directx_devices_with_config() {
            error!("could not synchronize DirectX devices on runtime thread: {err:#}");
        } else {
            self.recreate_drawers();
        }

        if finish_graphics_refresh(&GRAPHICS_REFRESH_STATE) {
            request_graphics_refresh();
        }
    }

    fn reload_borders(&mut self) {
        // Destroy old render resources before changing the shared device set. This avoids a window
        // where old V2 borders are still live while config reload has already switched devices or
        // disabled the V2 backend.
        self.destroy_all_borders();
        APP_STATE.initial_windows.lock().unwrap().clear();

        if let Err(err) = sync_directx_devices_with_config() {
            error!("could not synchronize DirectX devices while reloading borders: {err:#}");
            return;
        }

        let snapshot = match WindowSnapshot::collect(self.current_process_elevated) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                error!("could not enumerate windows while reloading borders: {err:#}");
                return;
            }
        };

        let mut initial_windows: Vec<isize> = snapshot.present.keys().copied().collect();
        initial_windows.sort_unstable();
        *APP_STATE.initial_windows.lock().unwrap() = initial_windows;

        let mut creatable: Vec<WindowIdentity> = snapshot
            .present
            .values()
            .copied()
            .filter(|identity| snapshot.creatable.contains(&identity.hwnd))
            .collect();
        creatable.sort_by_key(|identity| (identity.hwnd, identity.process_id, identity.thread_id));
        for identity in creatable {
            self.create_border(identity);
        }
    }

    fn refresh_komorebi(&mut self, tracking_hwnds: Vec<isize>) {
        for tracking in tracking_hwnds {
            let Some(record) = self.registry.get_by_key(tracking).copied() else {
                continue;
            };
            if self
                .borders
                .get(&tracking)
                .and_then(|border| border.tracked_identity())
                != Some(record.tracking)
            {
                continue;
            }
            if let Some(border) = self.borders.get_mut(&tracking) {
                border.handle_komorebi_refresh();
            }
        }
    }

    fn border_update_targets(&self, focused_only: bool) -> Vec<isize> {
        if focused_only {
            self.registry
                .get_by_key(self.active_window)
                .map(|record| vec![record.tracking.hwnd])
                .unwrap_or_default()
        } else {
            self.registry
                .records()
                .into_iter()
                .map(|record| record.tracking.hwnd)
                .collect()
        }
    }

    fn apply_border_update(&mut self, update: BorderRuntimeUpdate, focused_only: bool) {
        let targets = self.border_update_targets(focused_only);
        for tracking in targets {
            let Some(border) = self.borders.get_mut(&tracking) else {
                continue;
            };
            match &update {
                BorderRuntimeUpdate::Colors { active, inactive } => {
                    border.apply_colors(active.clone(), inactive.clone())
                }
                BorderRuntimeUpdate::Width(width_config) => border.apply_width(*width_config),
                BorderRuntimeUpdate::Offset(offset_config) => border.apply_offset(*offset_config),
                BorderRuntimeUpdate::Radius(radius_config) => border.apply_radius(*radius_config),
            }
        }
    }

    fn snapshot(&self) -> BorderRuntimeSnapshot {
        BorderRuntimeSnapshot {
            active_window: self.active_window,
            border_count: self.registry.len(),
        }
    }

    fn handle_timer(&mut self, timer_id: usize) {
        match timer_id {
            LOCATION_TIMER_ID => self.handle_location_timer(),
            REORDER_TIMER_ID => self.handle_reorder_timer(),
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
            KillTimer(Some(dispatcher), LOCATION_TIMER_ID).log_if_err();
            KillTimer(Some(dispatcher), REORDER_TIMER_ID).log_if_err();
            KillTimer(Some(dispatcher), ANIMATION_TIMER_ID).log_if_err();
            KillTimer(Some(dispatcher), FOREGROUND_POLL_TIMER_ID).log_if_err();
            KillTimer(Some(dispatcher), RECONCILE_TIMER_ID).log_if_err();
        }
        self.location.reset();
        LOCATION_COMMAND_GATE.lock().unwrap().clear();
        GRAPHICS_REFRESH_STATE.store(GRAPHICS_REFRESH_IDLE, Ordering::Release);
        self.reorder.reset();
        REORDER_COMMAND_PENDING.store(false, Ordering::Release);
        self.animation_timer_interval_ms = None;
    }

    fn create_border(&mut self, identity: WindowIdentity) {
        // The identity was captured by the producer. Reject an HWND that was destroyed/reused while
        // the command sat in the queue.
        if !identity.still_matches() {
            return;
        }
        let tracking_window = identity.hwnd();

        let existing = self.registry.get(tracking_window).copied();
        if let Some(existing) = existing {
            if existing.tracking == identity && self.borders.contains_key(&identity.hwnd) {
                return;
            }
            if existing.tracking == identity {
                // Registry survived but its runtime object did not. Drop the stale dispatch entry
                // and rebuild from the still-valid captured identity.
                self.registry.remove_by_key(identity.hwnd);
            } else {
                self.destroy_border(existing.tracking);
            }
        }

        if !should_create_border(identity, self.current_process_elevated) {
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
        self.registry.insert(BorderRecord {
            tracking: identity,
            border_hwnd: border_hwnd.0 as isize,
            state: BorderLifecycleState::Initializing,
        });

        if let Some(mut replaced) = self.borders.insert(identity.hwnd, border) {
            // Defensive: this should only be possible if runtime/registry state diverged.
            replaced.prepare_for_destroy();
            drop(replaced);
        }

        let is_active = identity.hwnd == self.active_window;
        let init_result = self
            .borders
            .get_mut(&identity.hwnd)
            .expect("border inserted immediately above")
            .init(window_rule, is_active);
        if let Err(err) = init_result {
            error!("could not initialize border for {tracking_window:?}: {err:#}");
            self.destroy_border(identity);
        }
    }

    fn destroy_border(&mut self, expected_identity: WindowIdentity) {
        let current = self.registry.get_by_key(expected_identity.hwnd).copied();

        // A delayed destroy event for an old HWND must never destroy a new window that reused the
        // same numeric handle.
        if current.map(|record| record.tracking) != Some(expected_identity) {
            return;
        }

        // Stop global dispatch before DestroyWindow can synchronously re-enter a window procedure.
        self.remove_animation_registration(expected_identity);
        self.refresh_animation_timer();

        if let Some(record) = self.registry.remove_by_key(expected_identity.hwnd) {
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
        self.reset_location_coalescing();

        if self.dispatcher_hwnd != 0 {
            unsafe { KillTimer(Some(HWND(self.dispatcher_hwnd as _)), REORDER_TIMER_ID) }
                .log_if_err();
        }
        self.reorder.reset();
        REORDER_COMMAND_PENDING.store(false, Ordering::Release);

        let identities: Vec<WindowIdentity> = self
            .registry
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
                let current_identity = self
                    .registry
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
            let current_identity = self
                .registry
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

    fn reconcile_foreground(&mut self) {
        let new_active_hwnd = get_foreground_window();
        let old_active_hwnd = HWND(self.active_window as _);
        if new_active_hwnd != old_active_hwnd && !new_active_hwnd.is_invalid() {
            self.update_foreground(new_active_hwnd);
        }
    }

    fn reconcile_runtime_invariants(&mut self) {
        // Registry entry with no runtime object: remove dispatch first so the next reconcile can
        // recreate it if the tracking identity is still present/eligible.
        let records = self.registry.records();
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
        let registered: HashSet<isize> = self
            .registry
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

        let snapshot = WindowSnapshot::collect(self.current_process_elevated)?;
        let current = self.registry.records();
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

fn mark_graphics_refresh_requested(state: &AtomicU8) -> bool {
    loop {
        match state.load(Ordering::Acquire) {
            GRAPHICS_REFRESH_IDLE => {
                if state
                    .compare_exchange(
                        GRAPHICS_REFRESH_IDLE,
                        GRAPHICS_REFRESH_PENDING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return true;
                }
            }
            GRAPHICS_REFRESH_PENDING => {
                if state
                    .compare_exchange(
                        GRAPHICS_REFRESH_PENDING,
                        GRAPHICS_REFRESH_DIRTY,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return false;
                }
            }
            GRAPHICS_REFRESH_DIRTY => return false,
            _ => unreachable!("invalid graphics refresh state"),
        }
    }
}

fn begin_graphics_refresh(state: &AtomicU8) {
    // Any dirty bit set before execution is already covered by this pass because device state is
    // sampled now. A new event during execution can set DIRTY again.
    state.store(GRAPHICS_REFRESH_PENDING, Ordering::Release);
}

fn finish_graphics_refresh(state: &AtomicU8) -> bool {
    state.swap(GRAPHICS_REFRESH_IDLE, Ordering::AcqRel) == GRAPHICS_REFRESH_DIRTY
}

pub(crate) fn classify_graphics_error(code: HRESULT) -> Option<GraphicsRecoveryReason> {
    if code == D2DERR_RECREATE_TARGET {
        Some(GraphicsRecoveryReason::RenderTargetLost)
    } else if code == DXGI_ERROR_DEVICE_REMOVED || code == DXGI_ERROR_DEVICE_RESET {
        Some(GraphicsRecoveryReason::DeviceLost)
    } else {
        None
    }
}

fn force_recreate_directx_devices_with_config() -> WindowsCompatibleResult<()> {
    let render_backend = APP_STATE.config.read().unwrap().render_backend;
    let mut directx_devices = APP_STATE.directx_devices.write().unwrap();

    match render_backend {
        RenderBackendConfig::V2 => {
            info!("force recreating render devices after device loss");
            *directx_devices = Some(DirectXDevices::new(&APP_STATE.render_factory)?);
        }
        RenderBackendConfig::Legacy => {
            *directx_devices = None;
        }
    }

    Ok(())
}

fn sync_directx_devices_with_config() -> WindowsCompatibleResult<()> {
    let render_backend = APP_STATE.config.read().unwrap().render_backend;
    let mut directx_devices = APP_STATE.directx_devices.write().unwrap();

    match render_backend {
        RenderBackendConfig::V2 => match directx_devices.as_mut() {
            Some(devices) => devices.recreate_if_needed()?,
            None => {
                *directx_devices = Some(DirectXDevices::new(&APP_STATE.render_factory)?);
            }
        },
        RenderBackendConfig::Legacy => {
            *directx_devices = None;
        }
    }

    Ok(())
}

fn animation_interval_ms(fps: u32) -> u32 {
    (1000 / fps.max(1)).max(MIN_ANIMATION_TIMER_INTERVAL_MS)
}

fn should_create_border(identity: WindowIdentity, current_process_elevated: bool) -> bool {
    let hwnd = identity.hwnd();
    if !is_window_top_level(hwnd) || !is_window_visible(hwnd) || is_window_cloaked(hwnd) {
        return false;
    }

    // UIPI prevents a non-elevated process from positioning its border relative to an elevated
    // window. Filter that window before initialization so reconciliation does not retry it every
    // second. An elevation-query failure is treated conservatively while we are non-elevated.
    let target_process_elevated = if current_process_elevated {
        None
    } else {
        is_process_elevated(identity.process_id).ok()
    };
    if !elevation_allows_border(current_process_elevated, target_process_elevated) {
        return false;
    }

    let window_rule = get_window_rule(hwnd);
    if window_rule.enabled == Some(EnableMode::Bool(false)) {
        return false;
    }
    window_rule.enabled == Some(EnableMode::Bool(true)) || !has_filtered_style(hwnd)
}

fn elevation_allows_border(
    current_process_elevated: bool,
    target_process_elevated: Option<bool>,
) -> bool {
    current_process_elevated || target_process_elevated == Some(false)
}

#[derive(Debug, Default)]
struct WindowSnapshot {
    // All currently-present top-level identities. Hidden/minimized/cloaked windows remain here so
    // they are not churned out of the registry merely because their border is temporarily hidden.
    present: HashMap<isize, WindowIdentity>,
    // Subset that is eligible for a newly-created border right now.
    creatable: HashSet<isize>,
    current_process_elevated: bool,
}

impl WindowSnapshot {
    fn collect(current_process_elevated: bool) -> anyhow::Result<Self> {
        let mut snapshot = Self {
            current_process_elevated,
            ..Default::default()
        };
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
    if should_create_border(identity, snapshot.current_process_elevated) {
        snapshot.creatable.insert(identity.hwnd);
    }

    TRUE
}

#[cfg(test)]
mod tests {
    use super::{
        BorderRuntime, GRAPHICS_REFRESH_DIRTY, GRAPHICS_REFRESH_IDLE, GRAPHICS_REFRESH_PENDING,
        GraphicsRecoveryReason, IdentityCommandGate, LOCATION_COALESCE_INTERVAL_MS, LocationAction,
        LocationCoalescer, REORDER_DEBOUNCE_INTERVAL_MS, ReorderAction, ReorderCoalescer,
        animation_interval_ms, begin_graphics_refresh, classify_graphics_error,
        elevation_allows_border, finish_graphics_refresh, mark_graphics_refresh_requested,
    };
    use crate::border_registry::WindowIdentity;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::D2DERR_RECREATE_TARGET;
    use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET};
    use windows::core::HRESULT;

    #[test]
    fn animation_interval_tracks_fastest_reasonable_timer_rate() {
        assert_eq!(animation_interval_ms(60), 16);
        assert_eq!(animation_interval_ms(30), 33);
        assert_eq!(animation_interval_ms(120), 10); // SetTimer is not useful below ~10 ms here.
        assert_eq!(animation_interval_ms(0), 1000);
    }

    #[test]
    fn reorder_coalescer_flushes_first_event_immediately() {
        let start = Instant::now();
        let mut coalescer = ReorderCoalescer::default();
        assert_eq!(coalescer.on_event(start), ReorderAction::FlushNow);
    }

    #[test]
    fn reorder_coalescer_arms_one_trailing_timer_and_collapses_more_events() {
        let start = Instant::now();
        let mut coalescer = ReorderCoalescer::default();
        assert_eq!(coalescer.on_event(start), ReorderAction::FlushNow);
        assert_eq!(
            coalescer.on_event(start + Duration::from_millis(5)),
            ReorderAction::ArmTimer(Duration::from_millis(11))
        );
        assert_eq!(
            coalescer.on_event(start + Duration::from_millis(6)),
            ReorderAction::Coalesced
        );
        assert!(coalescer.on_timer(start + Duration::from_millis(REORDER_DEBOUNCE_INTERVAL_MS)));
    }

    #[test]
    fn reorder_coalescer_flushes_again_after_interval() {
        let start = Instant::now();
        let mut coalescer = ReorderCoalescer::default();
        assert_eq!(coalescer.on_event(start), ReorderAction::FlushNow);
        assert_eq!(
            coalescer.on_event(start + Duration::from_millis(REORDER_DEBOUNCE_INTERVAL_MS)),
            ReorderAction::FlushNow
        );
    }

    fn test_identity(hwnd: isize, process_id: u32, thread_id: u32) -> WindowIdentity {
        WindowIdentity {
            hwnd,
            process_id,
            thread_id,
        }
    }

    #[test]
    fn location_gate_is_full_identity_aware() {
        let old = test_identity(10, 100, 1000);
        let new = test_identity(10, 200, 2000);
        let mut gate = IdentityCommandGate::default();

        assert!(gate.acquire(old));
        assert!(!gate.acquire(old));
        assert!(gate.acquire(new));
        assert_eq!(gate.pending.len(), 2);

        gate.release(old);
        assert!(gate.acquire(old));
    }

    #[test]
    fn location_coalescer_flushes_first_event_immediately() {
        let start = Instant::now();
        let identity = test_identity(10, 1, 2);
        let mut coalescer = LocationCoalescer::default();
        assert_eq!(
            coalescer.on_event(identity, start),
            LocationAction::FlushNow
        );
    }

    #[test]
    fn location_coalescer_batches_repeated_identity() {
        let start = Instant::now();
        let identity = test_identity(10, 1, 2);
        let mut coalescer = LocationCoalescer::default();
        assert_eq!(
            coalescer.on_event(identity, start),
            LocationAction::FlushNow
        );
        assert_eq!(
            coalescer.on_event(identity, start + Duration::from_millis(5)),
            LocationAction::ArmTimer(Duration::from_millis(11))
        );
        assert_eq!(
            coalescer.on_event(identity, start + Duration::from_millis(6)),
            LocationAction::Coalesced
        );

        assert_eq!(
            coalescer.on_timer(start + Duration::from_millis(LOCATION_COALESCE_INTERVAL_MS)),
            vec![identity]
        );
    }

    #[test]
    fn location_coalescer_keeps_reused_hwnd_identities_separate() {
        let start = Instant::now();
        let first = test_identity(10, 1, 2);
        let replacement = test_identity(10, 7, 8);
        let mut coalescer = LocationCoalescer::default();
        assert_eq!(coalescer.on_event(first, start), LocationAction::FlushNow);
        assert!(matches!(
            coalescer.on_event(first, start + Duration::from_millis(1)),
            LocationAction::ArmTimer(_)
        ));
        assert_eq!(
            coalescer.on_event(replacement, start + Duration::from_millis(2)),
            LocationAction::Coalesced
        );

        assert_eq!(
            coalescer.on_timer(start + Duration::from_millis(LOCATION_COALESCE_INTERVAL_MS)),
            vec![first, replacement]
        );
    }

    #[test]
    fn graphics_refresh_gate_coalesces_before_runtime_execution() {
        let state = AtomicU8::new(GRAPHICS_REFRESH_IDLE);
        assert!(mark_graphics_refresh_requested(&state));
        assert_eq!(state.load(Ordering::Acquire), GRAPHICS_REFRESH_PENDING);
        assert!(!mark_graphics_refresh_requested(&state));
        assert_eq!(state.load(Ordering::Acquire), GRAPHICS_REFRESH_DIRTY);

        begin_graphics_refresh(&state);
        assert_eq!(state.load(Ordering::Acquire), GRAPHICS_REFRESH_PENDING);
        assert!(!finish_graphics_refresh(&state));
        assert_eq!(state.load(Ordering::Acquire), GRAPHICS_REFRESH_IDLE);
    }

    #[test]
    fn graphics_refresh_gate_requests_trailing_pass_for_event_during_execution() {
        let state = AtomicU8::new(GRAPHICS_REFRESH_PENDING);
        begin_graphics_refresh(&state);
        assert!(!mark_graphics_refresh_requested(&state));
        assert_eq!(state.load(Ordering::Acquire), GRAPHICS_REFRESH_DIRTY);
        assert!(finish_graphics_refresh(&state));
        assert_eq!(state.load(Ordering::Acquire), GRAPHICS_REFRESH_IDLE);
    }

    #[test]
    fn graphics_refresh_gate_can_queue_again_after_completion() {
        let state = AtomicU8::new(GRAPHICS_REFRESH_PENDING);
        begin_graphics_refresh(&state);
        assert!(!finish_graphics_refresh(&state));
        assert!(mark_graphics_refresh_requested(&state));
        assert_eq!(state.load(Ordering::Acquire), GRAPHICS_REFRESH_PENDING);
    }

    #[test]
    fn graphics_error_classifies_render_target_loss() {
        assert_eq!(
            classify_graphics_error(D2DERR_RECREATE_TARGET),
            Some(GraphicsRecoveryReason::RenderTargetLost)
        );
    }

    #[test]
    fn graphics_error_classifies_device_loss_and_ignores_other_errors() {
        assert_eq!(
            classify_graphics_error(DXGI_ERROR_DEVICE_REMOVED),
            Some(GraphicsRecoveryReason::DeviceLost)
        );
        assert_eq!(
            classify_graphics_error(DXGI_ERROR_DEVICE_RESET),
            Some(GraphicsRecoveryReason::DeviceLost)
        );
        assert_eq!(classify_graphics_error(HRESULT(0)), None);
    }

    #[test]
    fn snapshot_reads_runtime_owned_active_window() {
        let runtime = BorderRuntime {
            active_window: 0x1234,
            ..Default::default()
        };
        assert_eq!(runtime.snapshot().active_window, 0x1234);
    }

    #[test]
    fn elevation_policy_blocks_only_inaccessible_targets() {
        assert!(elevation_allows_border(false, Some(false)));
        assert!(!elevation_allows_border(false, Some(true)));
        assert!(!elevation_allows_border(false, None));
        assert!(elevation_allows_border(true, Some(false)));
        assert!(elevation_allows_border(true, Some(true)));
        assert!(elevation_allows_border(true, None));
    }
}
