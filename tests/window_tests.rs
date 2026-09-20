use serial_test::serial;
use std::{thread, time};
use tacky_borders::border_runtime::{
    BorderRuntimeHost, BorderRuntimeSnapshot, request_runtime_snapshot,
};
use tacky_borders::utils::get_window_class;
use tacky_borders::{
    create_borders_for_existing_windows, destroy_borders, register_border_window_class,
    reload_borders,
};
use windows::Win32::Foundation::{HWND, LPARAM, TRUE};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumWindows, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
};
use windows::core::BOOL;

fn pump_runtime_for(duration: time::Duration) {
    let deadline = time::Instant::now() + duration;
    while time::Instant::now() < deadline {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        thread::sleep(time::Duration::from_millis(1));
    }
}

fn runtime_snapshot() -> anyhow::Result<BorderRuntimeSnapshot> {
    // request_runtime_snapshot() is synchronous and the runtime is owned by this test thread, so
    // issue the request from a worker while this thread continues pumping Win32 messages.
    let handle = thread::spawn(request_runtime_snapshot);
    while !handle.is_finished() {
        pump_runtime_for(time::Duration::from_millis(5));
    }
    handle.join().expect("runtime snapshot worker panicked")
}

#[test]
#[serial]
fn test_destroy_borders() -> anyhow::Result<()> {
    register_border_window_class()?;
    let _runtime = BorderRuntimeHost::new()?;

    for _ in 0..5 {
        create_borders_for_existing_windows()?;
        pump_runtime_for(time::Duration::from_millis(50));

        destroy_borders();
        pump_runtime_for(time::Duration::from_millis(50));

        unsafe { EnumWindows(Some(enum_windows_tests_callback), LPARAM::default()) }?;
    }

    Ok(())
}

#[test]
#[serial]
// This tests whether all borders are properly cleaned up when reload_borders() is called
fn test_reload_borders() -> anyhow::Result<()> {
    register_border_window_class()?;
    let _runtime = BorderRuntimeHost::new()?;
    create_borders_for_existing_windows()?;
    pump_runtime_for(time::Duration::from_millis(50));

    for _ in 0..5 {
        reload_borders();
        pump_runtime_for(time::Duration::from_millis(50));
    }
    destroy_borders();
    pump_runtime_for(time::Duration::from_millis(50));

    unsafe { EnumWindows(Some(enum_windows_tests_callback), LPARAM::default()) }?;

    Ok(())
}

#[test]
#[serial]
fn test_runtime_snapshot_after_destroy() -> anyhow::Result<()> {
    register_border_window_class()?;
    let _runtime = BorderRuntimeHost::new()?;

    create_borders_for_existing_windows()?;
    pump_runtime_for(time::Duration::from_millis(50));
    let _before = runtime_snapshot()?;

    destroy_borders();
    pump_runtime_for(time::Duration::from_millis(50));
    assert_eq!(runtime_snapshot()?.border_count, 0);

    Ok(())
}

unsafe extern "system" fn enum_windows_tests_callback(_hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let window_class = get_window_class(_hwnd).unwrap();
    assert!(window_class != "border");

    TRUE
}

// TODO: test border window rect with positive border offsets and negative effects translations
