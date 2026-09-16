//! 全域熱鍵。用 Carbon 的 `RegisterEventHotKey`：它不需要「輸入監控」或「輔助使用」權限，
//! 而且事件由 `NSApplication` 的 run loop 在主執行緒派送。
//!
//! `NSEvent.addGlobalMonitorForEventsMatchingMask:` 看似現代，但要使用者到系統設定開權限，
//! 對一個只想切滑鼠穿透的個人工具太重。

use std::ffi::c_void;

use objc2::MainThreadMarker;

type OSStatus = i32;
type EventRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventTargetRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHotKeyRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

type EventHandlerProc = extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        target: EventTargetRef,
        handler: EventHandlerProc,
        num_types: usize,
        list: *const EventTypeSpec,
        user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn RegisterEventHotKey(
        hot_key_code: u32,
        hot_key_modifiers: u32,
        hot_key_id: EventHotKeyID,
        target: EventTargetRef,
        options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
}

/// `kEventClassKeyboard = 'keyb'`。
const EVENT_CLASS_KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");
/// `kEventHotKeyPressed`。
const EVENT_HOT_KEY_PRESSED: u32 = 5;
/// 自己挑的四字碼，只用來辨識是我們註冊的熱鍵。
const SIGNATURE: u32 = u32::from_be_bytes(*b"kros");

/// Carbon 的修飾鍵位元。
pub mod modifiers {
    pub const CMD: u32 = 1 << 8;
    pub const OPTION: u32 = 1 << 11;
    pub const CONTROL: u32 = 1 << 12;
}

/// `kVK_ANSI_K`。
pub const KEY_K: u32 = 0x28;

/// 熱鍵按下時呼叫的閉包。只會在主執行緒被呼叫，所以不要求 `Send`。
type Callback = Box<dyn Fn()>;

extern "C" fn on_hot_key(
    _call: EventHandlerCallRef,
    _event: EventRef,
    user: *mut c_void,
) -> OSStatus {
    if MainThreadMarker::new().is_none() {
        eprintln!("熱鍵事件不在主執行緒，忽略");
        return 0;
    }
    // SAFETY: `user` 是 `register` 用 `Box::into_raw` 交出的指標，程式生命週期內不釋放。
    let cb = unsafe { &*(user as *const Callback) };
    cb();
    0
}

/// 註冊一個全域熱鍵。只註冊一次；閉包與 Carbon 的把手都活到程式結束。
pub fn register(key_code: u32, mods: u32, callback: Callback) -> Result<(), OSStatus> {
    let user = Box::into_raw(Box::new(callback)) as *mut c_void;
    let spec = EventTypeSpec {
        event_class: EVENT_CLASS_KEYBOARD,
        event_kind: EVENT_HOT_KEY_PRESSED,
    };
    let mut handler: EventHandlerRef = std::ptr::null_mut();
    let mut hot_key: EventHotKeyRef = std::ptr::null_mut();
    // SAFETY: 參數都是合法指標；`spec` 只在呼叫期間需要存活（Carbon 會複製）。
    unsafe {
        let target = GetApplicationEventTarget();
        let rc = InstallEventHandler(target, on_hot_key, 1, &spec, user, &mut handler);
        if rc != 0 {
            return Err(rc);
        }
        let rc = RegisterEventHotKey(
            key_code,
            mods,
            EventHotKeyID {
                signature: SIGNATURE,
                id: 1,
            },
            target,
            0,
            &mut hot_key,
        );
        if rc != 0 {
            return Err(rc);
        }
    }
    Ok(())
}
