use crate::animations::AnimationsConfig;
use crate::colors::ColorBrushConfig;
use crate::effects::EffectsConfig;
use crate::komorebi::KomorebiColorsConfig;
use crate::render_backend::RenderBackendConfig;
use crate::utils::{OwnedHANDLE, get_adjusted_radius, get_window_corner_preference};
use crate::{APP_STATE, BG_SERVICES, IS_WINDOWS_11, display_error_box, reload_borders};
use anyhow::{Context, anyhow};
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use std::fs::{self, DirBuilder};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};
use std::thread::JoinHandle;
use std::{env, iter, ptr, slice, thread, time};
use windows::Win32::Foundation::{HANDLE, HWND, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Dwm::{
    DWMWCP_DEFAULT, DWMWCP_DONOTROUND, DWMWCP_ROUND, DWMWCP_ROUNDSMALL,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::PCWSTR;

const DEFAULT_CONFIG: &str = include_str!("resources/config.yaml");

const CONFIG_RELOAD_IDLE: u8 = 0;
const CONFIG_RELOAD_PENDING: u8 = 1;
const CONFIG_RELOAD_DIRTY: u8 = 2;
static CONFIG_RELOAD_STATE: AtomicU8 = AtomicU8::new(CONFIG_RELOAD_IDLE);
static CONFIG_RELOAD_SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static CONFIG_RELOAD_WORKER: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
static CONFIG_ERROR_DIALOG_OPEN: AtomicBool = AtomicBool::new(false);

pub(crate) fn show_config_error_once(message: String) {
    if CONFIG_ERROR_DIALOG_OPEN
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        debug!("config error dialog is already open; suppressing duplicate dialog");
        return;
    }

    let spawn_result = thread::Builder::new()
        .name("tacky-config-error".to_string())
        .spawn(move || {
            struct ResetDialogGate;
            impl Drop for ResetDialogGate {
                fn drop(&mut self) {
                    CONFIG_ERROR_DIALOG_OPEN.store(false, Ordering::Release);
                }
            }

            let _reset = ResetDialogGate;
            display_error_box(message, None);
        });

    if let Err(err) = spawn_result {
        CONFIG_ERROR_DIALOG_OPEN.store(false, Ordering::Release);
        error!("could not spawn config error dialog worker: {err}");
    }
}

/// Requests a serialized config reload from a short-lived management worker. The worker is never
/// the ConfigWatcher, tray, or IPC client thread, so reconfiguring BackgroundServices can safely
/// stop and join any of those services without self-join or lock inversion.
pub fn request_config_reload() {
    if CONFIG_RELOAD_SHUTTING_DOWN.load(Ordering::Acquire) {
        return;
    }
    if !mark_config_reload_requested(&CONFIG_RELOAD_STATE) {
        return;
    }

    let mut worker_slot = CONFIG_RELOAD_WORKER.lock().unwrap();
    if CONFIG_RELOAD_SHUTTING_DOWN.load(Ordering::Acquire) {
        CONFIG_RELOAD_STATE.store(CONFIG_RELOAD_IDLE, Ordering::Release);
        return;
    }

    if let Some(handle) = worker_slot.take()
        && let Err(err) = handle.join()
    {
        error!("previous config reload worker panicked: {err:?}");
    }

    if CONFIG_RELOAD_SHUTTING_DOWN.load(Ordering::Acquire) {
        CONFIG_RELOAD_STATE.store(CONFIG_RELOAD_IDLE, Ordering::Release);
        return;
    }

    match thread::Builder::new()
        .name("tacky-config-reload".to_string())
        .spawn(run_config_reload_worker)
    {
        Ok(handle) => *worker_slot = Some(handle),
        Err(err) => {
            CONFIG_RELOAD_STATE.store(CONFIG_RELOAD_IDLE, Ordering::Release);
            error!("could not spawn config reload worker: {err}");
        }
    }
}

/// Stops accepting reload requests and waits for the current management worker to finish. Call
/// this before destroying borders or background services so a late reload cannot recreate either.
pub fn begin_config_shutdown() {
    CONFIG_RELOAD_SHUTTING_DOWN.store(true, Ordering::Release);
    CONFIG_RELOAD_STATE.store(CONFIG_RELOAD_IDLE, Ordering::Release);

    let handle = CONFIG_RELOAD_WORKER.lock().unwrap().take();
    if let Some(handle) = handle
        && let Err(err) = handle.join()
    {
        error!("config reload worker panicked during shutdown: {err:?}");
    }
}

fn mark_config_reload_requested(state: &AtomicU8) -> bool {
    loop {
        match state.load(Ordering::Acquire) {
            CONFIG_RELOAD_IDLE => {
                if state
                    .compare_exchange(
                        CONFIG_RELOAD_IDLE,
                        CONFIG_RELOAD_PENDING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return true;
                }
            }
            CONFIG_RELOAD_PENDING => {
                if state
                    .compare_exchange(
                        CONFIG_RELOAD_PENDING,
                        CONFIG_RELOAD_DIRTY,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return false;
                }
            }
            CONFIG_RELOAD_DIRTY => return false,
            _ => unreachable!("invalid config reload state"),
        }
    }
}

/// Returns true when a request arrived during the previous pass and another pass must run.
fn finish_config_reload_pass(state: &AtomicU8) -> bool {
    loop {
        match state.load(Ordering::Acquire) {
            CONFIG_RELOAD_PENDING => {
                if state
                    .compare_exchange(
                        CONFIG_RELOAD_PENDING,
                        CONFIG_RELOAD_IDLE,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return false;
                }
            }
            CONFIG_RELOAD_DIRTY => {
                if state
                    .compare_exchange(
                        CONFIG_RELOAD_DIRTY,
                        CONFIG_RELOAD_PENDING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return true;
                }
            }
            CONFIG_RELOAD_IDLE => return false,
            _ => unreachable!("invalid config reload state"),
        }
    }
}

fn run_config_reload_worker() {
    struct ResetReloadState {
        armed: bool,
    }
    impl Drop for ResetReloadState {
        fn drop(&mut self) {
            if self.armed {
                CONFIG_RELOAD_STATE.store(CONFIG_RELOAD_IDLE, Ordering::Release);
            }
        }
    }
    let mut reset = ResetReloadState { armed: true };

    loop {
        if CONFIG_RELOAD_SHUTTING_DOWN.load(Ordering::Acquire) {
            CONFIG_RELOAD_STATE.store(CONFIG_RELOAD_IDLE, Ordering::Release);
            reset.armed = false;
            break;
        }

        let old_config = (*APP_STATE.config.read().unwrap()).clone();
        let reconfigure_services = Config::reload_with_status();
        let new_config = (*APP_STATE.config.read().unwrap()).clone();

        // Preserve the existing parse-error behavior: a malformed file may temporarily publish the
        // default config, but it must not disable the watcher that is needed to observe the fix.
        if reconfigure_services && !CONFIG_RELOAD_SHUTTING_DOWN.load(Ordering::Acquire) {
            BG_SERVICES.lock().unwrap().reload(&new_config);
        }

        if old_config != new_config && !CONFIG_RELOAD_SHUTTING_DOWN.load(Ordering::Acquire) {
            info!("config.yaml has changed; reloading borders");
            reload_borders();
        }

        if CONFIG_RELOAD_SHUTTING_DOWN.load(Ordering::Acquire) {
            CONFIG_RELOAD_STATE.store(CONFIG_RELOAD_IDLE, Ordering::Release);
            reset.armed = false;
            break;
        }

        if !finish_config_reload_pass(&CONFIG_RELOAD_STATE) {
            reset.armed = false;
            break;
        }
    }
}

/// The config.yaml definition
#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub watch_config_changes: bool,
    #[serde(default = "serde_default_bool::<true>")]
    pub enable_logging: bool,
    #[serde(default = "serde_default_bool::<true>")]
    pub enable_ipc_server: bool,
    #[serde(default)]
    #[serde(alias = "rendering_backend")]
    pub render_backend: RenderBackendConfig,
    #[serde(default = "serde_default_global")]
    pub global: Global,
    #[serde(default)]
    pub window_rules: Vec<WindowRule>,
}

// Show borders even if the config.yaml is completely empty
// NOTE: This is intentionally kept separate from the Default trait because I want the
// width/offset zeroed out when config deserialization fails and falls back to Config::default()
fn serde_default_global() -> Global {
    Global {
        border_width: WidthConfig::serde_default(),
        border_offset: OffsetConfig::serde_default(),
        ..Default::default()
    }
}

#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Global {
    #[serde(default = "WidthConfig::serde_default")]
    pub border_width: WidthConfig,
    #[serde(default = "OffsetConfig::serde_default")]
    pub border_offset: OffsetConfig,
    #[serde(default)]
    pub border_radius: RadiusConfig,
    #[serde(default)]
    pub border_z_order: ZOrderMode,
    #[serde(default = "serde_default_bool::<true>")]
    pub follow_native_border: bool,
    #[serde(default)]
    pub active_color: ColorBrushConfig,
    #[serde(default)]
    pub inactive_color: ColorBrushConfig,
    #[serde(default)]
    pub komorebi_colors: KomorebiColorsConfig,
    #[serde(default)]
    pub animations: AnimationsConfig,
    #[serde(default)]
    pub effects: EffectsConfig,
    #[serde(alias = "init_delay")]
    #[serde(default = "serde_default_u64::<250>")]
    pub initialize_delay: u64, // Adjust delay when creating new windows/borders
    #[serde(alias = "restore_delay")]
    #[serde(default = "serde_default_u64::<200>")]
    pub unminimize_delay: u64, // Adjust delay when restoring minimized windows
}

pub fn serde_default_u64<const V: u64>() -> u64 {
    V
}

pub fn serde_default_i32<const V: i32>() -> i32 {
    V
}

// f32 cannot be a const, so we have to do the following instead
pub fn serde_default_f32<const V: i32>() -> f32 {
    V as f32
}

pub fn serde_default_bool<const V: bool>() -> bool {
    V
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WindowRule {
    #[serde(rename = "match")]
    pub kind: Option<MatchKind>,
    pub name: Option<String>,
    pub strategy: Option<MatchStrategy>,
    pub border_width: Option<WidthConfig>,
    pub border_offset: Option<OffsetConfig>,
    pub border_radius: Option<RadiusConfig>,
    pub border_z_order: Option<ZOrderMode>,
    pub follow_native_border: Option<bool>,
    pub active_color: Option<ColorBrushConfig>,
    pub inactive_color: Option<ColorBrushConfig>,
    pub komorebi_colors: Option<KomorebiColorsConfig>,
    pub animations: Option<AnimationsConfig>,
    pub effects: Option<EffectsConfig>,
    #[serde(alias = "init_delay")]
    pub initialize_delay: Option<u64>,
    #[serde(alias = "restore_delay")]
    pub unminimize_delay: Option<u64>,
    pub enabled: Option<EnableMode>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum MatchKind {
    Title,
    Class,
    Process,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum MatchStrategy {
    Equals,
    Contains,
    Regex,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct WidthConfig(f32);

impl WidthConfig {
    pub fn new(width: f32) -> Self {
        Self(width)
    }

    // TODO: Maybe rename this and other to_x methods to to_raw or smth idk
    /// Returns a DPI-adjusted raw width value
    pub fn to_width(&self, dpi: f32) -> i32 {
        (self.0 as f32 * dpi / 96.0).round() as i32
    }

    fn serde_default() -> Self {
        Self(4.0)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum OffsetConfig {
    Uniform(i32),
    PerSide(Offset),
}

impl OffsetConfig {
    pub fn new(offset: i32) -> Self {
        Self::Uniform(offset)
    }

    /// Returns a DPI-adjusted offset for each side
    pub fn to_offset(&self, dpi: f32) -> Offset {
        let scale = |value: i32| (value as f32 * dpi / 96.0).round() as i32;
        match *self {
            Self::Uniform(offset) => Offset::new(scale(offset)),
            Self::PerSide(offset) => Offset {
                top: scale(offset.top),
                left: scale(offset.left),
                right: scale(offset.right),
                bottom: scale(offset.bottom),
            },
        }
    }

    fn serde_default() -> Self {
        Self::Uniform(-1)
    }
}

impl Default for OffsetConfig {
    fn default() -> Self {
        Self::Uniform(0)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Offset {
    #[serde(default)]
    pub top: i32,
    #[serde(default)]
    pub left: i32,
    #[serde(default)]
    pub right: i32,
    #[serde(default)]
    pub bottom: i32,
}

impl Offset {
    pub fn new(offset: i32) -> Self {
        Self {
            top: offset,
            left: offset,
            right: offset,
            bottom: offset,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum RadiusConfig {
    #[default]
    Auto,
    Square,
    Round,
    RoundSmall,
    #[serde(untagged)]
    Custom(f32),
}

impl RadiusConfig {
    pub fn to_radius(&self, border_width: i32, dpi: u32, tracking_window: HWND) -> f32 {
        match self {
            // We also check Custom(-1.0) for legacy reasons (don't wanna break anyone's old config)
            RadiusConfig::Auto | RadiusConfig::Custom(-1.0) => {
                // I believe this will error on Windows 10, so we'll just use a default
                match get_window_corner_preference(tracking_window).unwrap_or(DWMWCP_DEFAULT) {
                    DWMWCP_DEFAULT => {
                        if *IS_WINDOWS_11 {
                            get_adjusted_radius(8.0, dpi, border_width)
                        } else {
                            0.0
                        }
                    }
                    DWMWCP_DONOTROUND => 0.0,
                    DWMWCP_ROUND => get_adjusted_radius(8.0, dpi, border_width),
                    DWMWCP_ROUNDSMALL => get_adjusted_radius(4.0, dpi, border_width),
                    _ => 0.0,
                }
            }
            RadiusConfig::Square => 0.0,
            RadiusConfig::Round => get_adjusted_radius(8.0, dpi, border_width),
            RadiusConfig::RoundSmall => get_adjusted_radius(4.0, dpi, border_width),
            RadiusConfig::Custom(radius) => get_adjusted_radius(*radius, dpi, border_width),
        }
    }
}
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub enum EnableMode {
    #[default]
    Auto,
    #[serde(untagged)]
    Bool(bool),
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
pub enum ZOrderMode {
    #[default]
    AboveWindow,
    BelowWindow,
}

impl Config {
    pub fn create() -> anyhow::Result<Self> {
        let config_dir = Self::get_dir()?;
        let config_path = config_dir.join("config.yaml");

        // When saving files with a text editor like VSCode or Neovim, there may be a small time
        // period where the target file is empty or doesn't exist, so we'll use some retries.
        let mut exists = fs::exists(&config_path).context("could not check if config exists")?;
        for _ in 0..2 {
            if !exists {
                debug!("config does not exist; attempting to check again");
                thread::sleep(time::Duration::from_millis(20));
                exists = fs::exists(&config_path).context("could not check if config exists")?;
            } else {
                break;
            }
        }

        // If the config.yaml does not exist, try to create it
        if !exists {
            let default_contents = DEFAULT_CONFIG.as_bytes();
            fs::write(&config_path, default_contents)
                .context("could not create default config.yaml")?;

            info!("generating default config in {}", config_dir.display());
        }

        // We also implement retries here for the same reasons listed earlier
        let mut contents = fs::read_to_string(&config_path).context("could not read config")?;
        for _ in 0..2 {
            if contents.is_empty() {
                debug!("config is empty; attempting to read again");
                thread::sleep(time::Duration::from_millis(20));
                contents = fs::read_to_string(&config_path).context("could not read config")?;
            } else {
                break;
            }
        }

        // Deserialize the config.yaml file
        serde_yaml_ng::from_str(&contents).map_err(anyhow::Error::new)
    }

    pub fn get_dir() -> anyhow::Result<PathBuf> {
        let config_dir = env::var("TACKY_BORDERS_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|env_err| {
                home_dir()
                    .map(|dir| dir.join(".config").join("tacky-borders"))
                    .ok_or(anyhow!("could not find home dir, and could not access TACKY_BORDERS_CONFIG_HOME env var: {env_err}"))
            })?;

        if !config_dir.exists() {
            DirBuilder::new()
                .recursive(true)
                .create(&config_dir)
                .context("could not create config directory")?;
        };

        Ok(config_dir)
    }

    fn reload_with_status() -> bool {
        let (new_config, parsed_successfully) = match Self::create() {
            Ok(config) => (config, true),
            Err(err) => {
                error!("could not reload config: {err:#}");
                show_config_error_once(format!("could not reload config: {err:#}"));

                (Config::default(), false)
            }
        };
        *APP_STATE.config.write().unwrap() = new_config;
        parsed_successfully
    }

    pub fn reload() {
        request_config_reload();
    }

    pub fn is_config_watcher_enabled(&self) -> bool {
        self.watch_config_changes
    }

    pub fn is_komorebi_integration_enabled(&self) -> bool {
        self.global.komorebi_colors.enabled
            || self.window_rules.iter().any(|rule| {
                rule.komorebi_colors
                    .as_ref()
                    .map(|komocolors| komocolors.enabled)
                    .unwrap_or(false)
            })
    }

    pub fn is_theme_aware_enabled(&self) -> bool {
        Self::is_color_theme_aware(&self.global.active_color)
            || Self::is_color_theme_aware(&self.global.inactive_color)
            || self.window_rules.iter().any(|rule| {
                rule.active_color
                    .as_ref()
                    .map_or(false, Self::is_color_theme_aware)
                    || rule
                        .inactive_color
                        .as_ref()
                        .map_or(false, Self::is_color_theme_aware)
            })
    }

    fn is_color_theme_aware(config: &ColorBrushConfig) -> bool {
        matches!(config, ColorBrushConfig::ThemeAware(_))
    }

    pub fn is_ipc_server_enabled(&self) -> bool {
        self.enable_ipc_server
    }
}

#[derive(Debug)]
pub struct ConfigWatcher {
    dir_handle: OwnedHANDLE,
    _changed_event: OwnedHANDLE,
    stop_event: OwnedHANDLE,
    thread_handle: Option<JoinHandle<()>>,
}

impl ConfigWatcher {
    pub fn new(
        config_path: PathBuf,
        debounce_time: u64,
        callback_fn: fn(),
    ) -> anyhow::Result<Self> {
        let config_dir = config_path
            .parent()
            .context("could not get parent dir for config watcher")?;
        let config_dir_vec: Vec<u16> = config_dir
            .as_os_str()
            .encode_wide()
            .chain(iter::once(0))
            .collect();

        let dir_handle = {
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(config_dir_vec.as_ptr()),
                    FILE_LIST_DIRECTORY.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                    None,
                )
            }
            .context("could not create overlapped dir handle for config watcher")?;
            OwnedHANDLE(handle)
        };
        let changed_event = OwnedHANDLE(unsafe { CreateEventW(None, false, false, None)? });
        let stop_event = OwnedHANDLE(unsafe { CreateEventW(None, true, false, None)? });

        // Convert HANDLEs to isize so the worker owns only raw copies while ConfigWatcher keeps the
        // actual handles alive until after the worker has been joined.
        let dir_handle_isize = dir_handle.0.0 as isize;
        let changed_handle_isize = changed_event.0.0 as isize;
        let stop_handle_isize = stop_event.0.0 as isize;

        let config_name = config_path
            .file_name()
            .context("could not get config name for config watcher")?
            .to_owned()
            .into_string()
            .map_err(|_| anyhow!("could not convert config name for config watcher"))?;
        let debounce_ms = debounce_time.min(u32::MAX as u64) as u32;

        let thread_handle = thread::spawn(move || unsafe {
            debug!("entering config watcher thread");

            let dir_handle = HANDLE(dir_handle_isize as _);
            let changed_event = HANDLE(changed_handle_isize as _);
            let stop_event = HANDLE(stop_handle_isize as _);
            let events = [changed_event, stop_event];
            const WAIT_OBJECT_1: WAIT_EVENT = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);

            let mut buffer = [0u8; 1024];

            loop {
                let mut overlapped = OVERLAPPED {
                    hEvent: changed_event,
                    ..Default::default()
                };

                if let Err(err) = ReadDirectoryChangesW(
                    dir_handle,
                    buffer.as_mut_ptr() as _,
                    buffer.len() as u32,
                    false,
                    FILE_NOTIFY_CHANGE_LAST_WRITE,
                    None,
                    Some(ptr::addr_of_mut!(overlapped)),
                    None,
                ) {
                    error!("could not arm config directory watcher: {err}");
                    break;
                }

                let wait_result = WaitForMultipleObjects(&events, false, INFINITE);
                if wait_result == WAIT_OBJECT_1 {
                    // The buffer and OVERLAPPED live on this worker stack. Cancel and drain the
                    // exact request before either can be dropped.
                    let _ = CancelIoEx(dir_handle, Some(ptr::addr_of!(overlapped)));
                    let mut ignored = 0u32;
                    let _ = GetOverlappedResult(
                        dir_handle,
                        ptr::addr_of!(overlapped),
                        ptr::addr_of_mut!(ignored),
                        true,
                    );
                    break;
                }

                if wait_result != WAIT_OBJECT_0 {
                    error!("could not wait for config directory changes: {wait_result:?}");
                    let _ = CancelIoEx(dir_handle, Some(ptr::addr_of!(overlapped)));
                    let mut ignored = 0u32;
                    let _ = GetOverlappedResult(
                        dir_handle,
                        ptr::addr_of!(overlapped),
                        ptr::addr_of_mut!(ignored),
                        true,
                    );
                    break;
                }

                let mut bytes_returned = 0u32;
                if let Err(err) = GetOverlappedResult(
                    dir_handle,
                    ptr::addr_of!(overlapped),
                    ptr::addr_of_mut!(bytes_returned),
                    false,
                ) {
                    error!("could not complete config directory read: {err}");
                    break;
                }

                Self::process_dir_change_notifs(&buffer, bytes_returned, &config_name, callback_fn);

                // Keep debounce interruptible by the same stop event. There is no pending I/O in
                // this interval, so a stop can exit immediately without any cancellation step.
                let debounce_result = WaitForSingleObject(stop_event, debounce_ms);
                if debounce_result == WAIT_OBJECT_0 {
                    break;
                }
                if debounce_result != WAIT_TIMEOUT {
                    error!("could not wait during config watcher debounce: {debounce_result:?}");
                    break;
                }
            }

            debug!("exiting config watcher thread");
        });

        Ok(Self {
            dir_handle,
            _changed_event: changed_event,
            stop_event,
            thread_handle: Some(thread_handle),
        })
    }

    pub fn process_dir_change_notifs(
        buffer: &[u8; 1024],
        bytes_returned: u32,
        config_name: &str,
        callback_fn: fn(),
    ) {
        let mut offset = 0usize;

        while offset < bytes_returned as usize {
            let info = unsafe { &*(buffer.as_ptr().add(offset) as *const FILE_NOTIFY_INFORMATION) };

            // We divide FileNameLength by 2 because it's in bytes (u8), but FileName is in u16
            let name_slice = unsafe {
                slice::from_raw_parts(info.FileName.as_ptr(), info.FileNameLength as usize / 2)
            };
            let file_name = String::from_utf16_lossy(name_slice);
            debug!("file changed: {file_name}");

            if file_name == *config_name {
                callback_fn();
                break; // Prevent multiple callbacks from the same notification
            }

            // If NextEntryOffset = 0, then we have reached the end of the notification
            if info.NextEntryOffset == 0 {
                break;
            } else {
                offset += info.NextEntryOffset as usize
            }
        }
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        // stop_event is manual-reset, so even if the worker has not armed its next overlapped read
        // yet it will observe the stop as soon as it reaches WaitForMultipleObjects.
        if let Err(err) = unsafe { SetEvent(self.stop_event.0) } {
            error!("could not signal config watcher stop event: {err:#}");
            // This is only a fallback for an invalid stop-event failure. With a valid event the
            // worker cancels its own exact OVERLAPPED request.
            let _ = unsafe { CancelIoEx(self.dir_handle.0, None) };
        }

        match self.thread_handle.take() {
            Some(handle) => {
                if let Err(err) = handle.join() {
                    error!("could not join config watcher thread handle: {err:?}");
                }
            }
            None => error!("could not take config watcher thread handle"),
        }
    }
}

pub fn config_watcher_callback() {
    // Never reload services on the watcher callback stack: the new config may disable this watcher
    // and dropping it here would attempt to join the current thread.
    request_config_reload();
}

#[cfg(test)]
mod lifecycle_tests {
    use super::{
        CONFIG_RELOAD_DIRTY, CONFIG_RELOAD_IDLE, CONFIG_RELOAD_PENDING, finish_config_reload_pass,
        mark_config_reload_requested,
    };
    use std::sync::atomic::{AtomicU8, Ordering};

    #[test]
    fn config_reload_gate_coalesces_and_runs_one_trailing_pass() {
        let state = AtomicU8::new(CONFIG_RELOAD_IDLE);
        assert!(mark_config_reload_requested(&state));
        assert_eq!(state.load(Ordering::Acquire), CONFIG_RELOAD_PENDING);
        assert!(!mark_config_reload_requested(&state));
        assert_eq!(state.load(Ordering::Acquire), CONFIG_RELOAD_DIRTY);
        assert!(finish_config_reload_pass(&state));
        assert_eq!(state.load(Ordering::Acquire), CONFIG_RELOAD_PENDING);
        assert!(!finish_config_reload_pass(&state));
        assert_eq!(state.load(Ordering::Acquire), CONFIG_RELOAD_IDLE);
    }

    #[test]
    fn config_reload_gate_can_start_again_after_completion() {
        let state = AtomicU8::new(CONFIG_RELOAD_IDLE);
        assert!(mark_config_reload_requested(&state));
        assert!(!finish_config_reload_pass(&state));
        assert!(mark_config_reload_requested(&state));
    }
}
