//! The audio endpoints a person can choose between: which microphone to record and which output
//! the system-audio loopback listens to.
//!
//! Endpoints are identified by their WASAPI endpoint id (`IMMDevice::GetId`), the one string that
//! stays the same across restarts and renames; the friendly name is for display only. An empty id
//! in the config means "the Windows default", which is what the backends used before a choice
//! existed. A configured id that is not present (unplugged, disabled) refuses to start rather than
//! quietly recording a different device.

use serde::Serialize;

/// One active audio endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AudioDevice {
    /// The WASAPI endpoint id, stored in the config.
    pub id: String,
    /// The name Windows shows for it, e.g. "Microphone (Blue Yeti)".
    pub name: String,
    /// Whether it is the current Windows default for its kind.
    pub is_default: bool,
}

/// Every active capture (microphone) and render (output) endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AudioDevices {
    pub inputs: Vec<AudioDevice>,
    pub outputs: Vec<AudioDevice>,
}

/// List the active endpoints. Off Windows there is nothing to choose between, so both lists are empty.
pub fn list_audio_devices() -> anyhow::Result<AudioDevices> {
    #[cfg(windows)]
    {
        windows_impl::list()
    }
    #[cfg(not(windows))]
    {
        Ok(AudioDevices::default())
    }
}

#[cfg(windows)]
pub(crate) mod windows_impl {
    use super::{AudioDevice, AudioDevices};
    use crate::platform::init_com;
    use anyhow::{Context, Result};
    use windows::core::{IUnknown, PCWSTR};
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Media::Audio::{
        eCapture, eCommunications, eConsole, eRender, EDataFlow, IMMDevice, IMMDeviceEnumerator,
        MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};

    pub(super) fn list() -> Result<AudioDevices> {
        let owned = init_com()?;
        let result = (|| {
            let enumerator = enumerator()?;
            Ok(AudioDevices {
                // The microphone default is the communications one: it is what the recorder opens.
                inputs: endpoints(&enumerator, eCapture, eCommunications.0)?,
                outputs: endpoints(&enumerator, eRender, eConsole.0)?,
            })
        })();
        if owned {
            // SAFETY: `init_com` took the apartment on this thread.
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
        result
    }

    pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator> {
        // SAFETY: the CLSID is the documented one; `None` means no aggregation.
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None::<&IUnknown>, CLSCTX_ALL) }
            .context("CoCreateInstance(MMDeviceEnumerator)")
    }

    /// The active endpoint with this id, or a message naming the id when it is gone.
    pub(crate) fn device_by_id(enumerator: &IMMDeviceEnumerator, id: &str) -> Result<IMMDevice> {
        let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let device = unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }.with_context(|| {
            format!(
                "the configured audio device {id:?} was not found: it may be unplugged or \
                 disabled. Pick another device in Settings, or choose the Windows default"
            )
        })?;
        // SAFETY: the device is alive.
        let state = unsafe { device.GetState() }.context("IMMDevice::GetState")?;
        anyhow::ensure!(
            state == DEVICE_STATE_ACTIVE,
            "the configured audio device {id:?} is not active (unplugged or disabled). Pick \
             another device in Settings, or choose the Windows default"
        );
        Ok(device)
    }

    fn endpoints(
        enumerator: &IMMDeviceEnumerator,
        flow: EDataFlow,
        default_role: i32,
    ) -> Result<Vec<AudioDevice>> {
        // SAFETY: the enumerator is alive.
        let collection = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }
            .context("IMMDeviceEnumerator::EnumAudioEndpoints")?;
        // SAFETY: the collection is alive.
        let count = unsafe { collection.GetCount() }.context("IMMDeviceCollection::GetCount")?;
        // SAFETY: as above; the role value comes from the enum the caller named.
        let default_id = unsafe {
            enumerator
                .GetDefaultAudioEndpoint(flow, windows::Win32::Media::Audio::ERole(default_role))
        }
        .ok()
        .and_then(|d| id_of(&d).ok());
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            // SAFETY: `i < count`.
            let device = unsafe { collection.Item(i) }.context("IMMDeviceCollection::Item")?;
            let id = id_of(&device)?;
            let name = name_of(&device).unwrap_or_else(|_| id.clone());
            out.push(AudioDevice { is_default: default_id.as_deref() == Some(id.as_str()), id, name });
        }
        Ok(out)
    }

    pub(crate) fn id_of(device: &IMMDevice) -> Result<String> {
        // SAFETY: the returned string is CoTaskMemAlloc'd; it is copied out and freed right away.
        unsafe {
            let id = device.GetId().context("IMMDevice::GetId")?;
            let text = id.to_string();
            CoTaskMemFree(Some(id.0 as *const core::ffi::c_void));
            text.context("IMMDevice::GetId returned a string that is not UTF-16")
        }
    }

    fn name_of(device: &IMMDevice) -> Result<String> {
        // SAFETY: the device is alive; the property store and value are released on drop.
        unsafe {
            let store = device.OpenPropertyStore(STGM_READ).context("IMMDevice::OpenPropertyStore")?;
            let value = store.GetValue(&PKEY_Device_FriendlyName).context("IPropertyStore::GetValue")?;
            Ok(value.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_windows_there_is_nothing_to_choose_between() {
        if cfg!(not(windows)) {
            assert_eq!(list_audio_devices().unwrap(), AudioDevices::default());
        }
    }
}
