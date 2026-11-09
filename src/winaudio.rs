//! Windows' own default speaker / microphone ("Windows Default" in Discord and games).
//!
//! Reading uses the documented MMDevice API. Changing the default uses IPolicyConfig,
//! the interface the Windows Sound control panel itself uses. It only changes
//! per-user audio defaults and needs no admin rights.

use anyhow::{anyhow, Context, Result};
use std::ffi::c_void;
use windows::core::{IUnknown, Interface, GUID, HRESULT, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};

const CLSID_POLICY_CONFIG: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
/// IPolicyConfig vtable: IUnknown(3) + GetMixFormat, GetDeviceFormat, ResetDeviceFormat,
/// SetDeviceFormat, Get/SetProcessingPeriod, Get/SetShareMode, Get/SetPropertyValue,
/// then SetDefaultEndpoint.
const SET_DEFAULT_ENDPOINT_SLOT: usize = 13;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
}

/// Snapshot of Windows' audio defaults.
#[derive(Clone, Debug, Default)]
pub struct Defaults {
    /// Default speaker for apps and games.
    pub speaker: Option<Endpoint>,
    /// Default speaker for calls (Discord, Teams can follow this one).
    pub speaker_comms: Option<Endpoint>,
    pub mic: Option<Endpoint>,
    pub mic_comms: Option<Endpoint>,
    pub speakers: Vec<Endpoint>,
    pub mics: Vec<Endpoint>,
}

/// Run COM work on a short-lived MTA thread so it never clashes with the UI thread's apartment.
fn on_com_thread<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    std::thread::spawn(move || unsafe {
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        let r = f();
        if init.is_ok() {
            CoUninitialize();
        }
        r
    })
    .join()
    .map_err(|_| anyhow!("Windows audio settings thread crashed"))?
}

unsafe fn endpoint(dev: &IMMDevice) -> Result<Endpoint> {
    let pid = dev.GetId()?;
    let id = pid.to_string().unwrap_or_default();
    CoTaskMemFree(Some(pid.0 as *const c_void));
    let store = dev.OpenPropertyStore(STGM_READ)?;
    let name = store.GetValue(&PKEY_Device_FriendlyName)?.to_string();
    Ok(Endpoint { id, name })
}

unsafe fn list(en: &IMMDeviceEnumerator, flow: EDataFlow) -> Result<Vec<Endpoint>> {
    let coll = en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
    let mut v = Vec::new();
    for i in 0..coll.GetCount()? {
        if let Ok(e) = endpoint(&coll.Item(i)?) {
            v.push(e);
        }
    }
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(v)
}

unsafe fn default_of(en: &IMMDeviceEnumerator, flow: EDataFlow, role: ERole) -> Option<Endpoint> {
    en.GetDefaultAudioEndpoint(flow, role).ok().and_then(|d| endpoint(&d).ok())
}

pub fn read() -> Result<Defaults> {
    on_com_thread(|| unsafe {
        let en: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("opening Windows audio devices")?;
        Ok(Defaults {
            speaker: default_of(&en, eRender, eConsole),
            speaker_comms: default_of(&en, eRender, eCommunications),
            mic: default_of(&en, eCapture, eConsole),
            mic_comms: default_of(&en, eCapture, eCommunications),
            speakers: list(&en, eRender)?,
            mics: list(&en, eCapture)?,
        })
    })
}

/// Make `id` Windows' default device for every role (apps, media and calls).
pub fn set_default(id: &str) -> Result<()> {
    set_roles(id, &[eConsole, eMultimedia, eCommunications])
}

pub fn set_roles(id: &str, roles: &[ERole]) -> Result<()> {
    let id = id.to_string();
    let roles = roles.to_vec();
    on_com_thread(move || unsafe {
        let unk: IUnknown = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)
            .context("Windows audio policy service is not available")?;
        let mut p: *mut c_void = std::ptr::null_mut();
        unk.query(&IID_POLICY_CONFIG, &mut p).ok().context("this Windows version does not allow changing defaults")?;
        if p.is_null() {
            return Err(anyhow!("audio policy interface unavailable"));
        }
        // Wrap so the reference is released on every path.
        let policy = IUnknown::from_raw(p);
        let vtbl = *(policy.as_raw() as *const *const *const c_void);
        type SetDefaultEndpoint = unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT;
        let set: SetDefaultEndpoint = std::mem::transmute(*vtbl.add(SET_DEFAULT_ENDPOINT_SLOT));
        let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
        for role in roles {
            set(policy.as_raw(), PCWSTR(wide.as_ptr()), role).ok().context("Windows refused to change the default device")?;
        }
        Ok(())
    })
}

/// The device the recommended setup would make Windows' default speaker:
/// the given preference if valid, else headphones, else any non-cable output.
pub fn pick_speaker(d: &Defaults, preferred: Option<&str>) -> Option<Endpoint> {
    let ok = |e: &&Endpoint| !crate::engine::is_cable_playback(&e.name);
    preferred
        .and_then(|p| d.speakers.iter().find(|e| e.name == p))
        .filter(|e| ok(e))
        .or_else(|| d.speakers.iter().filter(ok).find(|e| e.name.to_lowercase().contains("headphone")))
        .or_else(|| d.speakers.iter().find(ok))
        .cloned()
}

impl Defaults {
    /// True when Windows would send PC sound into the cable (others hear games/voices).
    pub fn speaker_is_cable(&self) -> bool {
        [&self.speaker, &self.speaker_comms]
            .iter()
            .any(|e| e.as_ref().map(|e| crate::engine::is_cable_playback(&e.name)).unwrap_or(false))
    }

    pub fn find_mic(&self, name: &str) -> Option<&Endpoint> {
        self.mics.iter().find(|e| e.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(name: &str) -> Endpoint {
        Endpoint { id: format!("id-{name}"), name: name.into() }
    }

    #[test]
    fn detects_cable_as_default_speaker() {
        let mut d = Defaults { speaker: Some(ep("Headphones (HyperX)")), ..Default::default() };
        assert!(!d.speaker_is_cable());
        d.speaker_comms = Some(ep("CABLE Input (VB-Audio Virtual Cable)"));
        assert!(d.speaker_is_cable());
    }

    #[test]
    fn recommended_speaker_prefers_choice_then_headphones_never_cable() {
        let d = Defaults {
            speakers: vec![ep("CABLE Input (VB-Audio Virtual Cable)"), ep("Speakers (HD Audio)"), ep("Headphones (HyperX Cloud III)")],
            ..Default::default()
        };
        assert_eq!(pick_speaker(&d, None).unwrap().name, "Headphones (HyperX Cloud III)");
        assert_eq!(pick_speaker(&d, Some("Speakers (HD Audio)")).unwrap().name, "Speakers (HD Audio)");
        assert_eq!(pick_speaker(&d, Some("CABLE Input (VB-Audio Virtual Cable)")).unwrap().name, "Headphones (HyperX Cloud III)");
    }

    /// Reads the real defaults (read-only, changes nothing).
    #[test]
    fn reads_real_windows_defaults() {
        let d = read().expect("read defaults");
        assert!(!d.speakers.is_empty(), "no playback devices found");
    }
}
