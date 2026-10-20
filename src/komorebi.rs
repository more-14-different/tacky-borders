use anyhow::{Context, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time;
use windows::Win32::Foundation::HWND;

use crate::border_runtime::request_komorebi_refresh;
use crate::colors::ColorBrushConfig;
use crate::config::serde_default_bool;
use crate::iocp::{UnixStreamSink, write_to_unix_socket};
use crate::utils::{get_foreground_window, is_window, remove_file_if_exists};

#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KomorebiColorsConfig {
    pub stack_color: Option<ColorBrushConfig>,
    pub monocle_color: Option<ColorBrushConfig>,
    pub floating_color: Option<ColorBrushConfig>,
    #[serde(default = "serde_default_bool::<true>")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowKind {
    Single,
    Stack,
    Monocle,
    Unfocused,
    Floating,
}

// Minimal version of a komorebi enum of the same name
#[derive(Serialize)]
#[serde(tag = "type", content = "content")]
enum SocketMessage {
    AddSubscriberSocket(String),
}

pub struct KomorebiIntegration {
    // NOTE: in komorebi it's <Border HWND, WindowKind>, but here it's <Tracking HWND, WindowKind>
    pub focus_state: Arc<Mutex<HashMap<isize, WindowKind>>>,
    _stream_sink: UnixStreamSink,
    subscribe_stop: Arc<AtomicBool>,
    subscribe_thread: Option<JoinHandle<()>>,
}

impl KomorebiIntegration {
    const TACKY_SOCKET: &str = "tacky-borders.sock";
    const KOMOREBI_SOCKET: &str = "komorebi.sock";
    const FOCUS_STATE_PRUNE_INTERVAL: time::Duration = time::Duration::from_secs(600);
    const SUBSCRIBE_RETRY_INTERVAL: time::Duration = time::Duration::from_secs(15);

    pub fn new() -> anyhow::Result<Self> {
        let komorebi_data_dir =
            Self::get_komorebi_data_dir().context("could not get komorebi data dir")?;
        let tacky_socket_path = komorebi_data_dir.join(Self::TACKY_SOCKET);
        let komorebi_socket_path = komorebi_data_dir.join(Self::KOMOREBI_SOCKET);

        remove_file_if_exists(&tacky_socket_path)
            .context("could not remove tacky-borders socket if it exists")?;

        let focus_state = Arc::new(Mutex::new(HashMap::new()));
        let focus_state_clone = focus_state.clone();

        let stream_sink =
            Self::spawn_komorebi_notification_handler(focus_state_clone, &tacky_socket_path)
                .context("could not spawn komorebi notification handler")?;

        let (subscribe_stop, subscribe_thread) =
            Self::spawn_komorebi_subscribe_thread(komorebi_socket_path)
                .context("could not spawn komorebi subscribe thread")?;

        Ok(Self {
            focus_state,
            _stream_sink: stream_sink,
            subscribe_stop,
            subscribe_thread: Some(subscribe_thread),
        })
    }

    fn spawn_komorebi_notification_handler(
        focus_state: Arc<Mutex<HashMap<isize, WindowKind>>>,
        tacky_socket_path: &Path,
    ) -> anyhow::Result<UnixStreamSink> {
        let mut last_focus_state_prune = time::Instant::now();

        let callback = move |buffer: &[u8], bytes_received: u32| {
            if last_focus_state_prune.elapsed() > Self::FOCUS_STATE_PRUNE_INTERVAL {
                debug!("pruning focus state for komorebi integration");
                focus_state
                    .lock()
                    .unwrap()
                    .retain(|&hwnd_isize, _| is_window(Some(HWND(hwnd_isize as _))));
                last_focus_state_prune = time::Instant::now();
            }

            Self::process_komorebi_notification(&focus_state, buffer, bytes_received);
        };

        let stream_sink = UnixStreamSink::new(tacky_socket_path, callback)?;

        Ok(stream_sink)
    }

    fn spawn_komorebi_subscribe_thread(
        komorebi_socket_path: PathBuf,
    ) -> anyhow::Result<(Arc<AtomicBool>, JoinHandle<()>)> {
        let mut subscribe_message = {
            let enum_variant = SocketMessage::AddSubscriberSocket(Self::TACKY_SOCKET.to_string());
            serde_json::to_string(&enum_variant)?
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let join_handle = thread::spawn(move || {
            let subscribe_bytes = unsafe { subscribe_message.as_bytes_mut() };

            Self::run_subscribe_loop(
                &stop_clone,
                || {
                    std::fs::metadata(&komorebi_socket_path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                },
                || write_to_unix_socket(&komorebi_socket_path, subscribe_bytes),
                || thread::park_timeout(Self::SUBSCRIBE_RETRY_INTERVAL),
            );
        });

        Ok((stop, join_handle))
    }

    fn run_subscribe_loop(
        stop: &AtomicBool,
        mut socket_generation: impl FnMut() -> Option<time::SystemTime>,
        mut subscribe: impl FnMut() -> anyhow::Result<()>,
        mut wait: impl FnMut(),
    ) {
        let mut subscribed_generation = None;

        while !stop.load(Ordering::Acquire) {
            match socket_generation() {
                Some(generation) if subscribed_generation != Some(generation) => {
                    match subscribe() {
                        Ok(()) => subscribed_generation = Some(generation),
                        Err(err) => {
                            // The write fails when komorebi isn't running which isn't a real issue.
                            debug!("could not send subscribe-socket message to komorebi: {err:#}");
                        }
                    }
                }
                None => {
                    // komorebi removes and recreates its socket on restart. Forget the successful
                    // subscription while the socket is absent so its replacement is registered.
                    subscribed_generation = None;
                }
                Some(_) => {}
            }

            if !stop.load(Ordering::Acquire) {
                wait();
            }
        }
    }

    fn get_komorebi_data_dir() -> anyhow::Result<PathBuf> {
        Ok(dirs::data_local_dir()
            .context("could not get data local dir")?
            .join("komorebi"))
    }

    // Largely adapted from komorebi's own border implementation. Thanks @LGUG2Z
    fn process_komorebi_notification(
        focus_state_mutex: &Arc<Mutex<HashMap<isize, WindowKind>>>,
        buffer: &[u8],
        bytes_received: u32,
    ) {
        let bytes_received = bytes_received as usize;
        if bytes_received > buffer.len() {
            error!(
                "komorebi notification length {bytes_received} exceeds receive buffer {}",
                buffer.len()
            );
            return;
        }

        let notification: serde_json_borrow::Value<'_> =
            match serde_json::from_slice(&buffer[..bytes_received]) {
                Ok(event) => event,
                Err(err) => {
                    error!("could not parse unix domain socket buffer: {err:#}");
                    return;
                }
            };

        let previous_focus_state = (*focus_state_mutex.lock().unwrap()).clone();
        let foreground_window = get_foreground_window();
        let next_focus_state = match Self::build_focus_state_from_notification(
            &notification,
            &previous_focus_state,
            foreground_window,
        ) {
            Ok(state) => state,
            Err(err) => {
                // External komorebi data is allowed to be malformed or come from a newer schema.
                // Reject the whole notification rather than panicking or publishing a partial map.
                error!("could not interpret komorebi notification: {err:#}");
                return;
            }
        };

        let mut changed_tracking: Vec<isize> = previous_focus_state
            .keys()
            .chain(next_focus_state.keys())
            .copied()
            .collect();
        changed_tracking.sort_unstable();
        changed_tracking.dedup();
        changed_tracking.retain(|tracking| {
            let previous_window_kind = previous_focus_state.get(tracking);
            let new_window_kind = next_focus_state.get(tracking);
            if previous_window_kind == new_window_kind {
                return false;
            }

            // Single <-> Unfocused is already represented by tacky-borders' active/inactive colors.
            !(matches!(
                previous_window_kind,
                Some(WindowKind::Single) | Some(WindowKind::Unfocused)
            ) && matches!(
                new_window_kind,
                Some(WindowKind::Single) | Some(WindowKind::Unfocused)
            ))
        });

        *focus_state_mutex.lock().unwrap() = next_focus_state;

        // Runtime filters this list against its private registry, so komorebi never needs border
        // HWNDs or registry access of its own.
        request_komorebi_refresh(changed_tracking);
    }

    fn build_focus_state_from_notification<'ctx>(
        notification: &'ctx serde_json_borrow::Value<'ctx>,
        previous_focus_state: &HashMap<isize, WindowKind>,
        foreground_window: HWND,
    ) -> anyhow::Result<HashMap<isize, WindowKind>> {
        let monitors = notification.get("state").get("monitors");
        let monitor_elements = monitors
            .get("elements")
            .as_array()
            .context("state.monitors.elements is not an array")?;
        let focused_monitor_idx =
            Self::komorebi_index(monitors.get("focused"), "state.monitors.focused")?;
        if focused_monitor_idx >= monitor_elements.len() {
            return Err(anyhow!(
                "state.monitors.focused index {focused_monitor_idx} is out of bounds for {} monitors",
                monitor_elements.len()
            ));
        }

        let mut next_focus_state = previous_focus_state.clone();

        for (monitor_idx, monitor) in monitor_elements.iter().enumerate() {
            let workspaces = monitor.get("workspaces");
            let workspace_elements = workspaces
                .get("elements")
                .as_array()
                .context("monitor.workspaces.elements is not an array")?;
            if workspace_elements.is_empty() {
                continue;
            }
            let focused_workspace_idx =
                Self::komorebi_index(workspaces.get("focused"), "monitor.workspaces.focused")?;
            let workspace = workspace_elements.get(focused_workspace_idx).with_context(|| {
                format!(
                    "monitor.workspaces.focused index {focused_workspace_idx} is out of bounds for {} workspaces",
                    workspace_elements.len()
                )
            })?;

            let monocle = workspace.get("monocle_container");
            if !monocle.is_null() {
                let windows = monocle
                    .get("windows")
                    .get("elements")
                    .as_array()
                    .context("monocle_container.windows.elements is not an array")?;
                let window = windows
                    .first()
                    .context("monocle_container.windows.elements is empty")?;
                let tracking_hwnd = Self::komorebi_hwnd(
                    window.get("hwnd"),
                    "monocle_container.windows.elements[0].hwnd",
                )?;
                let new_kind = if monitor_idx != focused_monitor_idx {
                    WindowKind::Unfocused
                } else {
                    WindowKind::Monocle
                };
                next_focus_state.insert(tracking_hwnd, new_kind);
            }

            let containers = workspace.get("containers");
            let container_elements = containers
                .get("elements")
                .as_array()
                .context("workspace.containers.elements is not an array")?;
            let focused_container_idx = if container_elements.is_empty() {
                None
            } else {
                let index = Self::komorebi_index(
                    containers.get("focused"),
                    "workspace.containers.focused",
                )?;
                if index >= container_elements.len() {
                    return Err(anyhow!(
                        "workspace.containers.focused index {index} is out of bounds for {} containers",
                        container_elements.len()
                    ));
                }
                Some(index)
            };

            for (container_idx, container) in container_elements.iter().enumerate() {
                let windows = container.get("windows");
                let window_elements = windows
                    .get("elements")
                    .as_array()
                    .context("container.windows.elements is not an array")?;
                let focused_window_idx =
                    Self::komorebi_index(windows.get("focused"), "container.windows.focused")?;
                let focused_window = window_elements.get(focused_window_idx).with_context(|| {
                    format!(
                        "container.windows.focused index {focused_window_idx} is out of bounds for {} windows",
                        window_elements.len()
                    )
                })?;
                let tracking_hwnd = Self::komorebi_hwnd(
                    focused_window.get("hwnd"),
                    "container focused window hwnd",
                )?;

                let new_kind = if Some(container_idx) != focused_container_idx
                    || monitor_idx != focused_monitor_idx
                    || tracking_hwnd != foreground_window.0 as isize
                {
                    WindowKind::Unfocused
                } else if window_elements.len() > 1 {
                    WindowKind::Stack
                } else {
                    WindowKind::Single
                };
                next_focus_state.insert(tracking_hwnd, new_kind);
            }

            let floating_windows = workspace
                .get("floating_windows")
                .get("elements")
                .as_array()
                .context("workspace.floating_windows.elements is not an array")?;
            for window in floating_windows {
                let tracking_hwnd =
                    Self::komorebi_hwnd(window.get("hwnd"), "floating window hwnd")?;
                let new_kind = if tracking_hwnd == foreground_window.0 as isize {
                    WindowKind::Floating
                } else {
                    WindowKind::Unfocused
                };
                next_focus_state.insert(tracking_hwnd, new_kind);
            }
        }

        Ok(next_focus_state)
    }

    fn komorebi_index(value: &serde_json_borrow::Value<'_>, field: &str) -> anyhow::Result<usize> {
        let value = value
            .as_u64()
            .with_context(|| format!("{field} is not an unsigned integer"))?;
        usize::try_from(value).with_context(|| format!("{field} is out of range"))
    }

    fn komorebi_hwnd(value: &serde_json_borrow::Value<'_>, field: &str) -> anyhow::Result<isize> {
        let value = value
            .as_i64()
            .with_context(|| format!("{field} is not a signed integer"))?;
        isize::try_from(value).with_context(|| format!("{field} is out of range"))
    }
}

impl Drop for KomorebiIntegration {
    fn drop(&mut self) {
        self.subscribe_stop.store(true, Ordering::Release);
        match self.subscribe_thread.take() {
            Some(handle) => {
                handle.thread().unpark();
                if let Err(err) = handle.join() {
                    error!("could not join komorebi subscribe thread: {err:?}");
                }
            }
            None => error!("could not take komorebi subscribe thread handle"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{KomorebiIntegration, WindowKind};
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::Win32::Foundation::HWND;

    #[test]
    fn komorebi_subscription_is_renewed_when_socket_generation_changes() {
        let stop = AtomicBool::new(false);
        let polls = Cell::new(0);
        let subscriptions = Cell::new(0);

        KomorebiIntegration::run_subscribe_loop(
            &stop,
            || {
                polls.set(polls.get() + 1);
                Some(if polls.get() < 3 {
                    std::time::UNIX_EPOCH
                } else {
                    std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)
                })
            },
            || {
                subscriptions.set(subscriptions.get() + 1);
                Ok(())
            },
            || {
                if polls.get() == 3 {
                    stop.store(true, Ordering::Release);
                }
            },
        );

        assert_eq!(subscriptions.get(), 2);
    }

    #[test]
    fn failed_komorebi_subscription_is_retried_for_the_same_socket_generation() {
        let stop = AtomicBool::new(false);
        let attempts = Cell::new(0);

        KomorebiIntegration::run_subscribe_loop(
            &stop,
            || Some(std::time::UNIX_EPOCH),
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    anyhow::bail!("transient connection failure");
                }
                Ok(())
            },
            || {
                if attempts.get() == 2 {
                    stop.store(true, Ordering::Release);
                }
            },
        );

        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn malformed_komorebi_schema_is_rejected_without_mutating_previous_state() {
        let previous = HashMap::from([(0x1234isize, WindowKind::Single)]);
        let notification: serde_json_borrow::Value<'_> =
            serde_json::from_slice(br#"{"state":{"monitors":{"focused":0,"elements":[]}}}"#)
                .expect("test notification should be valid json");

        let result = KomorebiIntegration::build_focus_state_from_notification(
            &notification,
            &previous,
            HWND::default(),
        );

        assert!(result.is_err());
        assert_eq!(previous.get(&0x1234), Some(&WindowKind::Single));
    }
}
