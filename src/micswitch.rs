//! Switch the default microphone: to "CABLE Output" on connect, back on disconnect.
//!
//! Direct COM calls through the windows crate: IMMDeviceEnumerator (documented) to find the
//! endpoints and IPolicyConfig (undocumented, stable since Vista, the same interface the Windows
//! sound settings use) to set the default endpoint. No PowerShell process, no C# compilation,
//! switching takes milliseconds. If Microsoft ever changes IPolicyConfig, only switching fails;
//! the audio path keeps working.
//!
//! Activating and restoring set all three roles (console, multimedia, communications) in ONE
//! pass, because every change of the default endpoint makes the audio engine stall briefly.

// COM method names follow the Windows vtable.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use windows::core::{interface, IUnknown, IUnknown_Vtbl, HRESULT, PCWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, ERole, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};

#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfig: IUnknown {
    // Only the vtable position matters; the first ten methods are never called.
    fn GetMixFormat(&self, a: *const c_void, b: *mut c_void) -> HRESULT;
    fn GetDeviceFormat(&self, a: *const c_void, b: i32, c: *mut c_void) -> HRESULT;
    fn ResetDeviceFormat(&self, a: *const c_void) -> HRESULT;
    fn SetDeviceFormat(&self, a: *const c_void, b: *const c_void, c: *const c_void) -> HRESULT;
    fn GetProcessingPeriod(
        &self,
        a: *const c_void,
        b: i32,
        c: *mut c_void,
        d: *mut c_void,
    ) -> HRESULT;
    fn SetProcessingPeriod(&self, a: *const c_void, b: *const c_void) -> HRESULT;
    fn GetShareMode(&self, a: *const c_void, b: *mut c_void) -> HRESULT;
    fn SetShareMode(&self, a: *const c_void, b: *const c_void) -> HRESULT;
    fn GetPropertyValue(
        &self,
        a: *const c_void,
        b: i32,
        c: *const c_void,
        d: *mut c_void,
    ) -> HRESULT;
    fn SetPropertyValue(
        &self,
        a: *const c_void,
        b: i32,
        c: *const c_void,
        d: *const c_void,
    ) -> HRESULT;
    fn SetDefaultEndpoint(&self, device_id: PCWSTR, role: ERole) -> HRESULT;
    fn SetEndpointVisibility(&self, a: *const c_void, b: i32) -> HRESULT;
}

const CLSID_POLICY_CONFIG: windows::core::GUID =
    windows::core::GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// PKEY_Device_FriendlyName (from FunctionDiscovery, defined here directly).
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

const ROLES: [(i32, ERole); 3] = [(0, eConsole), (1, eMultimedia), (2, eCommunications)];

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
}

/// Initialise COM per thread; uninitialise on drop if we initialised it.
struct ComGuard(bool);

impl ComGuard {
    fn new() -> Self {
        // S_OK or S_FALSE: we (co-)initialised. RPC_E_CHANGED_MODE: the thread already has a
        // different apartment, then release nothing.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        ComGuard(hr.is_ok())
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn device_id(dev: &IMMDevice) -> Option<String> {
    unsafe {
        let p = dev.GetId().ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

fn device_name(dev: &IMMDevice) -> Option<String> {
    unsafe {
        let store = dev.OpenPropertyStore(STGM_READ).ok()?;
        let mut v = store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME).ok()?;
        let name = PropVariantToStringAlloc(&v).ok().and_then(|p| {
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const c_void));
            s
        });
        let _ = PropVariantClear(&mut v);
        name
    }
}

fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// Current default recording endpoint for a role.
fn default_capture(en: &IMMDeviceEnumerator, role: ERole) -> Option<Endpoint> {
    let dev = unsafe { en.GetDefaultAudioEndpoint(eCapture, role) }.ok()?;
    Some(Endpoint {
        id: device_id(&dev)?,
        name: device_name(&dev).unwrap_or_default(),
    })
}

/// First active recording endpoint whose display name contains `contains` (case-insensitive).
fn find_capture(en: &IMMDeviceEnumerator, contains: &str) -> Option<Endpoint> {
    let needle = contains.to_lowercase();
    unsafe {
        let col = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE).ok()?;
        let n = col.GetCount().ok()?;
        for i in 0..n {
            let Ok(dev) = col.Item(i) else { continue };
            let Some(name) = device_name(&dev) else {
                continue;
            };
            if name.to_lowercase().contains(&needle) {
                if let Some(id) = device_id(&dev) {
                    return Some(Endpoint { id, name });
                }
            }
        }
    }
    None
}

fn set_default(id: &str, role: ERole) -> windows::core::Result<()> {
    let wide = to_wide(id);
    unsafe {
        let pc: IPolicyConfig = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL)?;
        pc.SetDefaultEndpoint(PCWSTR(wide.as_ptr()), role).ok()
    }
}

pub struct MicSwitch {
    target_filter: String,
    /// The target endpoint while switched (so restore can check it is still the default).
    target: Option<Endpoint>,
    previous: Option<Vec<(i32, Endpoint)>>,
    /// File holding the target and the remembered previous microphones. If the process dies
    /// (crash, Stop-Process, logoff), the next start restores from it.
    state_file: PathBuf,
}

/// State file: one line `T<TAB>id<TAB>name` for the target, then `role<TAB>id<TAB>name` per role.
fn save_state(path: &Path, target: &Endpoint, prev: &[(i32, Endpoint)]) -> std::io::Result<()> {
    let mut lines = vec![format!("T\t{}\t{}", target.id, target.name)];
    lines.extend(
        prev.iter()
            .map(|(r, e)| format!("{r}\t{}\t{}", e.id, e.name)),
    );
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(path, lines.join("\n"))
}

type State = (Option<Endpoint>, Vec<(i32, Endpoint)>);

fn parse_state(body: &str) -> State {
    let mut target = None;
    let mut prev = Vec::new();
    for line in body.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(role), Some(id), Some(name)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let ep = Endpoint {
            id: id.to_string(),
            name: name.to_string(),
        };
        if role == "T" {
            target = Some(ep);
        } else if let Ok(role) = role.trim().parse::<i32>() {
            prev.push((role, ep));
        }
    }
    (target, prev)
}

fn load_state(path: &Path) -> Option<State> {
    std::fs::read_to_string(path).ok().map(|b| parse_state(&b))
}

/// First active recording endpoint other than `exclude_id` (fallback when the remembered
/// microphone is gone, e.g. an unplugged USB headset).
fn other_capture(en: &IMMDeviceEnumerator, exclude_id: &str) -> Option<Endpoint> {
    unsafe {
        let col = en.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE).ok()?;
        let n = col.GetCount().ok()?;
        for i in 0..n {
            let Ok(dev) = col.Item(i) else { continue };
            let Some(id) = device_id(&dev) else { continue };
            if id != exclude_id {
                return Some(Endpoint {
                    name: device_name(&dev).unwrap_or_default(),
                    id,
                });
            }
        }
    }
    None
}

fn role_of(idx: i32) -> ERole {
    ROLES
        .iter()
        .find(|(i, _)| *i == idx)
        .map(|(_, r)| *r)
        .unwrap_or(eConsole)
}

impl MicSwitch {
    pub fn new(target_filter: &str, state_file: PathBuf) -> std::io::Result<Self> {
        // Check early whether COM and the enumerator are available.
        let _com = ComGuard::new();
        let en = enumerator().map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut me = Self {
            target_filter: target_filter.to_string(),
            target: None,
            previous: None,
            state_file,
        };
        me.recover_previous(&en);
        Ok(me)
    }

    /// After a crash or hard kill: if the remembered target is still the default, restore the
    /// previous microphone. Otherwise (the user picked another one meanwhile) just remove the
    /// file.
    fn recover_previous(&mut self, en: &IMMDeviceEnumerator) {
        let Some((target, prev)) = load_state(&self.state_file) else {
            return;
        };
        let target = target.or_else(|| find_capture(en, &self.target_filter));
        let target_is_default = target
            .as_ref()
            .zip(default_capture(en, eConsole))
            .is_some_and(|(t, d)| d.id == t.id);
        if target_is_default && !prev.is_empty() {
            tracing::warn!("previous run ended without restoring, restoring default microphone");
            self.target = target;
            self.previous = Some(prev);
            self.restore();
        } else {
            let _ = std::fs::remove_file(&self.state_file);
        }
    }

    /// Display name of the current default microphone (console role), for display and logs.
    pub fn current_default_name(&self) -> Option<String> {
        let _com = ComGuard::new();
        let en = enumerator().ok()?;
        default_capture(&en, eConsole).map(|e| e.name)
    }

    /// Remembers the current default microphones (per role) and makes the target the default.
    /// Idempotent: if the target already is the default, nothing is remembered or changed.
    pub fn activate(&mut self) {
        if self.previous.is_some() {
            return;
        }
        let _com = ComGuard::new();
        let en = match enumerator() {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("MMDeviceEnumerator unavailable: {e}");
                return;
            }
        };
        let Some(target) = find_capture(&en, &self.target_filter) else {
            tracing::warn!(filter = %self.target_filter, "target microphone not found, not switching");
            return;
        };
        let mut prev = Vec::new();
        for (idx, role) in ROLES {
            if let Some(cur) = default_capture(&en, role) {
                if cur.id != target.id {
                    prev.push((idx, cur));
                }
            }
        }
        if prev.is_empty() {
            tracing::info!(target = %target.name, "target microphone already is the default");
            self.previous = Some(prev);
            self.target = Some(target);
            return;
        }
        // Remember BEFORE switching: a kill right after the switch must still be recoverable.
        if let Err(e) = save_state(&self.state_file, &target, &prev) {
            tracing::warn!("cannot save previous microphone, not switching: {e}");
            return;
        }
        // Remember even on partial failure, so restore() cleans up all roles.
        for (_, role) in ROLES {
            if let Err(e) = set_default(&target.id, role) {
                tracing::warn!(?role, "SetDefaultEndpoint failed: {e}");
            }
        }
        tracing::info!(
            target = %target.name,
            previous = %prev.iter().map(|(r, e)| format!("{r}:{}", e.name)).collect::<Vec<_>>().join(", "),
            "default microphone switched"
        );
        self.previous = Some(prev);
        self.target = Some(target);
    }

    /// Restores the remembered default microphones, but only while the target is still the
    /// default: if the user picked another microphone meanwhile, that choice stays.
    pub fn restore(&mut self) {
        let Some(prev) = self.previous.take() else {
            return;
        };
        let target = self.target.take();
        if prev.is_empty() {
            return;
        }
        let _com = ComGuard::new();
        let en = enumerator().ok();
        let current = en.as_ref().and_then(|e| default_capture(e, eConsole));
        let still_default = match (&target, &current) {
            (Some(t), Some(d)) => d.id == t.id,
            // Unknown: better restore than leave CABLE Output as the default.
            _ => true,
        };
        if !still_default {
            tracing::info!("default microphone was changed by the user, not restoring");
            let _ = std::fs::remove_file(&self.state_file);
            return;
        }
        let mut all_ok = true;
        for (idx, ep) in &prev {
            let role = role_of(*idx);
            if let Err(e) = set_default(&ep.id, role) {
                tracing::warn!(?role, name = %ep.name, "restoring failed: {e}");
                // The remembered device may be gone (unplugged): fall back to any other one.
                let exclude = target.as_ref().map(|t| t.id.as_str()).unwrap_or("");
                let fallback = en.as_ref().and_then(|e| other_capture(e, exclude));
                match fallback.map(|f| (set_default(&f.id, role), f)) {
                    Some((Ok(()), f)) => {
                        tracing::info!(?role, name = %f.name, "fell back to another microphone")
                    }
                    _ => all_ok = false,
                }
            }
        }
        tracing::info!(
            restored = %prev.iter().map(|(r, e)| format!("{r}:{}", e.name)).collect::<Vec<_>>().join(", "),
            all_ok,
            "default microphone restored"
        );
        if all_ok {
            let _ = std::fs::remove_file(&self.state_file);
        } else {
            // Keep the file: the next start tries again.
            tracing::warn!("restore incomplete, will retry at the next start");
        }
    }

    pub fn is_active(&self) -> bool {
        self.previous.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_file_roundtrip_with_target() {
        let target = Endpoint {
            id: "{0.0.1}.{cable}".into(),
            name: "CABLE Output (VB-Audio Virtual Cable)".into(),
        };
        let prev = vec![
            (
                0,
                Endpoint {
                    id: "{0.0.1}.{usb}".into(),
                    name: "Mic\twith tab".into(),
                },
            ),
            (
                2,
                Endpoint {
                    id: "{0.0.1}.{bt}".into(),
                    name: "Headset".into(),
                },
            ),
        ];
        let d = std::env::temp_dir().join(format!("couchmic-state-{}", std::process::id()));
        let p = d.join("previous-mic.txt");
        save_state(&p, &target, &prev).unwrap();
        let (t, v) = load_state(&p).unwrap();
        assert_eq!(t.unwrap().id, target.id);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].1.name, "Mic\twith tab", "tabs in names survive");
        assert_eq!(v[1].0, 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn old_state_files_without_target_still_load() {
        let (t, v) = parse_state("0\t{a}\tMic\n1\t{a}\tMic");
        assert!(t.is_none());
        assert_eq!(v.len(), 2);
    }
}
