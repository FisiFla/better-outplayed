//! Global hotkey listener via `RegisterHotKey`.
//!
//! # Failure is reported, never swallowed
//!
//! [`listen`] returns `Err` when the chord could not be registered, **naming the chord**. A
//! hotkey that silently does not exist is the worst outcome this feature has: the user
//! presses `Ctrl+F8` in a game, nothing happens, and nothing anywhere says why. The most
//! common cause is another process already owning the combination — which includes a second
//! copy of localplay itself, because `RegisterHotKey` registers a chord
//! **system-wide**: the CLI and the desktop app pointed at the same `config.toml` cannot
//! both hold it, and the loser is told so instead of both clipping (or, worse, one of them
//! clipping twice).
//!
//! # Off Windows
//!
//! There is no global hotkey API in this crate for other platforms: [`listen`] returns a
//! channel that never yields, and [`supported`] answers `false` so a front-end can say that
//! out loud rather than promising a keypress nobody will ever hear.

use anyhow::Result;
use std::fmt;
use std::sync::mpsc::{channel, Receiver};
#[cfg(windows)]
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

/// How long [`listen`] waits for the registering thread to report its outcome.
///
/// `RegisterHotKey` is a local call that returns in microseconds; this bound exists so that
/// a thread that somehow never reaches it cannot hang a front-end's startup.
#[cfg(windows)]
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(5);

/// Key chord to listen for. Parsed from config (spec §10, `[hotkeys] clip`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Virtual-key code, e.g. `0x77` for F8.
    pub vk: u32,
}

impl Hotkey {
    /// Accepts the `"Ctrl+F8"` form used in `config.example.toml`.
    pub fn parse(spec: &str) -> Result<Self> {
        let mut hk = Self { ctrl: false, alt: false, shift: false, vk: 0 };
        let mut key = None;
        for part in spec.split('+') {
            match part.trim().to_ascii_lowercase().as_str() {
                "ctrl" => hk.ctrl = true,
                "alt" => hk.alt = true,
                "shift" => hk.shift = true,
                other => key = Some(other.to_string()),
            }
        }
        hk.vk = parse_key(key.as_deref())?;
        Ok(hk)
    }
}

/// The chord as a user should read it: modifiers first, in a fixed order, then the key.
///
/// This is the one spelling both front-ends print — the CLI's startup line and the desktop
/// shell's "press … to clip" — so a chord typed as `ctrl+f8` in `config.toml` reaches the
/// user as `Ctrl+F8` in both. It is also what a registration failure names.
impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (set, name) in [(self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift")] {
            if set {
                write!(f, "{name}+")?;
            }
        }
        match key_name(self.vk) {
            Some(name) => f.write_str(&name),
            // Unreachable for a `Hotkey` that came from `parse`, and honest if one ever did
            // not: print the virtual-key code rather than something that looks like a key.
            None => write!(f, "VK({:#04x})", self.vk),
        }
    }
}

/// The name of a virtual-key code this module can produce, or `None` for any other.
fn key_name(vk: u32) -> Option<String> {
    match vk {
        // VK_F1..VK_F24
        0x70..=0x87 => Some(format!("F{}", vk - 0x70 + 1)),
        // VK_A..VK_Z and VK_0..VK_9 are the ASCII codes of their characters.
        v if (u32::from(b'A')..=u32::from(b'Z')).contains(&v)
            || (u32::from(b'0')..=u32::from(b'9')).contains(&v) =>
        {
            Some(char::from(v as u8).to_string())
        }
        _ => None,
    }
}

/// Whether this build can install a global hotkey at all.
///
/// `true` on Windows ([`listen`] uses `RegisterHotKey`). Off Windows the shipping pipeline
/// is the synthetic stub anyway, and a front-end that shows a chord without asking this
/// first would be promising a keypress that cannot arrive.
pub const fn supported() -> bool {
    cfg!(windows)
}

fn parse_key(key: Option<&str>) -> Result<u32> {
    let key = key.ok_or_else(|| anyhow::anyhow!("hotkey has no main key (e.g. \"Ctrl+F8\")"))?;
    if let Some(n) = key.strip_prefix('f') {
        let n: u32 = n.parse().map_err(|_| anyhow::anyhow!("bad function key: F{n}"))?;
        if (1..=24).contains(&n) {
            return Ok(0x70 + n - 1); // VK_F1..VK_F24
        }
        anyhow::bail!("function key out of range: F{n}");
    }
    let ch = key.chars().next().ok_or_else(|| anyhow::anyhow!("empty key"))?;
    if ch.is_ascii_alphanumeric() {
        return Ok(ch.to_ascii_uppercase() as u32); // VK_A..VK_Z / VK_0..VK_9
    }
    anyhow::bail!("unsupported key: {key}")
}

/// Start listening. Returns a channel that yields once per press.
///
/// **Blocks until the registration outcome is known**, so the `Err` is the truth about this
/// process rather than a guess: on Windows, `RegisterHotKey` runs on a dedicated thread that
/// owns the message queue it will receive `WM_HOTKEY` on, and the error it produces is
/// carried back to this call. The error names the chord and the likely cause.
#[cfg(windows)]
pub fn listen(hk: Hotkey) -> Result<Receiver<()>> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

    let chord = hk.to_string();
    let (tx, rx) = channel();
    // The registration outcome: `Ok(())` once the chord is held, `Err(message)` when it is
    // not. A separate channel from the press channel, because one is a handshake and the
    // other is a stream of events.
    let (report_tx, report_rx) = channel::<std::result::Result<(), String>>();
    std::thread::Builder::new()
        .name("localplay-hotkey".into())
        .spawn(move || {
            let mut modifiers = HOT_KEY_MODIFIERS(0);
            if hk.ctrl {
                modifiers |= MOD_CONTROL;
            }
            if hk.alt {
                modifiers |= MOD_ALT;
            }
            if hk.shift {
                modifiers |= MOD_SHIFT;
            }
            // SAFETY: called on a dedicated thread that owns the message queue.
            unsafe {
                if let Err(err) = RegisterHotKey(None, 1, modifiers, hk.vk) {
                    let _ = report_tx.send(Err(format!(
                        "the chord {chord} could not be registered (RegisterHotKey failed: \
                         {err}). Another application — or another localplay (the CLI and \
                         this app cannot both hold one chord) — already owns it. Nothing \
                         will happen when {chord} is pressed. Close the other application, \
                         or set a different [hotkeys] clip in config.toml and restart."
                    )));
                    return;
                }
                let _ = report_tx.send(Ok(()));
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_HOTKEY && tx.send(()).is_err() {
                        break;
                    }
                }
            }
        })?;

    match report_rx.recv_timeout(REGISTRATION_TIMEOUT) {
        Ok(Ok(())) => Ok(rx),
        Ok(Err(message)) => Err(anyhow::anyhow!(message)),
        Err(RecvTimeoutError::Timeout) => Err(anyhow::anyhow!(
            "the chord {hk} was not registered within {REGISTRATION_TIMEOUT:?}: the \
             listener thread did not report back. Treat the hotkey as NOT installed."
        )),
        Err(RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!(
            "the clip hotkey listener thread died before it could register {hk}. Treat \
             the hotkey as NOT installed."
        )),
    }
}

/// Non-Windows builds have no global hotkey; the caller polls instead.
///
/// Deliberately still `Ok`: the CLI and the desktop shell share this one call site, and off
/// Windows they get a channel that never yields. [`supported`] is how a front-end says so to
/// its user instead of showing a dead chord.
#[cfg(not(windows))]
pub fn listen(_hk: Hotkey) -> Result<Receiver<()>> {
    let (_tx, rx) = channel();
    Ok(rx)
}

/// Poll helper so both platforms share one call site.
pub fn wait_for_press(rx: &Receiver<()>, timeout: Duration) -> bool {
    rx.recv_timeout(timeout).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_function_key_chord() {
        let hk = Hotkey::parse("Ctrl+F8").expect("Ctrl+F8 must parse");
        assert_eq!(hk, Hotkey { ctrl: true, alt: false, shift: false, vk: 0x77 });
    }

    #[test]
    fn parses_the_example_config_default() {
        // config.example.toml sets `clip = "Ctrl+F8"`; the parsed vk is VK_F8.
        let hk = Hotkey::parse("Ctrl+F8").unwrap();
        assert_eq!(hk.vk, 0x70 + 8 - 1);
    }

    #[test]
    fn rejects_a_spec_with_no_main_key() {
        let err = Hotkey::parse("Ctrl+Shift").unwrap_err();
        assert!(
            err.to_string().contains("no main key"),
            "expected a missing-main-key error, got: {err}"
        );
    }

    #[test]
    fn rejects_an_out_of_range_function_key() {
        assert!(Hotkey::parse("Ctrl+F25").is_err());
        assert!(Hotkey::parse("Ctrl+F0").is_err());
    }

    #[test]
    fn rejects_an_unsupported_symbol_key() {
        assert!(Hotkey::parse("Ctrl+@").is_err());
    }

    #[test]
    fn displays_the_chord_in_a_fixed_readable_form() {
        // The same spelling both front-ends print, whatever case or order the config used.
        assert_eq!(Hotkey::parse("Ctrl+F8").unwrap().to_string(), "Ctrl+F8");
        assert_eq!(Hotkey::parse("ctrl+f8").unwrap().to_string(), "Ctrl+F8");
        assert_eq!(Hotkey::parse("shift+ctrl+alt+f24").unwrap().to_string(), "Ctrl+Alt+Shift+F24");
        assert_eq!(Hotkey::parse("Alt+A").unwrap().to_string(), "Alt+A");
        assert_eq!(Hotkey::parse("Ctrl+7").unwrap().to_string(), "Ctrl+7");
        assert_eq!(Hotkey::parse("F1").unwrap().to_string(), "F1");
    }

    #[test]
    fn the_displayed_chord_parses_back_to_the_same_hotkey() {
        // What the UI shows is what the listener holds: a display that dropped a modifier
        // would tell the user to press a chord that does nothing.
        for spec in ["Ctrl+F8", "Ctrl+Alt+Shift+K", "Shift+F12", "Ctrl+9"] {
            let parsed = Hotkey::parse(spec).unwrap();
            assert_eq!(Hotkey::parse(&parsed.to_string()).unwrap(), parsed, "round trip: {spec}");
        }
    }

    #[test]
    fn a_chord_outside_the_key_names_prints_its_virtual_key_code() {
        // No `parse` path can build this, and the display must not invent a key name for it.
        let odd = Hotkey { ctrl: true, alt: false, shift: false, vk: 0x5B };
        assert_eq!(odd.to_string(), "Ctrl+VK(0x5b)");
    }

    #[test]
    fn a_global_hotkey_exists_only_on_windows() {
        // The desktop shell asks this before telling a user which chord to press: off
        // Windows `listen` returns a channel nothing ever writes to.
        assert_eq!(supported(), cfg!(windows));
    }

    #[test]
    fn listening_off_windows_reports_no_failure_and_never_fires() {
        // The non-Windows stub is a working "there is no hotkey here", not an error: the CLI
        // and the desktop shell share this call site. `wait_for_press` times out, which is
        // what its own callers expect.
        let chord = Hotkey::parse("Ctrl+F8").unwrap();
        let rx = listen(chord).expect("the non-Windows listener is a stub, not a failure");
        assert!(!wait_for_press(&rx, Duration::from_millis(50)), "nothing may fire off Windows");
        if !cfg!(windows) {
            assert!(!supported());
        }
    }
}
