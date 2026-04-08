// Windows autostart via HKCU\Software\Microsoft\Windows\CurrentVersion\Run.

use anyhow::{Result, bail};
use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::PCWSTR;

const SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "RustAEC";

fn wide_str(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn check(err: WIN32_ERROR, msg: &str) -> Result<()> {
    if err.0 != 0 {
        bail!("{}: error code {}", msg, err.0);
    }
    Ok(())
}

pub fn is_autostart_enabled() -> bool {
    unsafe {
        let subkey = wide_str(SUBKEY);
        let value_name = wide_str(VALUE_NAME);
        let mut hkey = HKEY::default();
        let res = RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        );
        if res.0 != 0 {
            return false;
        }
        let ok = RegQueryValueExW(hkey, PCWSTR(value_name.as_ptr()), None, None, None, None).0 == 0;
        let _ = RegCloseKey(hkey);
        ok
    }
}

pub fn enable_autostart() -> Result<()> {
    let exe_path = std::env::current_exe()?;
    let path_str = exe_path.to_string_lossy().to_string();
    unsafe {
        let subkey = wide_str(SUBKEY);
        let value_name = wide_str(VALUE_NAME);
        let mut hkey = HKEY::default();
        check(
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                0,
                KEY_SET_VALUE,
                &mut hkey,
            ),
            "RegOpenKeyExW",
        )?;
        let path_wide = wide_str(&path_str);
        let path_bytes: &[u8] =
            std::slice::from_raw_parts(path_wide.as_ptr() as *const u8, path_wide.len() * 2);
        check(
            RegSetValueExW(
                hkey,
                PCWSTR(value_name.as_ptr()),
                0,
                REG_SZ,
                Some(path_bytes),
            ),
            "RegSetValueExW",
        )?;
        let _ = RegCloseKey(hkey);
    }
    Ok(())
}

pub fn disable_autostart() -> Result<()> {
    unsafe {
        let subkey = wide_str(SUBKEY);
        let value_name = wide_str(VALUE_NAME);
        let mut hkey = HKEY::default();
        check(
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                0,
                KEY_SET_VALUE,
                &mut hkey,
            ),
            "RegOpenKeyExW",
        )?;
        let _ = RegDeleteValueW(hkey, PCWSTR(value_name.as_ptr()));
        let _ = RegCloseKey(hkey);
    }
    Ok(())
}
