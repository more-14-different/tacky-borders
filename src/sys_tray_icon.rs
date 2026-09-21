use anyhow::Context;
use std::thread;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, UnhookWinEvent};

use crate::BG_SERVICES;
use crate::auto_start::{is_autostart_enabled, toggle_autostart};
use crate::border_runtime::request_finalize_shutdown;
use crate::config::{
    Config, join_config_reload_worker, request_config_reload, signal_config_shutdown,
};
use crate::utils::LogIfErr;

pub fn create_tray_icon(hwineventhook: HWINEVENTHOOK) -> anyhow::Result<TrayIcon> {
    let icon = match Icon::from_resource(1, Some((64, 64))) {
        Ok(icon) => icon,
        Err(err) => {
            error!("could not retrieve icon from tacky-borders.exe for tray menu: {err:#}");

            // If we could not retrieve an icon from the exe, then try to create an empty icon. If
            // even that fails, then we'll just return an Error.
            let rgba: Vec<u8> = vec![0, 0, 0, 0];
            Icon::from_rgba(rgba, 1, 1).context("could not create empty tray icon")?
        }
    };

    let auto_enabled = is_autostart_enabled()?;

    let tray_menu = Menu::new();
    tray_menu.append_items(&[
        &MenuItem::with_id("0", "Show Config", true, None),
        &CheckMenuItem::with_id("1", "Auto Start", true, auto_enabled, None),
        &MenuItem::with_id("2", "Reload", true, None),
        &MenuItem::with_id("3", "Close", true, None),
    ])?;

    let tooltip = format!("{}{}", "tacky-borders v", env!("CARGO_PKG_VERSION"));

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip(tooltip)
        .with_icon(icon)
        .build();

    // Convert HWINEVENTHOOK to isize so we can move it into the event handler below
    let hwineventhook_isize = hwineventhook.0 as isize;

    // Handle tray icon events (i.e. clicking on the menu items)
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| match event.id.0.as_str() {
        // Show Config
        "0" => match Config::get_dir() {
            Ok(dir) => {
                open::that(dir).log_if_err();
            }
            Err(err) => error!("{err:#}"),
        },
        // Auto Start
        "1" => {
            if let Err(err) = toggle_autostart() {
                error!("{err:#}")
            }
        }
        // Reload
        "2" => request_config_reload(),
        // Close
        "3" => {
            // The first phase only closes producer gates on the UI callback stack. Background
            // joins happen on a coordinator so the runtime message pump remains able to answer
            // synchronous IPC requests (notably GetState) while those workers shut down.
            if !signal_config_shutdown() {
                return;
            }

            let hwineventhook = HWINEVENTHOOK(hwineventhook_isize as _);
            unsafe { UnhookWinEvent(hwineventhook) }
                .ok()
                .context("could not unhook win event")
                .log_if_err();

            let spawn_result = thread::Builder::new()
                .name("tacky-shutdown-coordinator".to_string())
                .spawn(|| {
                    join_config_reload_worker();
                    BG_SERVICES.lock().unwrap().shutdown();
                    request_finalize_shutdown();
                });

            if let Err(err) = spawn_result {
                error!("could not spawn shutdown coordinator: {err}");
                // Thread creation failure is exceptional. Fall back to synchronous cleanup, then
                // still finalize through the runtime so border HWND destruction stays UI-owned.
                join_config_reload_worker();
                BG_SERVICES.lock().unwrap().shutdown();
                request_finalize_shutdown();
            }
        }
        _ => {}
    }));

    tray_icon.map_err(anyhow::Error::new)
}
