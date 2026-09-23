//! Global hotkey listener via `RegisterHotKey`.

use anyhow::Result;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

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
#[cfg(windows)]
pub fn listen(hk: Hotkey) -> Result<Receiver<()>> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("localplay-hotkey".into())
        .spawn(move || {
            let mut modifiers = 0u32;
            if hk.ctrl {
                modifiers |= MOD_CONTROL.0;
            }
            if hk.alt {
                modifiers |= MOD_ALT.0;
            }
            if hk.shift {
                modifiers |= MOD_SHIFT.0;
            }
            // SAFETY: called on a dedicated thread that owns the message queue.
            unsafe {
                if RegisterHotKey(None, 1, modifiers, hk.vk).is_err() {
                    return;
                }
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    if msg.message == WM_HOTKEY && tx.send(()).is_err() {
                        break;
                    }
                }
            }
        })?;
    Ok(rx)
}

/// Non-Windows builds have no global hotkey; the caller polls instead.
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
}
