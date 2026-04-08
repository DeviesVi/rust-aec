// WASAPI device enumeration and selection.

use anyhow::{bail, Context, Result};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eRender, IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator,
    MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM,
};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

/// Initialises COM (must be called once per thread before any WASAPI work).
pub fn com_init() -> Result<ComGuard> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok().context("CoInitializeEx")
    }
    .map(|_| ComGuard)
}

pub struct ComGuard;

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize(); }
    }
}

pub struct HandleGuard(HANDLE);

impl HandleGuard {
    pub fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    pub fn get(&self) -> HANDLE {
        self.0
    }
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

pub struct CoTaskMemGuard<T>(*mut T);

impl<T> CoTaskMemGuard<T> {
    pub fn new(ptr: *mut T) -> Self {
        Self(ptr)
    }

    pub fn get(&self) -> *mut T {
        self.0
    }
}

impl<T> Drop for CoTaskMemGuard<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CoTaskMemFree(Some(self.0 as *const _));
            }
        }
    }
}

fn get_enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe {
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .context("CoCreateInstance MMDeviceEnumerator")
    }
}

/// Information about an audio endpoint.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub index: usize,
    pub name: String,
    pub id: String,
}

fn enumerate_devices(data_flow: windows::Win32::Media::Audio::EDataFlow) -> Result<Vec<DeviceInfo>> {
    let enumerator = get_enumerator()?;
    let collection: IMMDeviceCollection = unsafe {
        enumerator.EnumAudioEndpoints(data_flow, DEVICE_STATE_ACTIVE)?
    };
    let count = unsafe { collection.GetCount()? };
    let mut devices = Vec::with_capacity(count as usize);
    for i in 0..count {
        let device: IMMDevice = unsafe { collection.Item(i)? };
        let id = unsafe {
            let pwstr: PWSTR = device.GetId()?;
            let s = pwstr.to_string()?;
            windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.0 as *const _));
            s
        };
        let name = get_device_name(&device).unwrap_or_else(|_| format!("Device {i}"));
        devices.push(DeviceInfo {
            index: i as usize,
            name,
            id,
        });
    }
    Ok(devices)
}

fn get_device_name(device: &IMMDevice) -> Result<String> {
    unsafe {
        let store: IPropertyStore = device.OpenPropertyStore(STGM(0))?;
        let prop = store.GetValue(&PKEY_Device_FriendlyName)?;
        let name = prop.to_string();
        Ok(name)
    }
}

/// List all active capture devices (microphones).
pub fn list_capture_devices() -> Result<Vec<DeviceInfo>> {
    enumerate_devices(eCapture)
}

/// List all active render devices (speakers / virtual cables).
pub fn list_render_devices() -> Result<Vec<DeviceInfo>> {
    enumerate_devices(eRender)
}

/// Get the default capture (microphone) device ID.
pub fn default_capture_device_id() -> Result<String> {
    let enumerator = get_enumerator()?;
    unsafe {
        let device = enumerator
            .GetDefaultAudioEndpoint(eCapture, windows::Win32::Media::Audio::eConsole)
            .context("GetDefaultAudioEndpoint(capture)")?;
        let pwstr: PWSTR = device.GetId()?;
        let s = pwstr.to_string()?;
        windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.0 as *const _));
        Ok(s)
    }
}

/// Get the default render (speaker) device ID.
pub fn default_render_device_id() -> Result<String> {
    let enumerator = get_enumerator()?;
    unsafe {
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, windows::Win32::Media::Audio::eConsole)
            .context("GetDefaultAudioEndpoint(render)")?;
        let pwstr: PWSTR = device.GetId()?;
        let s = pwstr.to_string()?;
        windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.0 as *const _));
        Ok(s)
    }
}

/// Open a device by its endpoint ID string (safe to call from any COM-initialised thread).
pub fn open_device_by_id(id: &str) -> Result<IMMDevice> {
    let enumerator = get_enumerator()?;
    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        enumerator
            .GetDevice(windows::core::PCWSTR(wide.as_ptr()))
            .context("GetDevice by ID")
    }
}

/// Returns true if the device name looks like a virtual audio cable.
pub fn is_virtual_cable(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("cable")
}

/// Find a device by substring of its friendly name (case-insensitive). Returns its ID.
pub fn find_device_id_by_name(devices: &[DeviceInfo], query: &str) -> Result<String> {
    let query_lower = query.to_lowercase();
    for info in devices {
        if info.name.to_lowercase().contains(&query_lower) {
            return Ok(info.id.clone());
        }
    }
    bail!("No device matching '{query}' found")
}

/// Find the first real (non-virtual-cable) capture device. Returns its ID.
pub fn find_real_capture_device(devices: &[DeviceInfo]) -> Result<String> {
    for info in devices {
        if !is_virtual_cable(&info.name) {
            return Ok(info.id.clone());
        }
    }
    bail!("No real microphone found (all capture devices appear to be virtual cables)")
}

/// Get the device name for a given device ID, or "Unknown" if not found.
pub fn device_name_by_id(devices: &[DeviceInfo], id: &str) -> String {
    devices
        .iter()
        .find(|d| d.id == id)
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "Unknown".to_string())
}
