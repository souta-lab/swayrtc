//! Native Wayland input injection for SwayRTC.
//!
//! The previous implementation shelled out to `wtype` (one process per key
//! stroke) and `wlrctl` (one process per pointer event).  That made it
//! impossible to hold keys or mouse buttons down (press and release could not
//! be separated), which in turn broke long presses, drag & drop and
//! modifier+click combinations, and added a full process spawn (~30-80 ms) of
//! latency to every single event.
//!
//! This module keeps a resident pair of virtual input devices on a dedicated
//! OS thread instead:
//!
//! * `zwp_virtual_keyboard_v1` -- raw evdev key codes, with the keymap taken
//!   from the compositor's own `wl_keyboard.keymap` event (no xkbcommon link
//!   dependency, and the user's real keyboard layout is honoured).
//! * `zwlr_virtual_pointer_v1` -- absolute motion, separated button press /
//!   release, wheel axis and frame events.
//!
//! The tokio-facing contract is unchanged:
//!
//! ```ignore
//! pub async fn input_worker(rx: tokio::sync::mpsc::Receiver<serde_json::Value>)
//! ```
//!
//! Compose text (Japanese, emoji, i.e. anything non-ASCII) cannot be typed with
//! a virtual keyboard: `zwp_virtual_keyboard_v1` only carries evdev key codes
//! and no xkb keymap has a keysym for arbitrary Unicode.  Those `text` payloads
//! therefore go through the Wayland clipboard -- snapshot `wl-paste`, fill
//! `wl-copy` from stdin, tap Ctrl+V (Ctrl+Shift+V with
//! `SWAYRTC_PASTE_SHIFT=1`) on the virtual keyboard, then restore the snapshot
//! after the paste has landed.  Pure ASCII text keeps using `wtype` so the real
//! session layout is honoured.
//!
//! Untrusted DataChannel payloads are normalised and validated here (key code
//! allowlist, coordinate clamping, payload length limits) before they ever
//! reach the Wayland socket.  When the native path is unavailable (no
//! Wayland session, compositor without the wlroots protocols) the worker falls
//! back to the old `wtype`/`wlrctl` subprocess behaviour so the feature
//! degrades instead of breaking.

use serde_json::Value;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsFd,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc as std_mpsc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, QueueHandle, WEnum,
};

/// Bindings for the two virtual-device protocols.
///
/// The XML descriptions live in `native/protocols/` so the crate builds
/// offline; `build.rs` only tracks them for rebuilds.  The module layout
/// mirrors what `wayland-protocols` does: the scanner macros need
/// `wayland_client` (and `wayland_backend`) visible in the enclosing module.
mod protocols {
    pub mod virtual_keyboard {
        use wayland_client;
        use wayland_client::protocol::*;

        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/virtual-keyboard-unstable-v1.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_client_code!("protocols/virtual-keyboard-unstable-v1.xml");
    }

    pub mod virtual_pointer {
        use wayland_client;
        use wayland_client::protocol::*;

        pub mod __interfaces {
            use wayland_client::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocols/wlr-virtual-pointer-unstable-v1.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_client_code!("protocols/wlr-virtual-pointer-unstable-v1.xml");
    }
}

use self::protocols::{virtual_keyboard as vk, virtual_pointer as vp};

type VirtualKeyboard = vk::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
type VirtualKeyboardManager = vk::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
type VirtualPointer = vp::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;
type VirtualPointerManager =
    vp::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// evdev key codes used for the modifier keys we synthesise.
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTALT: u32 = 56;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTMETA: u32 = 125;
const KEY_CAPSLOCK: u32 = 58;
const KEY_NUMLOCK: u32 = 69;

/// Default xkb modifier masks (the standard "evdev" index order used by every
/// wlroots session).  `zwp_virtual_keyboard_v1` does *not* derive modifier
/// state from key events (`wlr_event_keyboard_key.update_state = false` in
/// wlroots), so the client has to push the mask explicitly with the
/// `modifiers` request.
const XKB_SHIFT: u32 = 1 << 0;
const XKB_LOCK: u32 = 1 << 1;
const XKB_CTRL: u32 = 1 << 2;
const XKB_MOD1: u32 = 1 << 3;
const XKB_MOD2: u32 = 1 << 4;
const XKB_MOD4: u32 = 1 << 6;

/// depressed mask contribution per modifier index: ctrl, alt, shift, meta.
const MOD_MASKS: [u32; 4] = [XKB_CTRL, XKB_MOD1, XKB_SHIFT, XKB_MOD4];
/// Canonical modifier key code per index: ctrl, alt, shift, meta.
const MOD_KEYS: [u32; 4] = [KEY_LEFTCTRL, KEY_LEFTALT, KEY_LEFTSHIFT, KEY_LEFTMETA];

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

/// `wl_keyboard.keymap_format`: XKB v1 text keymap.
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// Wheel deltas are browser CSS pixels.  A wheel notch is ~100 px which we
/// translate to ~20 surface-local units (a notch is 15 units in wlroots) --
/// the same scale the old `wlrctl` path used.
const WHEEL_SCALE: f64 = 0.2;

/// Upper bound for a single text payload, in bytes.
///
/// The signalling server caps the whole DataChannel message at 512 bytes
/// (`native/src/main.rs`), so the JSON envelope is the tighter bound in
/// production; 1024 is the ceiling this module enforces on its own so a
/// directly embedded caller cannot push an unbounded paste into the compositor.
const MAX_TEXT_LEN: usize = 1024;

/// Clipboard MIME type used for pasted text.
const CLIPBOARD_TEXT_MIME: &str = "text/plain;charset=utf-8";

/// Upper bound for the clipboard content we save and restore around a paste.
/// A selection larger than this (image, video, huge log) is left alone instead
/// of being copied through two extra processes.
const MAX_CLIPBOARD_SNAPSHOT: usize = 256 * 1024;

/// How long a paste gets before the previous clipboard contents are restored.
const CLIPBOARD_RESTORE_DELAY_MS: u64 = 250;

/// Per-subprocess deadline for the clipboard helpers: a hung selection owner
/// must not freeze input injection.
const CLIPBOARD_TIMEOUT_MS: u64 = 1500;

/// Maximum absolute value of one relative pointer motion delta, in surface
/// units.  A hostile viewer cannot teleport the cursor across the screen.
const MAX_REL_MOTION: i32 = 500;

/// `KEY_V` (evdev 47, `KeyboardEvent.code == "KeyV"`): the paste shortcut.
const KEY_V: u32 = 47;

/// Fallback keymap used only when the compositor did not hand us a keymap
/// (e.g. a headless seat with no physical keyboard).  Includes are resolved by
/// libxkbcommon inside the compositor, so this stays layout-agnostic and tiny.
const FALLBACK_KEYMAP: &[u8] = b"xkb_keymap {\n\
\txkb_keycodes { include \"evdev+aliases(qwerty)\" };\n\
\txkb_types { include \"complete\" };\n\
\txkb_compatibility { include \"complete\" };\n\
\txkb_symbols { include \"pc+us+inet(evdev)\" };\n\
\txkb_geometry { include \"pc(pc105)\" };\n\
};\n\0";

// ---------------------------------------------------------------------------
// Anonymous mmap-able file for the keymap fd
// ---------------------------------------------------------------------------

/// Create a private, already-unlinked file containing `data`.
///
/// Wayland passes the keymap as a file descriptor and the compositor mmap()s
/// it, so a pipe is not enough.  `memfd_create` is not reachable through std,
/// so use a `create_new` + immediate `unlink` file in a tmpfs (or any local
/// filesystem as a fallback); the descriptor stays anonymous afterwards.
fn anonymous_file(data: &[u8]) -> std::io::Result<File> {
    let mut last_err = None;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for dir in [PathBuf::from("/dev/shm"), std::env::temp_dir()] {
        for attempt in 0..8u32 {
            let path = dir.join(format!(
                "swayrtc-keymap-{}-{stamp}-{attempt}",
                std::process::id()
            ));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .mode(0o600)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    // Unlink immediately: the fd is all we need.
                    let _ = std::fs::remove_file(&path);
                    file.write_all(data)?;
                    return Ok(file);
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_err = Some(err);
                }
                Err(err) => {
                    last_err = Some(err);
                    break;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no writable temp directory")))
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

fn debug_logging() -> bool {
    fn on(name: &str) -> bool {
        matches!(std::env::var(name).as_deref(), Ok("1") | Ok("true"))
    }
    on("SWAYRTC_INPUT_DEBUG") || on("SWAYRTC_DEBUG")
}

/// Trace connection setup on stderr (`SWAYRTC_INPUT_DEBUG=1`).
macro_rules! dlog {
    ($($arg:tt)*) => {
        if debug_logging() {
            eprintln!("swayrtc-input: {}", format_args!($($arg)*));
        }
    };
}

// ---------------------------------------------------------------------------
// Normalised, validated input events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Move {
        x: i32,
        y: i32,
    },
    /// Relative pointer motion (`{"type":"move","relative":true,"dx":..,
    /// "dy":..}`).  Browsers under pointer lock have no absolute coordinates,
    /// only `movementX`/`movementY`.
    MoveRelative {
        dx: i32,
        dy: i32,
    },
    Button {
        button: u32,
        down: bool,
        x: Option<i32>,
        y: Option<i32>,
    },
    Wheel {
        x: f64,
        y: f64,
    },
    Key {
        /// evdev key code derived from `KeyboardEvent.code` when available.
        code: Option<u32>,
        /// Legacy `KeyboardEvent.key` fallback (keysym-ish string).
        key: String,
        down: bool,
        /// True for synthetic one-shot key presses (on-screen keyboard panel),
        /// where the browser cannot send a matching key-up.
        tap: bool,
        mods: Mods,
    },
    Text(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Mods {
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
}

impl Mods {
    fn from_value(v: &Value) -> Self {
        Self {
            ctrl: v.get("ctrlKey").and_then(Value::as_bool) == Some(true),
            alt: v.get("altKey").and_then(Value::as_bool) == Some(true),
            shift: v.get("shiftKey").and_then(Value::as_bool) == Some(true),
            meta: v.get("metaKey").and_then(Value::as_bool) == Some(true),
        }
    }

    fn as_array(self) -> [bool; 4] {
        [self.ctrl, self.alt, self.shift, self.meta]
    }
}

fn finite(v: &Value, key: &str) -> Option<f64> {
    let n = v.get(key)?.as_f64()?;
    if !n.is_finite() || n.abs() > 100_000.0 {
        return None;
    }
    Some(n)
}

fn integer(v: &Value, key: &str) -> Option<i32> {
    finite(v, key).map(|n| n.round() as i32)
}

fn clamp_i(v: i32, max: i32) -> i32 {
    v.clamp(0, max.max(0))
}

/// Parse one untrusted DataChannel message into a validated event.
fn parse_event(v: &Value) -> Option<Event> {
    match v.get("type").and_then(Value::as_str)? {
        "move" => {
            // Relative motion (pointer lock) takes precedence when requested;
            // the absolute form stays the default for the normal viewer.
            if v.get("relative").and_then(Value::as_bool) == Some(true) {
                let dx = integer(v, "dx")?;
                let dy = integer(v, "dy")?;
                Some(Event::MoveRelative {
                    dx: dx.clamp(-MAX_REL_MOTION, MAX_REL_MOTION),
                    dy: dy.clamp(-MAX_REL_MOTION, MAX_REL_MOTION),
                })
            } else {
                Some(Event::Move {
                    x: clamp_i(integer(v, "x")?, 100_000),
                    y: clamp_i(integer(v, "y")?, 100_000),
                })
            }
        }
        "button" => {
            let button = v.get("button").and_then(Value::as_u64)?;
            if button > 4 {
                return None;
            }
            let down = v.get("down").and_then(Value::as_bool)?;
            Some(Event::Button {
                button: button as u32,
                down,
                x: integer(v, "x").map(|n| clamp_i(n, 100_000)),
                y: integer(v, "y").map(|n| clamp_i(n, 100_000)),
            })
        }
        "wheel" => {
            let x = finite(v, "x").unwrap_or(0.0).clamp(-250.0, 250.0);
            let y = finite(v, "y").unwrap_or(0.0).clamp(-250.0, 250.0);
            Some(Event::Wheel { x, y })
        }
        "key" => {
            let down = v.get("down").and_then(Value::as_bool)?;
            let repeat = v.get("repeat").and_then(Value::as_bool);
            // Key auto-repeat is handled by the compositor for a held key.
            if down && repeat == Some(true) {
                return None;
            }
            let code = v
                .get("code")
                .and_then(Value::as_str)
                .and_then(evdev_keycode);
            let key = v
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(64)
                .collect::<String>();
            if code.is_none() && !key.is_ascii() {
                return None;
            }
            // The on-screen keyboard sends key-down only (no `repeat` field and
            // no physical `code`); treat those as a one-shot tap so keys do not
            // get stuck.
            let tap = down && repeat.is_none() && code.is_none();
            if code.is_none() && key.is_empty() {
                return None;
            }
            Some(Event::Key {
                code,
                key,
                down,
                tap,
                mods: Mods::from_value(v),
            })
        }
        "text" => {
            let text = v.get("text").and_then(Value::as_str)?;
            if text.is_empty() || text.len() > MAX_TEXT_LEN {
                return None;
            }
            if text
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
            {
                return None;
            }
            Some(Event::Text(text.to_owned()))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// evdev key code table (KeyboardEvent.code -> Linux KEY_*)
// ---------------------------------------------------------------------------

fn evdev_keycode(code: &str) -> Option<u32> {
    let code = match code {
        // Accept the legacy/prefixed aliases too.
        "OSLeft" | "SuperLeft" => "MetaLeft",
        "OSRight" | "SuperRight" => "MetaRight",
        other => other,
    };
    Some(match code {
        "Escape" => 1,
        "Digit1" => 2,
        "Digit2" => 3,
        "Digit3" => 4,
        "Digit4" => 5,
        "Digit5" => 6,
        "Digit6" => 7,
        "Digit7" => 8,
        "Digit8" => 9,
        "Digit9" => 10,
        "Digit0" => 11,
        "Minus" => 12,
        "Equal" => 13,
        "Backspace" => 14,
        "Tab" => 15,
        "KeyQ" => 16,
        "KeyW" => 17,
        "KeyE" => 18,
        "KeyR" => 19,
        "KeyT" => 20,
        "KeyY" => 21,
        "KeyU" => 22,
        "KeyI" => 23,
        "KeyO" => 24,
        "KeyP" => 25,
        "BracketLeft" => 26,
        "BracketRight" => 27,
        "Enter" => 28,
        "ControlLeft" => KEY_LEFTCTRL,
        "KeyA" => 30,
        "KeyS" => 31,
        "KeyD" => 32,
        "KeyF" => 33,
        "KeyG" => 34,
        "KeyH" => 35,
        "KeyJ" => 36,
        "KeyK" => 37,
        "KeyL" => 38,
        "Semicolon" => 39,
        "Quote" => 40,
        "Backquote" => 41,
        "ShiftLeft" => KEY_LEFTSHIFT,
        "Backslash" => 43,
        "KeyZ" => 44,
        "KeyX" => 45,
        "KeyC" => 46,
        "KeyV" => 47,
        "KeyB" => 48,
        "KeyN" => 49,
        "KeyM" => 50,
        "Comma" => 51,
        "Period" => 52,
        "Slash" => 53,
        "ShiftRight" => 54,
        "NumpadMultiply" => 55,
        "AltLeft" => KEY_LEFTALT,
        "Space" => 57,
        "CapsLock" => 58,
        "F1" => 59,
        "F2" => 60,
        "F3" => 61,
        "F4" => 62,
        "F5" => 63,
        "F6" => 64,
        "F7" => 65,
        "F8" => 66,
        "F9" => 67,
        "F10" => 68,
        "NumLock" => 69,
        "ScrollLock" => 70,
        "Numpad7" => 71,
        "Numpad8" => 72,
        "Numpad9" => 73,
        "NumpadSubtract" => 74,
        "Numpad4" => 75,
        "Numpad5" => 76,
        "Numpad6" => 77,
        "NumpadAdd" => 78,
        "Numpad1" => 79,
        "Numpad2" => 80,
        "Numpad3" => 81,
        "Numpad0" => 82,
        "NumpadDecimal" => 83,
        "IntlBackslash" => 86,
        "F11" => 87,
        "F12" => 88,
        "IntlRo" => 89,
        "NumpadEnter" => 96,
        "ControlRight" => 97,
        "NumpadDivide" => 98,
        "PrintScreen" => 99,
        "AltRight" => 100,
        "Home" => 102,
        "ArrowUp" => 103,
        "PageUp" => 104,
        "ArrowLeft" => 105,
        "ArrowRight" => 106,
        "End" => 107,
        "ArrowDown" => 108,
        "PageDown" => 109,
        "Insert" => 110,
        "Delete" => 111,
        "AudioVolumeMute" => 113,
        "AudioVolumeDown" => 114,
        "AudioVolumeUp" => 115,
        "NumpadEqual" => 117,
        "Pause" => 119,
        "NumpadComma" => 121,
        "IntlYen" => 124,
        "MetaLeft" => KEY_LEFTMETA,
        "MetaRight" => 126,
        "ContextMenu" => 127,
        "BrowserBack" => 158,
        "BrowserForward" => 159,
        "MediaTrackNext" => 163,
        "MediaPlayPause" => 164,
        "MediaTrackPrevious" => 165,
        "MediaStop" => 166,
        _ => return None,
    })
}

/// US layout fallback: character -> (evdev key code, needs shift).
///
/// Only used for `{"type":"text"}` when `wtype` is not installed, or as a last
/// resort for a key whose `code` we do not know.  The primary text path keeps
/// using `wtype` because that one respects the session's real layout.
fn us_char(ch: char) -> Option<(u32, bool)> {
    Some(match ch {
        'a'..='z' => {
            let idx = ch as u8 - b'a';
            (
                [
                    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31,
                    20, 22, 47, 17, 45, 21, 44,
                ][idx as usize],
                false,
            )
        }
        'A'..='Z' => {
            let lower = ch.to_ascii_lowercase();
            let (code, _) = us_char(lower)?;
            (code, true)
        }
        '1' => (2, false),
        '2' => (3, false),
        '3' => (4, false),
        '4' => (5, false),
        '5' => (6, false),
        '6' => (7, false),
        '7' => (8, false),
        '8' => (9, false),
        '9' => (10, false),
        '0' => (11, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        '\\' => (43, false),
        '|' => (43, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        ' ' => (57, false),
        '\n' => (28, false),
        '\t' => (15, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        _ => return None,
    })
}

/// Map browser key names (`KeyboardEvent.key` / on-screen keyboard buttons) to
/// evdev codes.  The on-screen keyboard only sends `key`, never `code`.
fn named_key_code(key: &str) -> Option<u32> {
    Some(match key {
        " " => 57,
        "Control" => KEY_LEFTCTRL,
        "Shift" => KEY_LEFTSHIFT,
        "Alt" => KEY_LEFTALT,
        "Meta" | "Super" | "OS" => KEY_LEFTMETA,
        "CapsLock" => KEY_CAPSLOCK,
        "NumLock" => KEY_NUMLOCK,
        // Legacy keysym spellings used by the old wtype path.
        "Return" => 28,
        "BackSpace" => 14,
        "Left" => 105,
        "Right" => 106,
        "Up" => 103,
        "Down" => 108,
        "Prior" => 104,
        "Next" => 109,
        "space" => 57,
        other => evdev_keycode(other)?,
    })
}

// ---------------------------------------------------------------------------
// Wayland state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct LegacyPointer {
    position: Option<(i32, i32)>,
}

impl LegacyPointer {
    /// wlrctl only has relative motion; anchor at the top-left corner once and
    /// then move relatively, like the old implementation did.
    fn move_to(&mut self, x: i32, y: i32) {
        if self.position.is_none() {
            run_quiet("wlrctl", &["pointer", "move", "-10000", "-10000"]);
            self.position = Some((0, 0));
            // The anchor jump leaves the pointer at 0,0 which is also our model
            // of the current position after the jump.
        }
        let (ox, oy) = self.position.unwrap_or((0, 0));
        let (dx, dy) = (x - ox, y - oy);
        if dx == 0 && dy == 0 {
            return;
        }
        if run_quiet("wlrctl", &["pointer", "move", &dx.to_string(), &dy.to_string()]) {
            self.position = Some((x, y));
        }
    }

    /// Relative motion, used for pointer-lock style events.  wlrctl already
    /// speaks relative coordinates, so it is a direct pass-through.
    fn move_by(&mut self, dx: i32, dy: i32) {
        if dx == 0 && dy == 0 {
            return;
        }
        if !run_quiet("wlrctl", &["pointer", "move", &dx.to_string(), &dy.to_string()]) {
            return;
        }
        let (x, y) = self.position.unwrap_or((0, 0));
        self.position = Some((x + dx, y + dy));
    }

    fn button(&mut self, button: u32) {
        let name = match button {
            0 => "left",
            1 => "middle",
            2 => "right",
            _ => return,
        };
        run_quiet("wlrctl", &["pointer", "click", name]);
    }

    fn scroll(&mut self, x: f64, y: f64) {
        let v = (y.clamp(-250.0, 250.0) / 5.0).round() as i32;
        let h = (x.clamp(-250.0, 250.0) / 5.0).round() as i32;
        run_quiet(
            "wlrctl",
            &["pointer", "scroll", &v.to_string(), &h.to_string()],
        );
    }
}

/// A copy of the clipboard taken right before a paste, put back afterwards so
/// typing remote text does not destroy what the user had copied.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClipboardSnapshot {
    mime: String,
    data: Vec<u8>,
}

/// Clipboard snapshot waiting for its restore deadline.
#[derive(Debug)]
struct PendingRestore {
    due: Instant,
    snapshot: ClipboardSnapshot,
}

struct InputState {
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keymap: Option<Vec<u8>>,
    vk: Option<VirtualKeyboard>,
    vp: Option<VirtualPointer>,
    /// Set once we have applied a keymap that came from the compositor.
    keymap_from_compositor: bool,
    /// Pointer coordinate space extents (output size - 1).
    extent: (u32, u32),
    start: Instant,
    pressed_keys: Vec<u32>,
    /// Code we pressed for each modifier index (ctrl, alt, shift, meta), if any.
    mod_pressed: [u32; 4],
    /// Modifier state we told the compositor about: ctrl, alt, shift, meta.
    mods_active: [bool; 4],
    /// Lock modifier bits (caps lock / num lock) we are holding.
    locked_mask: u32,
    /// Last modifier mask pushed to the compositor.
    mods_sent: u32,
    locked_sent: u32,
    keys_sent: bool,
    /// Last absolute pointer position we asked for (used by the relative
    /// motion fallback).
    last_motion: (i32, i32),
    legacy: LegacyPointer,
    /// Clipboard contents to put back once the pending clipboard paste has
    /// been consumed by the focused client.
    pending_restore: Option<PendingRestore>,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            seat: None,
            keyboard: None,
            keymap: None,
            vk: None,
            vp: None,
            keymap_from_compositor: false,
            extent: (1919, 1079),
            start: Instant::now(),
            pressed_keys: Vec::new(),
            mod_pressed: [0; 4],
            mods_active: [false; 4],
            locked_mask: 0,
            mods_sent: 0,
            locked_sent: 0,
            keys_sent: false,
            last_motion: (0, 0),
            legacy: LegacyPointer::default(),
            pending_restore: None,
        }
    }
}

impl InputState {
    fn timestamp(&self) -> u32 {
        // Millisecond timestamps on a monotonic clock; wlroots ignores the base.
        self.start.elapsed().as_millis() as u32
    }

    fn clamp_x(&self, x: i32) -> u32 {
        x.clamp(0, self.extent.0 as i32) as u32
    }

    fn clamp_y(&self, y: i32) -> u32 {
        y.clamp(0, self.extent.1 as i32) as u32
    }

    fn motion(&mut self, x: i32, y: i32) {
        let Some(vp) = &self.vp else {
            self.legacy.move_to(x, y);
            return;
        };
        let (cx, cy) = (self.clamp_x(x), self.clamp_y(y));
        dlog!("motion {cx},{cy} (extent {:?})", self.extent);
        if std::env::var("SWAYRTC_INPUT_RELATIVE").as_deref() == Ok("1") {
            let (lx, ly) = self.last_motion;
            vp.motion(self.timestamp(), (cx as i32 - lx) as f64, (cy as i32 - ly) as f64);
            self.last_motion = (cx as i32, cy as i32);
        } else {
            vp.motion_absolute(self.timestamp(), cx, cy, self.extent.0, self.extent.1);
        }
        vp.frame();
    }

    /// Relative pointer motion (`zwlr_virtual_pointer_v1.motion`).
    ///
    /// Used by browsers under pointer lock, where only `movementX`/`movementY`
    /// exist and every delta has to be applied by the compositor.  Deltas are
    /// clamped so a buggy or hostile viewer cannot teleport the cursor.
    fn motion_relative(&mut self, dx: i32, dy: i32) {
        let dx = dx.clamp(-MAX_REL_MOTION, MAX_REL_MOTION);
        let dy = dy.clamp(-MAX_REL_MOTION, MAX_REL_MOTION);
        if dx == 0 && dy == 0 {
            return;
        }
        let Some(vp) = &self.vp else {
            self.legacy.move_by(dx, dy);
            return;
        };
        let time = self.timestamp();
        dlog!("relative motion {dx},{dy}");
        vp.motion(time, dx as f64, dy as f64);
        vp.frame();
        // Keep the absolute-position bookkeeping in sync so the legacy
        // `SWAYRTC_INPUT_RELATIVE` diff path does not fight a real motion.
        self.last_motion = (
            (self.last_motion.0 + dx).clamp(0, self.extent.0 as i32),
            (self.last_motion.1 + dy).clamp(0, self.extent.1 as i32),
        );
    }

    /// Remember the clipboard contents we replaced, to be restored after the
    /// paste had time to reach the focused client.
    fn schedule_restore(&mut self, snapshot: ClipboardSnapshot, delay: Duration) {
        self.schedule_restore_at(snapshot, Instant::now() + delay);
    }

    fn schedule_restore_at(&mut self, snapshot: ClipboardSnapshot, due: Instant) {
        self.pending_restore = Some(PendingRestore { due, snapshot });
    }

    fn has_pending_restore(&self) -> bool {
        self.pending_restore.is_some()
    }

    /// Take the snapshot back once its deadline has passed.
    fn take_due_restore(&mut self, now: Instant) -> Option<ClipboardSnapshot> {
        match &self.pending_restore {
            Some(pending) if pending.due <= now => {}
            _ => return None,
        }
        self.pending_restore.take().map(|pending| pending.snapshot)
    }

    /// Take the snapshot regardless of the deadline (session teardown).
    fn take_pending_restore(&mut self) -> Option<ClipboardSnapshot> {
        self.pending_restore.take().map(|pending| pending.snapshot)
    }

    fn button(&mut self, button: u32, down: bool, x: Option<i32>, y: Option<i32>) {
        if let (Some(x), Some(y)) = (x, y) {
            self.motion(x, y);
        }
        if self.vp.is_none() {
            if down {
                self.legacy.button(button);
            }
            return;
        }
        let code = match button {
            0 => BTN_LEFT,
            1 => BTN_MIDDLE,
            2 => BTN_RIGHT,
            3 => BTN_SIDE,
            4 => BTN_EXTRA,
            _ => return,
        };
        let Some(vp) = &self.vp else { return };
        let state = if down {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        vp.button(self.timestamp(), code, state);
        vp.frame();
    }

    fn axis(&mut self, x: f64, y: f64) {
        if self.vp.is_none() {
            self.legacy.scroll(x, y);
            return;
        }
        let Some(vp) = &self.vp else { return };
        let time = self.timestamp();
        if y.abs() >= 0.5 {
            vp.axis(
                time,
                wl_pointer::Axis::VerticalScroll,
                y.clamp(-250.0, 250.0) * WHEEL_SCALE,
            );
        }
        if x.abs() >= 0.5 {
            vp.axis(
                time,
                wl_pointer::Axis::HorizontalScroll,
                x.clamp(-250.0, 250.0) * WHEEL_SCALE,
            );
        }
        vp.frame();
    }

    /// Send one raw key event through the virtual keyboard.
    fn raw_key(&mut self, code: u32, down: bool) {
        let Some(vk) = &self.vk else { return };
        vk.key(self.timestamp(), code, u32::from(down));
        self.keys_sent = true;
        if down {
            if !self.pressed_keys.contains(&code) {
                self.pressed_keys.push(code);
            }
        } else {
            self.pressed_keys.retain(|&c| c != code);
        }
    }

    fn modifier_index(code: u32) -> Option<usize> {
        match code {
            KEY_LEFTCTRL | 97 => Some(0),
            KEY_LEFTALT | 100 => Some(1),
            KEY_LEFTSHIFT | 54 => Some(2),
            KEY_LEFTMETA | 126 => Some(3),
            _ => None,
        }
    }

    /// Push the current modifier/lock mask to the compositor.
    ///
    /// The virtual keyboard protocol requires this: wlroots does not update the
    /// keyboard's xkb state from `key` requests, so without this request
    /// Shift/Ctrl/Alt would never reach clients (no `wl_keyboard.modifiers`).
    fn push_modifiers(&mut self) {
        let mut depressed = 0u32;
        for (idx, active) in self.mods_active.iter().enumerate() {
            if *active {
                depressed |= MOD_MASKS[idx];
            }
        }
        let locked = self.locked_mask;
        if depressed == self.mods_sent && locked == self.locked_sent {
            return;
        }
        self.mods_sent = depressed;
        self.locked_sent = locked;
        if let Some(vk) = &self.vk {
            vk.modifiers(depressed, 0, locked, 0);
        }
    }

    /// Reconcile the physical modifier keys with the modifier flags the browser
    /// sent, without touching the modifier index belonging to `skip`.
    fn sync_modifiers(&mut self, desired: [bool; 4], skip: Option<usize>) {
        for idx in 0..4 {
            if Some(idx) == skip || desired[idx] == self.mods_active[idx] {
                continue;
            }
            self.mods_active[idx] = desired[idx];
            if desired[idx] {
                self.mod_pressed[idx] = MOD_KEYS[idx];
                self.raw_key(MOD_KEYS[idx], true);
            } else {
                let code = match self.mod_pressed[idx] {
                    0 => MOD_KEYS[idx],
                    code => code,
                };
                self.mod_pressed[idx] = 0;
                self.raw_key(code, false);
            }
        }
    }

    /// Undo modifier changes made for a synthetic one-shot key press.
    fn restore_modifiers(&mut self, before: [bool; 4]) {
        for idx in 0..4 {
            if before[idx] == self.mods_active[idx] {
                continue;
            }
            self.mods_active[idx] = before[idx];
            if before[idx] {
                self.mod_pressed[idx] = MOD_KEYS[idx];
                self.raw_key(MOD_KEYS[idx], true);
            } else {
                let code = match self.mod_pressed[idx] {
                    0 => MOD_KEYS[idx],
                    code => code,
                };
                self.mod_pressed[idx] = 0;
                self.raw_key(code, false);
            }
        }
        self.push_modifiers();
    }

    fn tap_key(&mut self, code: u32) {
        self.raw_key(code, true);
        self.raw_key(code, false);
    }

    fn key_event(&mut self, code: Option<u32>, key: &str, down: bool, tap: bool, mods: Mods) {
        if self.vk.is_none() {
            if down {
                legacy_key(key, mods);
            }
            return;
        }
        let desired = mods.as_array();
        let before = self.mods_active;
        match code {
            Some(code) => {
                if let Some(idx) = Self::modifier_index(code) {
                    // Physical modifier key: its own state comes from the event.
                    self.raw_key(code, down);
                    self.mods_active[idx] = down;
                    self.mod_pressed[idx] = if down { code } else { 0 };
                    self.sync_modifiers(desired, Some(idx));
                    self.push_modifiers();
                } else if down && (code == KEY_CAPSLOCK || code == KEY_NUMLOCK) {
                    // Locks are part of the mask we maintain ourselves.
                    self.tap_key(code);
                    self.locked_mask ^= if code == KEY_CAPSLOCK {
                        XKB_LOCK
                    } else {
                        XKB_MOD2
                    };
                    self.push_modifiers();
                } else {
                    self.sync_modifiers(desired, None);
                    self.push_modifiers();
                    self.raw_key(code, down);
                    if tap {
                        self.raw_key(code, false);
                        self.restore_modifiers(before);
                    }
                }
            }
            None => {
                // Unknown physical code: fall back to the keysym-ish `key`
                // value (single character via the US mapping, otherwise a key
                // name such as "Escape") so press/release still pair up.
                let mut chars = key.chars();
                let mapped = match (chars.next(), chars.next()) {
                    (Some(ch), None) => us_char(ch),
                    _ => named_key_code(key).map(|code| (code, false)),
                };
                if let Some((kc, shift)) = mapped {
                    if shift {
                        let mut flags = desired;
                        flags[2] = true;
                        self.sync_modifiers(flags, None);
                    } else {
                        self.sync_modifiers(desired, None);
                    }
                    self.push_modifiers();
                    self.tap_key(kc);
                    if tap {
                        self.restore_modifiers(before);
                    }
                }
            }
        }
    }

    /// Type text by synthesising key strokes from the US layout (only used
    /// when `wtype` is unavailable; `wtype` handles real layouts).
    fn type_text_fallback(&mut self, text: &str) {
        if self.vk.is_none() {
            return;
        }
        for ch in text.chars() {
            let Some((code, shift)) = us_char(ch) else {
                continue;
            };
            let before = self.mods_active;
            if shift {
                self.mods_active[2] = true;
                self.raw_key(KEY_LEFTSHIFT, true);
            }
            self.push_modifiers();
            self.tap_key(code);
            self.restore_modifiers(before);
        }
    }

    fn release_all(&mut self) {
        for code in self.pressed_keys.clone() {
            self.raw_key(code, false);
        }
        self.pressed_keys.clear();
        self.mod_pressed = [0; 4];
        self.mods_active = [false; 4];
        self.locked_mask = 0;
        if let Some(vk) = &self.vk {
            vk.modifiers(0, 0, 0, 0);
        }
        self.mods_sent = 0;
        self.locked_sent = 0;
        if let Some(vp) = &self.vp {
            for code in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                vp.button(self.timestamp(), code, wl_pointer::ButtonState::Released);
            }
            vp.frame();
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch implementations (only the events we actually need)
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for InputState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for InputState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let WEnum::Value(caps) = capabilities else {
                return;
            };
            dlog!("seat capabilities: {caps:?}");
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                dlog!("requesting wl_keyboard");
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for InputState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            let is_xkb_v1 = match format {
                WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) => true,
                // Be permissive if a compositor sends an unknown tag that
                // happens to match the XKB v1 format number.
                WEnum::Unknown(1) => true,
                _ => false,
            };
            if !is_xkb_v1 {
                return;
            }
            let file = std::fs::File::from(fd);
            dlog!("keymap event: {size} bytes");
            let mut data = Vec::with_capacity(size as usize);
            if file.take(size as u64).read_to_end(&mut data).is_ok() && !data.is_empty() {
                state.keymap = Some(data);
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for InputState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode {
            flags,
            width,
            height,
            ..
        } = event
        {
            let current = match flags {
                WEnum::Value(f) => f.contains(wl_output::Mode::Current),
                WEnum::Unknown(_) => false,
            };
            if current && width > 1 && height > 1 {
                state.extent = ((width - 1) as u32, (height - 1) as u32);
            }
        }
    }
}

impl Dispatch<VirtualKeyboardManager, ()> for InputState {
    fn event(
        _: &mut Self,
        _: &VirtualKeyboardManager,
        _: <VirtualKeyboardManager as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<VirtualKeyboard, ()> for InputState {
    fn event(
        _: &mut Self,
        _: &VirtualKeyboard,
        _: <VirtualKeyboard as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<VirtualPointerManager, ()> for InputState {
    fn event(
        _: &mut Self,
        _: &VirtualPointerManager,
        _: <VirtualPointerManager as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<VirtualPointer, ()> for InputState {
    fn event(
        _: &mut Self,
        _: &VirtualPointer,
        _: <VirtualPointer as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// ---------------------------------------------------------------------------
// Resident native session
// ---------------------------------------------------------------------------

enum RunOutcome {
    Closed,
    Broken(anyhow::Error),
}

struct Native {
    conn: Connection,
    queue: EventQueue<InputState>,
    state: InputState,
    /// Throttles the per-motion debug log to one line per 20 ms.
    last_move_log: Instant,
    /// A press and release arriving in one network batch must not collapse
    /// into a zero-duration click (some clients sample buttons per frame).
    button_pressed_at: [Option<Instant>; 5],
}

impl Native {
    fn connect() -> anyhow::Result<Self> {
        use anyhow::{anyhow, Context};

        let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
        dlog!("connecting (WAYLAND_DISPLAY={display:?})");
        let conn = Connection::connect_to_env()
            .context("cannot connect to the Wayland compositor (WAYLAND_DISPLAY)")?;
        let (globals, mut queue) = registry_queue_init::<InputState>(&conn)
            .context("cannot read the Wayland registry")?;
        let qh = queue.handle();

        let mut state = InputState::default();
        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=7, ())
            .context("compositor has no wl_seat")?;
        state.seat = Some(seat.clone());

        let keyboard_manager = globals
            .bind::<VirtualKeyboardManager, _, _>(&qh, 1..=1, ())
            .ok();
        let pointer_manager = globals
            .bind::<VirtualPointerManager, _, _>(&qh, 1..=1, ())
            .ok();
        // Optional: only used to learn the pointer coordinate space.
        let _output = globals
            .bind::<wl_output::WlOutput, _, _>(&qh, 1..=2, ())
            .ok();

        dlog!(
            "globals: vk_manager={} vp_manager={}",
            keyboard_manager.is_some(),
            pointer_manager.is_some()
        );

        // First roundtrip: seat capabilities + output mode.  The second one is
        // needed because `wl_keyboard` (created while dispatching the
        // capabilities event) only gets its keymap event afterwards -- this is
        // where the user's real xkb layout comes from in a normal session.
        queue.roundtrip(&mut state).context("wayland roundtrip")?;
        queue.roundtrip(&mut state).context("wayland roundtrip")?;

        let Some(keyboard_manager) = keyboard_manager else {
            return Err(anyhow!(
                "compositor does not advertise zwp_virtual_keyboard_manager_v1"
            ));
        };

        let vk = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());
        state.vk = Some(vk);
        if let Some(pointer_manager) = pointer_manager {
            state.vp = Some(pointer_manager.create_virtual_pointer(Some(&seat), &qh, ()));
        }

        let from_compositor = state.keymap.is_some();
        let (keymap, keymap_source) = match state.keymap.clone() {
            Some(map) => (map, "compositor"),
            None => (FALLBACK_KEYMAP.to_vec(), "builtin-us"),
        };
        state.keymap_from_compositor = from_compositor;
        dlog!(
            "devices created; pointer={} extent={:?} keymap={} ({} bytes)",
            state.vp.is_some(),
            state.extent,
            keymap_source,
            keymap.len()
        );
        let mut native = Self {
            conn,
            queue,
            state,
            last_move_log: Instant::now(),
            button_pressed_at: [None; 5],
        };
        // The keymap has to reach the compositor in the same batch as the
        // device creation: if the keyboard is registered first the seat may be
        // published without a keymap and clients will never see one.
        native.apply_keymap(&keymap)?;
        if let Some(vk) = &native.state.vk {
            vk.modifiers(0, 0, 0, 0);
        }
        native.conn.flush().context("wayland flush")?;
        // No extra roundtrip here on purpose: everything that has to be
        // ordered before the first key (device creation, keymap, modifier
        // state) is already in the socket, and a blocking roundtrip would make
        // session setup sensitive to compositor stalls.
        dlog!("native input session ready");
        Ok(native)
    }

    fn apply_keymap(&mut self, keymap: &[u8]) -> anyhow::Result<()> {
        use anyhow::Context;

        let Some(vk) = &self.state.vk else {
            return Ok(());
        };
        let mut data = keymap.to_vec();
        if data.last() != Some(&0) {
            data.push(0);
        }
        let size = data.len() as u32;
        // The compositor mmap()s this fd, so it must be a real (anonymous)
        // file, not a pipe.  The file is unlinked right after creation; the
        // compositor receives a dup of it over SCM_RIGHTS.
        let mut file = anonymous_file(&data).context("anonymous keymap file")?;
        file.seek(SeekFrom::Start(0)).context("rewind keymap file")?;
        dlog!("keymap file ready ({size} bytes), sending request");
        vk.keymap(KEYMAP_FORMAT_XKB_V1, file.as_fd(), size);
        self.conn.flush().context("flush keymap")?;
        drop(file);
        dlog!("keymap request flushed");
        Ok(())
    }

    fn handle(&mut self, event: Event) -> anyhow::Result<()> {
        // One debug line per accepted event (`SWAYRTC_DEBUG=1`), so end-to-end
        // tests can assert delivery mechanically.  Motion is throttled.
        match &event {
            Event::Move { x, y } => {
                if debug_logging()
                    && self.last_move_log.elapsed() >= Duration::from_millis(20)
                {
                    self.last_move_log = Instant::now();
                    dlog!("event move x={x} y={y}");
                }
            }
            Event::MoveRelative { dx, dy } => {
                if debug_logging()
                    && self.last_move_log.elapsed() >= Duration::from_millis(20)
                {
                    self.last_move_log = Instant::now();
                    dlog!("event move relative dx={dx} dy={dy}");
                }
            }
            Event::Button {
                button,
                down,
                x,
                y,
            } => dlog!("event button button={button} down={down} x={x:?} y={y:?}"),
            Event::Wheel { x, y } => dlog!("event wheel dx={x} dy={y}"),
            Event::Key {
                code,
                key,
                down,
                tap,
                mods,
            } => dlog!(
                "event key code={code:?} key={key:?} down={down} tap={tap} mods={mods:?}"
            ),
            Event::Text(text) => dlog!(
                "event text len={} ascii={}",
                text.len(),
                text.is_ascii()
            ),
        }
        match event {
            Event::Move { x, y } => self.state.motion(x, y),
            Event::MoveRelative { dx, dy } => self.state.motion_relative(dx, dy),
            Event::Button {
                button,
                down,
                x,
                y,
            } => {
                // Immediately flush the press so the compositor sees it before
                // a release delivered in the same ordered DataChannel batch.
                // A ~35 ms minimum dwell lets even 60 Hz polling clients
                // observe the pressed state (touch taps previously produced
                // press+release in the same millisecond).
                const MIN_CLICK_HOLD: Duration = Duration::from_millis(35);
                let index = (button as usize).min(4);
                if down {
                    self.state.button(button, true, x, y);
                    self.conn.flush()?;
                    self.button_pressed_at[index] = Some(Instant::now());
                } else {
                    if let Some(started) = self.button_pressed_at[index].take() {
                        if let Some(remaining) = MIN_CLICK_HOLD.checked_sub(started.elapsed()) {
                            std::thread::sleep(remaining);
                        }
                    }
                    self.state.button(button, false, x, y);
                }
            },
            Event::Wheel { x, y } => self.state.axis(x, y),
            Event::Key {
                code,
                key,
                down,
                tap,
                mods,
            } => self.state.key_event(code, &key, down, tap, mods),
            Event::Text(text) => self.text(&text),
        }
        Ok(())
    }

    /// Inject a `{"type":"text"}` payload.
    ///
    /// ASCII goes through `wtype` (layout aware, fast, no clipboard side
    /// effects).  Anything else is composed text -- CJK, emoji -- which a
    /// virtual keyboard cannot type, so it is pasted through the clipboard.
    fn text(&mut self, text: &str) {
        if text.is_ascii() {
            if !spawn_wtype(text) {
                self.state.type_text_fallback(text);
            }
            return;
        }
        if self.paste_text(text) {
            return;
        }
        // Clipboard tooling missing or failed: keep the old behaviour so the
        // feature degrades instead of silently dropping the text.
        dlog!("clipboard paste unavailable for non-ASCII text; using key strokes");
        if !spawn_wtype(text) {
            self.state.type_text_fallback(text);
        }
    }

    /// Paste `text` via the Wayland clipboard (snapshot, wl-copy, Ctrl+V,
    /// deferred restore).  Returns false if `wl-copy` is missing or failed.
    fn paste_text(&mut self, text: &str) -> bool {
        let snapshot = clipboard_snapshot();
        if !write_clipboard(CLIPBOARD_TEXT_MIME, text.as_bytes()) {
            return false;
        }
        let shift = paste_uses_shift();
        dlog!(
            "clipboard paste: {} bytes, shortcut=Ctrl+{}V, saved={}",
            text.len(),
            if shift { "Shift+" } else { "" },
            snapshot.is_some()
        );
        self.state
            .key_event(Some(KEY_V), "v", true, true, paste_modifiers(shift));
        if let Some(snapshot) = snapshot {
            self.state.schedule_restore(
                snapshot,
                Duration::from_millis(CLIPBOARD_RESTORE_DELAY_MS),
            );
        }
        true
    }

    /// Put the clipboard back once the paste had time to be consumed.  Called
    /// from the event loop (no sleeping here) and with `force` on teardown.
    fn flush_clipboard_restore(&mut self, force: bool) {
        let snapshot = if force {
            self.state.take_pending_restore()
        } else {
            self.state.take_due_restore(Instant::now())
        };
        let Some(snapshot) = snapshot else { return };
        if write_clipboard(&snapshot.mime, &snapshot.data) {
            dlog!(
                "clipboard restored ({} bytes, {})",
                snapshot.data.len(),
                snapshot.mime
            );
        } else {
            dlog!(
                "clipboard restore failed ({} bytes, {})",
                snapshot.data.len(),
                snapshot.mime
            );
        }
    }

    /// Release everything we are holding and put the clipboard back.
    fn shutdown(&mut self) {
        self.state.release_all();
        self.flush_clipboard_restore(true);
    }

    fn run(&mut self, rx: &std_mpsc::Receiver<Event>) -> RunOutcome {
        // Give the compositor a moment to publish the new keyboard to the
        // focused client (it has to create its wl_keyboard before it can
        // receive key events).  Events that arrive meanwhile stay queued.
        std::thread::sleep(Duration::from_millis(40));
        loop {
            // While a clipboard restore is pending poll faster so the user's
            // clipboard comes back promptly after the paste landed.
            let idle = if self.state.has_pending_restore() {
                Duration::from_millis(20)
            } else {
                Duration::from_millis(250)
            };
            match rx.recv_timeout(idle) {
                Ok(event) => {
                    if let Err(err) = self.handle(event) {
                        self.shutdown();
                        return RunOutcome::Broken(err);
                    }
                    // Drain whatever else arrived while we were busy so one
                    // flush covers the whole batch (lower latency).
                    loop {
                        match rx.try_recv() {
                            Ok(next) => {
                                if let Err(err) = self.handle(next) {
                                    self.shutdown();
                                    return RunOutcome::Broken(err);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    if let Err(err) = self.conn.flush() {
                        self.shutdown();
                        return RunOutcome::Broken(err.into());
                    }
                    if debug_logging() {
                        // Wall clock at the moment the requests reached the
                        // compositor, so the host side of the input path can be
                        // measured against the host's frame timestamps.
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis())
                            .unwrap_or(0);
                        dlog!("event=inject.flushed ts={now}");
                    }
                }
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(err) = self.queue.dispatch_pending(&mut self.state) {
                        self.shutdown();
                        return RunOutcome::Broken(err.into());
                    }
                    let _ = self.conn.flush();
                }
                Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    self.shutdown();
                    let _ = self.conn.flush();
                    return RunOutcome::Closed;
                }
            }
            self.flush_clipboard_restore(false);
            // Pick up compositor-side events (late keymap, seat changes).
            match self.queue.dispatch_pending(&mut self.state) {
                Ok(_) => {}
                Err(err) => {
                    self.shutdown();
                    return RunOutcome::Broken(err.into());
                }
            }
            // A compositor keymap can arrive after connection setup (e.g. when
            // the seat only becomes keyboard-focused later).  Upgrade to it as
            // long as we have not typed anything yet, so the user's real layout
            // is used whenever it is available.
            if !self.state.keymap_from_compositor && !self.state.keys_sent {
                if let Some(keymap) = self.state.keymap.clone() {
                    if let Err(err) = self.apply_keymap(&keymap) {
                        return RunOutcome::Broken(err);
                    }
                    self.state.keymap_from_compositor = true;
                    dlog!("applied late compositor keymap ({} bytes)", keymap.len());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Legacy subprocess fallback (wtype + wlrctl)
// ---------------------------------------------------------------------------

fn run_quiet(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Clipboard paste helpers (composed / non-ASCII text)
// ---------------------------------------------------------------------------

fn clipboard_timeout() -> Duration {
    Duration::from_millis(CLIPBOARD_TIMEOUT_MS)
}

/// `Ctrl+V` pastes in most apps; terminals (and a few toolkits) want
/// `Ctrl+Shift+V`, which `SWAYRTC_PASTE_SHIFT=1` selects.
fn paste_uses_shift() -> bool {
    matches!(
        std::env::var("SWAYRTC_PASTE_SHIFT").as_deref(),
        Ok("1") | Ok("true")
    )
}

fn paste_modifiers(shift: bool) -> Mods {
    Mods {
        ctrl: true,
        alt: false,
        shift,
        meta: false,
    }
}

/// Read the current text clipboard.
///
/// Only text MIME types are snapshotted: a huge image/video selection must
/// never be pulled into the host, and restoring it could not be done faithfully.
fn clipboard_snapshot() -> Option<ClipboardSnapshot> {
    for mime in ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"] {
        let Some(data) = run_capture(
            "wl-paste",
            &["--no-newline", "--type", mime],
            clipboard_timeout(),
        ) else {
            continue;
        };
        // Too large to round-trip cheaply: leave the clipboard alone.
        if data.len() > MAX_CLIPBOARD_SNAPSHOT {
            return None;
        }
        dlog!("clipboard snapshot: {} bytes ({mime})", data.len());
        return Some(ClipboardSnapshot {
            mime: mime.to_owned(),
            data,
        });
    }
    None
}

/// Become the clipboard owner with `data`.
///
/// The payload always goes over stdin -- never through a shell -- so it cannot
/// be interpreted as arguments or shell syntax.
fn write_clipboard(mime: &str, data: &[u8]) -> bool {
    if data.len() > MAX_CLIPBOARD_SNAPSHOT {
        return false;
    }
    run_with_stdin("wl-copy", &["--type", mime], data, clipboard_timeout())
}

/// Run `bin` with `input` on stdin, giving it `timeout` to exit successfully.
fn run_with_stdin(bin: &str, args: &[&str], input: &[u8], timeout: Duration) -> bool {
    let Ok(mut child) = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(input).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
    }
    matches!(wait_with_timeout(&mut child, timeout), Some(status) if status.success())
}

/// Run `bin` and capture its stdout, killing it if it outlives `timeout`.
fn run_capture(bin: &str, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    // Read on a separate thread so a chatty or stuck child cannot deadlock the
    // timeout below (the pipe closes when the process is killed).
    let reader = std::thread::spawn(move || {
        let mut stdout = stdout;
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let status = wait_with_timeout(&mut child, timeout);
    let data = reader.join().ok()?;
    match status {
        Some(status) if status.success() => Some(data),
        _ => None,
    }
}

/// Wait for `child` to exit, killing it (and reaping it) after `timeout`.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn legacy_keysym(key: &str) -> Option<&'static str> {
    Some(match key {
        "Enter" => "Return",
        "Backspace" => "BackSpace",
        "Escape" => "Escape",
        "ArrowLeft" => "Left",
        "ArrowRight" => "Right",
        "ArrowUp" => "Up",
        "ArrowDown" => "Down",
        "PageUp" => "Prior",
        "PageDown" => "Next",
        "Delete" => "Delete",
        "Insert" => "Insert",
        "Home" => "Home",
        "End" => "End",
        "Tab" => "Tab",
        " " => "space",
        "a" => "a",
        "b" => "b",
        "c" => "c",
        "d" => "d",
        "e" => "e",
        "f" => "f",
        "g" => "g",
        "h" => "h",
        "i" => "i",
        "j" => "j",
        "k" => "k",
        "l" => "l",
        "m" => "m",
        "n" => "n",
        "o" => "o",
        "p" => "p",
        "q" => "q",
        "r" => "r",
        "s" => "s",
        "t" => "t",
        "u" => "u",
        "v" => "v",
        "w" => "w",
        "x" => "x",
        "y" => "y",
        "z" => "z",
        "0" => "0",
        "1" => "1",
        "2" => "2",
        "3" => "3",
        "4" => "4",
        "5" => "5",
        "6" => "6",
        "7" => "7",
        "8" => "8",
        "9" => "9",
        "F1" => "F1",
        "F2" => "F2",
        "F3" => "F3",
        "F4" => "F4",
        "F5" => "F5",
        "F6" => "F6",
        "F7" => "F7",
        "F8" => "F8",
        "F9" => "F9",
        "F10" => "F10",
        "F11" => "F11",
        "F12" => "F12",
        "-" => "minus",
        "=" => "equal",
        "[" => "bracketleft",
        "]" => "bracketright",
        "\\" => "backslash",
        ";" => "semicolon",
        "'" => "apostrophe",
        "," => "comma",
        "." => "period",
        "/" => "slash",
        "`" => "grave",
        _ => return None,
    })
}

fn legacy_key(key: &str, mods: Mods) {
    let Some(keysym) = legacy_keysym(key) else {
        return;
    };
    let mut args: Vec<String> = Vec::new();
    for (flag, name) in [
        (mods.ctrl, "ctrl"),
        (mods.alt, "alt"),
        (mods.shift, "shift"),
        (mods.meta, "logo"),
    ] {
        if flag {
            args.push("-M".to_owned());
            args.push(name.to_owned());
        }
    }
    args.push("-k".to_owned());
    args.push(keysym.to_owned());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_quiet("wtype", &refs);
}

/// Type text with wtype (layout aware).  Returns false when wtype could not run
/// so the caller can fall back to synthetic key codes.
fn spawn_wtype(text: &str) -> bool {
    let Ok(mut child) = Command::new("wtype")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        if stdin.write_all(text.as_bytes()).is_err() {
            let _ = child.kill();
            return false;
        }
    }
    matches!(child.wait(), Ok(status) if status.success())
}

fn legacy_run(rx: &std_mpsc::Receiver<Event>) {
    let mut pointer = LegacyPointer::default();
    while let Ok(event) = rx.recv() {
        match event {
            Event::Move { x, y } => pointer.move_to(clamp_i(x, 1919), clamp_i(y, 1079)),
            Event::MoveRelative { dx, dy } => pointer.move_by(dx, dy),
            Event::Button {
                button,
                down,
                x,
                y,
            } => {
                if let (Some(x), Some(y)) = (x, y) {
                    pointer.move_to(clamp_i(x, 1919), clamp_i(y, 1079));
                }
                if down {
                    pointer.button(button);
                }
            }
            Event::Wheel { x, y } => pointer.scroll(x, y),
            Event::Key { key, down, mods, .. } => {
                if down {
                    legacy_key(&key, mods);
                }
            }
            Event::Text(text) => {
                spawn_wtype(&text);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worker thread plumbing
// ---------------------------------------------------------------------------

fn input_thread(rx: std_mpsc::Receiver<Event>) {
    // `SWAYRTC_INPUT_BACKEND=legacy` forces the old wtype/wlrctl path; useful
    // as an operational escape hatch and for testing the fallback.
    if std::env::var("SWAYRTC_INPUT_BACKEND").as_deref() == Ok("legacy") {
        eprintln!("swayrtc-input: legacy backend forced by SWAYRTC_INPUT_BACKEND");
        legacy_run(&rx);
        return;
    }
    let mut attempts = 0u32;
    loop {
        match Native::connect() {
            Ok(mut native) => {
                attempts = 0;
                match native.run(&rx) {
                    RunOutcome::Closed => return,
                    RunOutcome::Broken(err) => {
                        eprintln!("swayrtc-input: native input session lost: {err:#}");
                        attempts += 1;
                        if attempts > 2 {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(300));
                    }
                }
            }
            Err(err) => {
                eprintln!("swayrtc-input: native Wayland input unavailable: {err:#}");
                attempts += 1;
                if attempts > 2 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300 * attempts as u64));
            }
        }
    }
    eprintln!("swayrtc-input: falling back to wtype/wlrctl subprocesses");
    legacy_run(&rx);
}

/// Input worker: validates DataChannel payloads and forwards them to the
/// resident Wayland input thread.
pub async fn input_worker(mut rx: mpsc::Receiver<Value>) {
    let (tx, std_rx) = std_mpsc::channel::<Event>();
    if std::thread::Builder::new()
        .name("swayrtc-input".to_owned())
        .spawn(move || input_thread(std_rx))
        .is_err()
    {
        eprintln!("swayrtc-input: cannot spawn input thread; dropping control input");
        while rx.recv().await.is_some() {}
        return;
    }
    while let Some(value) = rx.recv().await {
        if let Some(event) = parse_event(&value) {
            if tx.send(event).is_err() {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_move_with_clamping() {
        let ev = parse_event(&json!({"type":"move","x":-5.0,"y":2000.0})).unwrap();
        assert_eq!(ev, Event::Move { x: 0, y: 2000 });
    }

    #[test]
    fn rejects_non_finite_and_huge_coordinates() {
        assert!(parse_event(&json!({"type":"move","x":1e300,"y":0})).is_none());
        assert!(parse_event(&json!({"type":"move","x":0,"y":"1"})).is_none());
    }

    #[test]
    fn parses_button_press_and_release() {
        let down = parse_event(&json!({"type":"button","button":0,"down":true,"x":10,"y":20}))
            .unwrap();
        assert_eq!(
            down,
            Event::Button { button: 0, down: true, x: Some(10), y: Some(20) }
        );
        let up = parse_event(&json!({"type":"button","button":2,"down":false})).unwrap();
        assert_eq!(up, Event::Button { button: 2, down: false, x: None, y: None });
        assert!(parse_event(&json!({"type":"button","button":0})).is_none());
        assert!(parse_event(&json!({"type":"button","button":9,"down":true})).is_none());
    }

    #[test]
    fn wheel_is_clamped() {
        let ev = parse_event(&json!({"type":"wheel","x":-1000,"y":5000})).unwrap();
        assert_eq!(ev, Event::Wheel { x: -250.0, y: 250.0 });
    }

    #[test]
    fn key_events_use_code_and_drop_repeats() {
        let down = parse_event(&json!({
            "type":"key","key":"a","code":"KeyA","down":true,"repeat":false,
            "ctrlKey":true,"altKey":false,"shiftKey":false,"metaKey":false
        }))
        .unwrap();
        match down {
            Event::Key { code, down, tap, mods, .. } => {
                assert_eq!(code, Some(30));
                assert!(down);
                assert!(!tap);
                assert_eq!(mods, Mods { ctrl: true, ..Default::default() });
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(parse_event(&json!({
            "type":"key","key":"a","code":"KeyA","down":true,"repeat":true
        }))
        .is_none());
        assert!(parse_event(&json!({
            "type":"key","key":"a","code":"KeyA","down":false,"repeat":false
        }))
        .is_some());
    }

    #[test]
    fn panel_key_is_a_tap() {
        let ev = parse_event(&json!({"type":"key","key":"Escape","down":true})).unwrap();
        match ev {
            Event::Key { code, tap, down, .. } => {
                assert_eq!(code, None);
                assert!(down);
                assert!(tap);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn text_validation() {
        assert_eq!(
            parse_event(&json!({"type":"text","text":"hello"})),
            Some(Event::Text("hello".into()))
        );
        assert!(parse_event(&json!({"type":"text","text":""})).is_none());
        assert!(parse_event(&json!({"type":"text","text":"a\u{1}b"})).is_none());
        // The cap is measured in bytes.
        assert!(parse_event(&json!({"type":"text","text":"x".repeat(MAX_TEXT_LEN)})).is_some());
        assert!(parse_event(&json!({"type":"text","text":"x".repeat(MAX_TEXT_LEN + 1)})).is_none());
    }

    #[test]
    fn non_ascii_text_is_accepted() {
        // Composed text (IME output) must reach the clipboard path intact.
        let ja = "こんにちは、世界";
        assert_eq!(
            parse_event(&json!({"type":"text","text":ja})),
            Some(Event::Text(ja.to_owned()))
        );
        // NUL, ESC and other control characters are still rejected outright:
        // they would be pasted verbatim into the remote application.
        assert!(parse_event(&json!({"type":"text","text":"a\u{0}b"})).is_none());
        assert!(parse_event(&json!({"type":"text","text":"\u{1b}[2J"})).is_none());
        // Newline and tab are the only allowed control characters.
        assert!(parse_event(&json!({"type":"text","text":"a\nb\tc"})).is_some());
        // A multi-byte payload is capped by bytes, not characters.
        let max_chars = MAX_TEXT_LEN / 'あ'.len_utf8();
        assert!(parse_event(&json!({"type":"text","text":"あ".repeat(max_chars)})).is_some());
        assert!(
            parse_event(&json!({"type":"text","text":"あ".repeat(max_chars + 1)})).is_none()
        );
    }

    #[test]
    fn parses_relative_move_and_clamps_deltas() {
        assert_eq!(
            parse_event(&json!({"type":"move","relative":true,"dx":12.4,"dy":-8})),
            Some(Event::MoveRelative { dx: 12, dy: -8 })
        );
        // Deltas are clamped so a hostile viewer cannot teleport the cursor.
        assert_eq!(
            parse_event(&json!({"type":"move","relative":true,"dx":9000,"dy":-9000})),
            Some(Event::MoveRelative {
                dx: MAX_REL_MOTION,
                dy: -MAX_REL_MOTION
            })
        );
        // A relative event without usable deltas is dropped, not treated as 0,0.
        assert!(parse_event(&json!({"type":"move","relative":true})).is_none());
        assert!(parse_event(&json!({"type":"move","relative":true,"dx":1})).is_none());
        assert!(parse_event(&json!({"type":"move","relative":true,"dx":1e300,"dy":1})).is_none());
        // Absolute motion stays the default and ignores stray dx/dy.
        assert_eq!(
            parse_event(&json!({"type":"move","x":10,"y":20})),
            Some(Event::Move { x: 10, y: 20 })
        );
        assert_eq!(
            parse_event(&json!({"type":"move","relative":false,"x":3,"y":4,"dx":99})),
            Some(Event::Move { x: 3, y: 4 })
        );
    }

    #[test]
    fn paste_shortcut_defaults_to_ctrl_v() {
        assert_eq!(
            paste_modifiers(false),
            Mods {
                ctrl: true,
                ..Default::default()
            }
        );
        assert_eq!(
            paste_modifiers(true),
            Mods {
                ctrl: true,
                shift: true,
                ..Default::default()
            }
        );
        // Ctrl+V is `KEY_V` with the Ctrl modifier; `key_event` pushes
        // XKB_CTRL so clients see mods_depressed=4.
        assert_eq!(KEY_V, 47);
        assert_eq!(MOD_MASKS[0], XKB_CTRL);
    }

    #[test]
    fn clipboard_restore_respects_deadline() {
        let snapshot = ClipboardSnapshot {
            mime: CLIPBOARD_TEXT_MIME.to_owned(),
            data: b"previous".to_vec(),
        };
        let start = Instant::now();
        let mut state = InputState::default();
        state.schedule_restore_at(
            snapshot.clone(),
            start + Duration::from_millis(CLIPBOARD_RESTORE_DELAY_MS),
        );
        assert!(state.has_pending_restore());
        // Not due yet: the paste is still in flight.
        assert_eq!(state.take_due_restore(start), None);
        assert!(state.has_pending_restore());
        assert_eq!(
            state.take_due_restore(start + Duration::from_millis(CLIPBOARD_RESTORE_DELAY_MS)),
            Some(snapshot)
        );
        assert!(!state.has_pending_restore());
        assert_eq!(state.take_due_restore(start), None);

        // Teardown ignores the deadline, and a taken restore is gone.
        let mut state = InputState::default();
        state.schedule_restore(
            ClipboardSnapshot {
                mime: "text/plain".to_owned(),
                data: b"x".to_vec(),
            },
            Duration::from_secs(3600),
        );
        assert!(state.take_pending_restore().is_some());
        assert_eq!(state.take_pending_restore(), None);
    }

    #[test]
    fn meta_and_super_keys_map_to_leftmeta() {
        // ChromeOS calls it "Search"; the browser reports it as either the
        // legacy OSLeft/OSRight or MetaLeft/MetaRight, plus SuperLeft aliases.
        for code in ["MetaLeft", "OSLeft", "SuperLeft"] {
            assert_eq!(evdev_keycode(code), Some(KEY_LEFTMETA), "{code}");
        }
        for code in ["MetaRight", "OSRight", "SuperRight"] {
            assert_eq!(evdev_keycode(code), Some(126), "{code}");
        }
        // The on-screen keyboard sends `key` names instead of `code`.
        for key in ["Meta", "Super", "OS"] {
            assert_eq!(named_key_code(key), Some(KEY_LEFTMETA), "{key}");
        }
        // The meta modifier goes out as Mod4 (mask 1 << 6 == 64), which is what
        // sway binds as $mod and what wev prints as "Mod4".
        assert_eq!(MOD_KEYS[3], KEY_LEFTMETA);
        assert_eq!(MOD_MASKS[3], XKB_MOD4);
        assert_eq!(XKB_MOD4, 64);
        // A Super press keeps `metaKey` so `key_event` pushes the mask.
        match parse_event(&json!({
            "type":"key","code":"MetaLeft","key":"Meta","down":true,
            "metaKey":true,"ctrlKey":false,"altKey":false,"shiftKey":false
        }))
        .unwrap()
        {
            Event::Key { code, mods, .. } => {
                assert_eq!(code, Some(KEY_LEFTMETA));
                assert!(mods.meta);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn unknown_types_are_dropped() {
        assert!(parse_event(&json!({"type":"ping","t":1})).is_none());
        assert!(parse_event(&json!({"type":"exec","cmd":"rm -rf /"})).is_none());
        assert!(parse_event(&json!({})).is_none());
    }

    #[test]
    fn keycode_table_covers_common_codes() {
        assert_eq!(evdev_keycode("KeyA"), Some(30));
        assert_eq!(evdev_keycode("Escape"), Some(1));
        assert_eq!(evdev_keycode("ArrowLeft"), Some(105));
        assert_eq!(evdev_keycode("ShiftLeft"), Some(42));
        assert_eq!(evdev_keycode("ControlRight"), Some(97));
        assert_eq!(evdev_keycode("SuperLeft"), Some(125));
        assert_eq!(evdev_keycode("F12"), Some(88));
        assert_eq!(evdev_keycode("Nope"), None);
    }

    #[test]
    fn us_layout_mapping() {
        assert_eq!(us_char('a'), Some((30, false)));
        assert_eq!(us_char('A'), Some((30, true)));
        assert_eq!(us_char('1'), Some((2, false)));
        assert_eq!(us_char('!'), Some((2, true)));
        assert_eq!(us_char('\n'), Some((28, false)));
        assert_eq!(us_char('あ'), None);
    }

    #[test]
    fn fallback_keymap_is_nul_terminated() {
        assert_eq!(FALLBACK_KEYMAP.last(), Some(&0));
        assert!(std::str::from_utf8(&FALLBACK_KEYMAP[..FALLBACK_KEYMAP.len() - 1]).is_ok());
    }

    // -------------------------------------------------------------------------
    // End to end test against a live nested compositor.
    //
    // Ignored by default.  It injects through the production plumbing
    // (DataChannel-shaped JSON -> `input_worker` -> `parse_event` -> resident
    // Wayland thread) and checks the result in real clients:
    //
    //   * `wev` sees relative pointer motion and the Super key as `Meta_L`
    //     with the Mod4 modifier,
    //   * `zenity --entry` receives Japanese text through the clipboard and
    //     the previous clipboard contents are restored afterwards,
    //   * `SWAYRTC_PASTE_SHIFT=1` turns the shortcut into Ctrl+Shift+V.
    //
    // Run it against a throwaway nested sway only, e.g.:
    //
    //   WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 \
    //     sway -d -c /tmp/nested.conf &
    //   WAYLAND_DISPLAY=wayland-2 SWAYSOCK=$XDG_RUNTIME_DIR/sway-ipc... \
    //     SWAYRTC_E2E=1 SWAYRTC_DEBUG=1 \
    //     cargo test --release --offline --bin swayrtc-host \
    //       -- --ignored --test-threads=1 --nocapture
    //
    // The `SWAYRTC_E2E=1` gate keeps it away from the user's real session.
    // -------------------------------------------------------------------------

    /// app_id of the focused toplevel, as reported by `swaymsg -t get_tree`.
    fn focused_app() -> Option<String> {
        let output = std::process::Command::new("swaymsg")
            .args(["-t", "get_tree"])
            .output()
            .ok()?;
        let tree: Value = serde_json::from_slice(&output.stdout).ok()?;
        fn walk(node: &Value) -> Option<String> {
            // Tiled windows are `con`, floating dialogs (`zenity`) are
            // wrapped in `floating_con`.
            if matches!(
                node.get("type").and_then(Value::as_str),
                Some("con") | Some("floating_con")
            ) && node.get("focused").and_then(Value::as_bool) == Some(true)
            {
                return Some(
                    node.get("app_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                );
            }
            for key in ["nodes", "floating_nodes"] {
                let Some(children) = node.get(key).and_then(Value::as_array) else {
                    continue;
                };
                for child in children {
                    if let Some(found) = walk(child) {
                        return Some(found);
                    }
                }
            }
            None
        }
        walk(&tree)
    }

    fn wait_for_focus(app: &str, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if focused_app().as_deref() == Some(app) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Last absolute `wl_pointer.motion` coordinate pair that `wev` printed.
    fn last_motion(log: &str) -> Option<(f64, f64)> {
        let line = log
            .lines()
            .rev()
            .find(|line| line.contains("motion:") && line.contains("x, y:"))?;
        let coords = line.split("x, y:").nth(1)?;
        let (x, y) = coords.split_once(',')?;
        Some((
            x.trim().parse().ok()?,
            y.trim().split_whitespace().next()?.parse().ok()?,
        ))
    }

    /// `wev` line-buffered, so its log can be read while it is running.
    fn spawn_wev(log: &std::path::Path) -> std::process::Child {
        std::process::Command::new("stdbuf")
            .args([
                "-oL",
                "wev",
                "-f",
                "wl_keyboard",
                "-f",
                "wl_pointer",
                "-f",
                "wl_seat",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                std::fs::File::create(log).expect("wev log file"),
            ))
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn wev")
    }

    #[test]
    #[ignore = "needs a live nested Wayland session (SWAYRTC_E2E=1)"]
    fn e2e_relative_super_and_unicode_paste() {
        if std::env::var("SWAYRTC_E2E").as_deref() != Ok("1") {
            eprintln!("SKIP: set SWAYRTC_E2E=1 and point WAYLAND_DISPLAY at a nested sway");
            return;
        }
        let dir = std::env::temp_dir().join(format!("swayrtc-input-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create e2e dir");

        let (tx, rx) = tokio::sync::mpsc::channel::<Value>(64);
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(input_worker(rx));
        });
        let send = |value: Value| tx.blocking_send(value).expect("input channel");

        // --- 1. relative motion + Super/Meta, observed with wev -------------
        let wev_log = dir.join("wev.log");
        let mut wev = spawn_wev(&wev_log);
        assert!(
            wait_for_focus("wev", Duration::from_secs(10)),
            "wev never got keyboard/pointer focus (check WAYLAND_DISPLAY/SWAYSOCK)"
        );
        std::thread::sleep(Duration::from_millis(300));
        send(json!({"type":"move","x":400,"y":300}));
        std::thread::sleep(Duration::from_millis(150));
        send(json!({"type":"move","relative":true,"dx":100,"dy":50}));
        std::thread::sleep(Duration::from_millis(150));
        // Button and wheel must still reach the client unchanged.
        send(json!({"type":"button","button":0,"down":true}));
        send(json!({"type":"button","button":0,"down":false}));
        send(json!({"type":"wheel","x":0,"y":120}));
        std::thread::sleep(Duration::from_millis(250));
        // ASCII text keeps using wtype and must not go through the clipboard.
        let ascii_sentinel = "swayrtc-ascii-sentinel";
        assert!(
            write_clipboard(CLIPBOARD_TEXT_MIME, ascii_sentinel.as_bytes()),
            "wl-copy is required for this test"
        );
        send(json!({"type":"text","text":"abc"}));
        std::thread::sleep(Duration::from_millis(700));
        let untouched = run_capture("wl-paste", &["--no-newline"], Duration::from_millis(2000));
        assert_eq!(
            untouched.as_deref(),
            Some(ascii_sentinel.as_bytes()),
            "ASCII text must be typed with wtype, not pasted via the clipboard"
        );
        // Super held down together with a normal key (Search+A on ChromeOS).
        send(json!({"type":"key","code":"MetaLeft","key":"Meta","down":true,"metaKey":true}));
        std::thread::sleep(Duration::from_millis(120));
        send(json!({"type":"key","code":"KeyA","key":"a","down":true,"metaKey":true}));
        std::thread::sleep(Duration::from_millis(80));
        send(json!({"type":"key","code":"KeyA","key":"a","down":false,"metaKey":true}));
        std::thread::sleep(Duration::from_millis(80));
        send(json!({"type":"key","code":"MetaLeft","key":"Meta","down":false}));
        std::thread::sleep(Duration::from_millis(400));
        let _ = wev.kill();
        let _ = wev.wait();
        let log = std::fs::read_to_string(&wev_log).unwrap_or_default();
        // wl_keyboard key codes are evdev codes + 8, so KEY_LEFTMETA (125) is
        // reported as 133.  Depending on the keymap it is named Super_L or
        // Meta_L; both must carry Mod4 (mask 64) on the modifiers event.
        assert!(
            log.contains("key: 133;"),
            "wev did not receive KEY_LEFTMETA (wl key 133) for the Super key:\n{log}"
        );
        assert!(
            log.contains("Super_L") || log.contains("Meta_L"),
            "wev did not resolve the Super key to Super_L/Meta_L:\n{log}"
        );
        assert!(
            log.contains("00000040: Mod4"),
            "wev did not receive the meta modifier (Mod4, mask 64):\n{log}"
        );
        // The Super+key combination: the key itself and the Mod4 mask.
        assert!(
            log.contains("key: 38;"),
            "wev did not receive KeyA (wl key 38) while Super was held:\n{log}"
        );
        assert!(
            log.contains("sym: a"),
            "wev did not receive the A keysym (wtype ASCII path or Super+A):\n{log}"
        );
        assert!(
            log.contains("sym: b") && log.contains("sym: c"),
            "the ASCII text path (wtype) did not type abc:\n{log}"
        );
        assert!(
            log.contains("button: 272"),
            "wev did not receive the left button (BTN_LEFT 272):\n{log}"
        );
        // 120 px of wheel is 120 * WHEEL_SCALE = 24 surface units.
        assert!(
            log.contains("axis: 0 (vertical), value: 24.000000"),
            "wev did not receive the wheel axis (24 units for 120px):\n{log}"
        );
        let (mx, my) =
            last_motion(&log).unwrap_or_else(|| panic!("wev saw no pointer motion:\n{log}"));
        assert!(
            (mx - 500.0).abs() <= 2.0 && (my - 350.0).abs() <= 2.0,
            "relative motion (400,300)+(100,50) did not land near 500,350; last motion was {mx},{my}:\n{log}"
        );

        // --- 2. Japanese text through the clipboard, observed with zenity ---
        let zenity_out = dir.join("zenity.out");
        let mut zenity = std::process::Command::new("zenity")
            .args(["--entry", "--title=swayrtc-e2e"])
            .env("GDK_BACKEND", "wayland")
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                std::fs::File::create(&zenity_out).expect("zenity log file"),
            ))
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn zenity");
        assert!(
            wait_for_focus("zenity", Duration::from_secs(10)),
            "zenity never got focus"
        );
        // Seed the clipboard first so the snapshot/restore path is observable.
        let sentinel = "swayrtc-e2e-sentinel";
        assert!(
            write_clipboard(CLIPBOARD_TEXT_MIME, sentinel.as_bytes()),
            "wl-copy is required for this test"
        );
        std::thread::sleep(Duration::from_millis(200));

        let japanese = "こんにちは、世界";
        send(json!({"type":"text","text":japanese}));
        std::thread::sleep(Duration::from_millis(1000));
        let restored = run_capture("wl-paste", &["--no-newline"], Duration::from_millis(2000));
        assert_eq!(
            restored.as_deref(),
            Some(sentinel.as_bytes()),
            "the previous clipboard was not restored after the paste"
        );

        send(json!({"type":"key","code":"Enter","key":"Enter","down":true}));
        send(json!({"type":"key","code":"Enter","key":"Enter","down":false}));
        assert!(
            wait_with_timeout(&mut zenity, Duration::from_secs(8))
                .map(|status| status.success())
                .unwrap_or(false),
            "zenity did not accept the entry (Return did not arrive)"
        );
        let typed = std::fs::read_to_string(&zenity_out).unwrap_or_default();
        assert_eq!(
            typed.trim_end_matches('\n'),
            japanese,
            "zenity did not receive the pasted Japanese text"
        );

        // --- 3. SWAYRTC_PASTE_SHIFT switches the shortcut to Ctrl+Shift+V ----
        std::env::set_var("SWAYRTC_PASTE_SHIFT", "1");
        let wev_log = dir.join("wev-shift.log");
        let mut wev = spawn_wev(&wev_log);
        assert!(
            wait_for_focus("wev", Duration::from_secs(10)),
            "wev never got focus for the shift-mode phase"
        );
        send(json!({"type":"text","text":"日本語テキスト"}));
        std::thread::sleep(Duration::from_millis(900));
        let _ = wev.kill();
        let _ = wev.wait();
        std::env::remove_var("SWAYRTC_PASTE_SHIFT");
        let log = std::fs::read_to_string(&wev_log).unwrap_or_default();
        assert!(
            log.contains("00000005"),
            "SWAYRTC_PASTE_SHIFT=1 did not send Ctrl+Shift+V (mods mask 5):\n{log}"
        );
        // Shift is held, so the keysym is the shifted level `V`.
        assert!(
            log.contains("sym: V") || log.contains("sym: v"),
            "the paste shortcut did not contain a V key:\n{log}"
        );

        drop(tx);
        let _ = worker.join();
        eprintln!("e2e artifacts: {}", dir.display());
    }
}
