use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, OBJID_CLIENT,
    OBJID_CURSOR, OBJID_WINDOW,
};

use crate::border_runtime::{
    BorderWindowEvent, request_destroy_observed_window, request_foreground_change,
    request_reorder_borders, request_window_event,
};
use crate::utils::get_foreground_window;

pub extern "system" fn process_win_event(
    _h_win_event_hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _dw_event_thread: u32,
    _dwms_event_time: u32,
) {
    if _id_object == OBJID_CURSOR.0 {
        return;
    }

    match _event {
        EVENT_OBJECT_LOCATIONCHANGE => {
            if _id_child == CHILDID_SELF as i32 {
                request_window_event(_hwnd, BorderWindowEvent::LocationChange);
            }
        }
        EVENT_OBJECT_REORDER => {
            if _id_object == OBJID_CLIENT.0 {
                request_reorder_borders();
            }
        }
        EVENT_SYSTEM_FOREGROUND => {
            request_foreground_change(get_foreground_window(), _hwnd);
        }
        EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED => {
            if _id_object == OBJID_WINDOW.0 {
                request_window_event(_hwnd, BorderWindowEvent::ShowUncloaked);
            }
        }
        EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED => {
            if _id_object == OBJID_WINDOW.0 {
                request_window_event(_hwnd, BorderWindowEvent::HideCloaked);
            }
        }
        EVENT_SYSTEM_MINIMIZESTART => {
            request_window_event(_hwnd, BorderWindowEvent::MinimizeStart);
        }
        EVENT_SYSTEM_MINIMIZEEND => {
            request_window_event(_hwnd, BorderWindowEvent::MinimizeEnd);
        }
        EVENT_OBJECT_DESTROY => {
            if _id_object == OBJID_WINDOW.0 && _id_child == CHILDID_SELF as i32 {
                request_destroy_observed_window(_hwnd);
            }
        }
        _ => {}
    }
}
