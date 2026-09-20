use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use crate::APP_STATE;
use crate::border_runtime::{
    BorderRuntimeUpdate, request_apply_border_update, request_runtime_snapshot,
};
use crate::colors::ColorBrushConfig;
use crate::config::{Config, OffsetConfig, RadiusConfig, WidthConfig};
use crate::iocp::{UnixListener, UnixStream};
use crate::utils::{
    LogIfErr, WM_APP_SET_COLORS, WM_APP_SET_OFFSET, WM_APP_SET_RADIUS, WM_APP_SET_WIDTH,
    remove_file_if_exists,
};

pub trait IpcPayload: Clone {
    /// The message to post to the message queue for this payload
    const WND_MSG: u32;
}

#[derive(Clone)]
pub struct IpcSetColorsPayload {
    pub active_color: Option<ColorBrushConfig>,
    pub inactive_color: Option<ColorBrushConfig>,
}

impl IpcPayload for IpcSetColorsPayload {
    const WND_MSG: u32 = WM_APP_SET_COLORS;
}

#[derive(Clone)]
pub struct IpcSetWidthPayload {
    pub width_config: WidthConfig,
}

impl IpcPayload for IpcSetWidthPayload {
    const WND_MSG: u32 = WM_APP_SET_WIDTH;
}

#[derive(Clone)]
pub struct IpcSetOffsetPayload {
    pub offset_config: OffsetConfig,
}

impl IpcPayload for IpcSetOffsetPayload {
    const WND_MSG: u32 = WM_APP_SET_OFFSET;
}

#[derive(Clone)]
pub struct IpcSetRadiusPayload {
    pub radius_config: RadiusConfig,
}

impl IpcPayload for IpcSetRadiusPayload {
    const WND_MSG: u32 = WM_APP_SET_RADIUS;
}

pub fn socket_path() -> anyhow::Result<PathBuf> {
    Config::get_dir().map(|dir| dir.join("tacky-borders.sock"))
}

/// IPC Server that handles communication between a CLI and daemon.
/// Changes made via IPC are not written back to the config file.
pub struct IpcServer {
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    thread_handle: Option<JoinHandle<()>>,
}

impl IpcServer {
    pub fn new(socket_path: &Path) -> anyhow::Result<Self> {
        // Remove a stale socket file left over from a previous run; bind fails otherwise
        remove_file_if_exists(socket_path).context("could not remove stale ipc socket")?;

        let listener = UnixListener::bind(socket_path).context("could not bind ipc socket")?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let thread_handle = thread::spawn(move || run_server(listener, stop_clone));

        info!("ipc server listening on {}", socket_path.display());

        Ok(Self {
            socket_path: socket_path.to_owned(),
            stop,
            thread_handle: Some(thread_handle),
        })
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        // Unblock the accept() call in the server thread with a dummy connection
        let _ = UnixStream::connect(&self.socket_path);

        match self.thread_handle.take() {
            Some(handle) => {
                if let Err(err) = handle.join() {
                    error!("could not join ipc server thread handle: {err:?}");
                }
            }
            None => error!("could not take ipc server thread handle"),
        }

        // The listener has been dropped (its thread exited), so the socket file can go too
        remove_file_if_exists(&self.socket_path)
            .context("could not remove ipc socket")
            .log_if_err();

        debug!("ipc server stopped");
    }
}

fn run_server(listener: UnixListener, stop: Arc<AtomicBool>) {
    debug!("entering ipc server thread");

    loop {
        match listener.accept() {
            Ok(stream) => {
                // The dummy connection sent by Drop should not be processed
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                thread::spawn(move || {
                    if let Err(err) = handle_client(stream) {
                        debug!("ipc client disconnected: {err:#}");
                    }
                });
            }
            Err(err) => {
                if !stop.load(Ordering::Relaxed) {
                    error!("could not accept ipc client: {err}");
                }
                break;
            }
        }
    }

    debug!("exiting ipc server thread");
}

fn handle_client(stream: UnixStream) -> anyhow::Result<()> {
    let reader = BufReader::new(&stream);

    for line in reader.lines() {
        let line = line.context("could not read line from ipc client")?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut response = process_command(trimmed);
        response.push('\n');

        (&stream)
            .write_all(response.as_bytes())
            .context("could not write response to ipc client")?;
    }

    Ok(())
}

/// All commands that can be sent through the IPC mechanism. When serialized,
/// the enum variant is in snake case and denoted with "cmd".
/// Example JSON format: {"cmd":"set_color","active":<color>,"inactive":<color>}
#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum IpcCommand {
    SetColor {
        #[serde(default)]
        active: Option<ColorBrushConfig>,
        #[serde(default)]
        inactive: Option<ColorBrushConfig>,
        /// When true, only the currently focused window's border is updated.
        #[serde(default)]
        focused: bool,
    },
    SetWidth {
        width: WidthConfig,
        #[serde(default)]
        focused: bool,
    },
    SetOffset {
        offset: OffsetConfig,
        #[serde(default)]
        focused: bool,
    },
    SetRadius {
        radius: RadiusConfig,
        #[serde(default)]
        focused: bool,
    },
    Reload,
    GetState,
}

fn process_command(raw: &str) -> String {
    let command: IpcCommand = match serde_json::from_str(raw) {
        Ok(command) => command,
        Err(err) => {
            return json!({"ok": false, "error": format!("invalid command: {err}")}).to_string();
        }
    };

    match command {
        IpcCommand::SetColor {
            active,
            inactive,
            focused,
        } => {
            if active.is_none() && inactive.is_none() {
                return json!({"ok": false, "error": "no colors provided"}).to_string();
            }
            apply_colors(active, inactive, focused);
            json!({"ok": true}).to_string()
        }
        IpcCommand::SetWidth { width, focused } => {
            apply_width(width, focused);
            json!({"ok": true}).to_string()
        }
        IpcCommand::SetOffset { offset, focused } => {
            apply_offset(offset, focused);
            json!({"ok": true}).to_string()
        }
        IpcCommand::SetRadius { radius, focused } => {
            apply_radius(radius, focused);
            json!({"ok": true}).to_string()
        }
        IpcCommand::Reload => {
            Config::reload();
            crate::reload_borders();
            json!({"ok": true}).to_string()
        }
        IpcCommand::GetState => {
            let (active_color, inactive_color, border_width, border_offset, border_radius) = {
                let config = APP_STATE.config.read().unwrap();
                (
                    config.global.active_color.clone(),
                    config.global.inactive_color.clone(),
                    config.global.border_width,
                    config.global.border_offset,
                    config.global.border_radius,
                )
            };
            let snapshot = match request_runtime_snapshot() {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    return json!({
                        "ok": false,
                        "error": format!("could not query border runtime: {err:#}")
                    })
                    .to_string();
                }
            };

            json!({
                "ok": true,
                "active_window": format!("{:#x}", snapshot.active_window),
                "border_count": snapshot.border_count,
                "active_color": active_color,
                "inactive_color": inactive_color,
                "border_width": border_width,
                "border_offset": border_offset,
                "border_radius": border_radius,
            })
            .to_string()
        }
    }
}

fn apply_colors(
    active: Option<ColorBrushConfig>,
    inactive: Option<ColorBrushConfig>,
    focused_only: bool,
) {
    if !focused_only {
        // Update the in-memory global config so newly created borders pick up the colors too.
        let mut config = APP_STATE.config.write().unwrap();
        if let Some(ref color) = active {
            config.global.active_color = color.clone();
        }
        if let Some(ref color) = inactive {
            config.global.inactive_color = color.clone();
        }
    }
    request_apply_border_update(
        BorderRuntimeUpdate::Colors { active, inactive },
        focused_only,
    );
}

fn apply_width(width_config: WidthConfig, focused_only: bool) {
    if !focused_only {
        APP_STATE.config.write().unwrap().global.border_width = width_config;
    }
    request_apply_border_update(BorderRuntimeUpdate::Width(width_config), focused_only);
}

fn apply_offset(offset_config: OffsetConfig, focused_only: bool) {
    if !focused_only {
        APP_STATE.config.write().unwrap().global.border_offset = offset_config;
    }
    request_apply_border_update(BorderRuntimeUpdate::Offset(offset_config), focused_only);
}

fn apply_radius(radius_config: RadiusConfig, focused_only: bool) {
    if !focused_only {
        APP_STATE.config.write().unwrap().global.border_radius = radius_config;
    }
    request_apply_border_update(BorderRuntimeUpdate::Radius(radius_config), focused_only);
}
