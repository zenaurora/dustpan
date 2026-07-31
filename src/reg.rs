//! Shared Windows registry access (advapi32 FFI, zero dependencies).
//!
//! Read is unrestricted; write is deliberately crippled: the only write
//! entry points are `Key::create_user` / opening with `open_user_rw`,
//! which are hardwired to HKEY_CURRENT_USER. dustpan never writes HKLM
//! and never deletes registry keys — only values it set itself.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

pub type Hkey = *mut c_void;

pub const HKCR: isize = 0x8000_0000u32 as i32 as isize;
pub const HKCU: isize = 0x8000_0001u32 as i32 as isize;
pub const HKLM: isize = 0x8000_0002u32 as i32 as isize;

const KEY_READ: u32 = 0x2_0019;
const KEY_READ_WRITE: u32 = 0x2_001F;
const ERROR_SUCCESS: i32 = 0;
const ERROR_NO_MORE_ITEMS: i32 = 259;
const REG_SZ: u32 = 1;
const REG_EXPAND_SZ: u32 = 2;
const REG_DWORD: u32 = 4;

#[link(name = "advapi32")]
extern "system" {
    fn RegOpenKeyExW(
        key: Hkey,
        sub_key: *const u16,
        options: u32,
        desired: u32,
        result: *mut Hkey,
    ) -> i32;
    fn RegCreateKeyExW(
        key: Hkey,
        sub_key: *const u16,
        reserved: u32,
        class: *const u16,
        options: u32,
        desired: u32,
        security: *mut c_void,
        result: *mut Hkey,
        disposition: *mut u32,
    ) -> i32;
    fn RegEnumKeyExW(
        key: Hkey,
        index: u32,
        name: *mut u16,
        name_len: *mut u32,
        reserved: *mut u32,
        class: *mut u16,
        class_len: *mut u32,
        last_write: *mut c_void,
    ) -> i32;
    fn RegQueryValueExW(
        key: Hkey,
        value_name: *const u16,
        reserved: *mut u32,
        value_type: *mut u32,
        data: *mut u8,
        data_len: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        key: Hkey,
        value_name: *const u16,
        reserved: u32,
        value_type: u32,
        data: *const u8,
        data_len: u32,
    ) -> i32;
    fn RegDeleteValueW(key: Hkey, value_name: *const u16) -> i32;
    fn RegCloseKey(key: Hkey) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

/// RAII registry key handle.
pub struct Key(Hkey);

impl Key {
    pub fn open(root: isize, path: &str) -> Option<Key> {
        let mut out: Hkey = std::ptr::null_mut();
        let rc = unsafe { RegOpenKeyExW(root as Hkey, wide(path).as_ptr(), 0, KEY_READ, &mut out) };
        (rc == ERROR_SUCCESS).then(|| Key(out))
    }

    /// Open an existing HKCU key for read+write. The only non-creating
    /// write path, and it cannot reach HKLM/HKCR by construction.
    pub fn open_user_rw(path: &str) -> Option<Key> {
        let mut out: Hkey = std::ptr::null_mut();
        let rc = unsafe {
            RegOpenKeyExW(
                HKCU as Hkey,
                wide(path).as_ptr(),
                0,
                KEY_READ_WRITE,
                &mut out,
            )
        };
        (rc == ERROR_SUCCESS).then(|| Key(out))
    }

    /// Create (or open) an HKCU key for read+write.
    pub fn create_user(path: &str) -> Option<Key> {
        let mut out: Hkey = std::ptr::null_mut();
        let rc = unsafe {
            RegCreateKeyExW(
                HKCU as Hkey,
                wide(path).as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_READ_WRITE,
                std::ptr::null_mut(),
                &mut out,
                std::ptr::null_mut(),
            )
        };
        (rc == ERROR_SUCCESS).then(|| Key(out))
    }

    pub fn subkeys(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut index = 0u32;
        loop {
            let mut buf = [0u16; 256];
            let mut len = buf.len() as u32;
            let rc = unsafe {
                RegEnumKeyExW(
                    self.0,
                    index,
                    buf.as_mut_ptr(),
                    &mut len,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc == ERROR_SUCCESS {
                names.push(String::from_utf16_lossy(&buf[..len as usize]));
            }
            index += 1;
        }
        names
    }

    /// Read a REG_SZ/REG_EXPAND_SZ value; empty name reads the default value.
    pub fn string_value(&self, name: &str) -> String {
        let mut ty = 0u32;
        let mut buf = vec![0u8; 8192];
        let mut len = buf.len() as u32;
        let rc = unsafe {
            RegQueryValueExW(
                self.0,
                wide(name).as_ptr(),
                std::ptr::null_mut(),
                &mut ty,
                buf.as_mut_ptr(),
                &mut len,
            )
        };
        if rc != ERROR_SUCCESS || (ty != REG_SZ && ty != REG_EXPAND_SZ) {
            return String::new();
        }
        let units: Vec<u16> = buf[..len as usize]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
            .trim_end_matches('\0')
            .to_string()
    }

    pub fn dword_value(&self, name: &str) -> Option<u32> {
        let mut ty = 0u32;
        let mut buf = [0u8; 4];
        let mut len = buf.len() as u32;
        let rc = unsafe {
            RegQueryValueExW(
                self.0,
                wide(name).as_ptr(),
                std::ptr::null_mut(),
                &mut ty,
                buf.as_mut_ptr(),
                &mut len,
            )
        };
        (rc == ERROR_SUCCESS && ty == REG_DWORD).then(|| u32::from_le_bytes(buf))
    }

    /// Does a value with this name exist (any type)?
    pub fn has_value(&self, name: &str) -> bool {
        let mut len = 0u32;
        let rc = unsafe {
            RegQueryValueExW(
                self.0,
                wide(name).as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        rc == ERROR_SUCCESS
    }

    /// Write a REG_SZ value (key must have been opened via a user_rw path).
    pub fn set_string(&self, name: &str, data: &str) -> bool {
        let wdata = wide(data);
        let bytes = wdata.len() * 2;
        let rc = unsafe {
            RegSetValueExW(
                self.0,
                wide(name).as_ptr(),
                0,
                REG_SZ,
                wdata.as_ptr() as *const u8,
                bytes as u32,
            )
        };
        rc == ERROR_SUCCESS
    }

    pub fn delete_value(&self, name: &str) -> bool {
        unsafe { RegDeleteValueW(self.0, wide(name).as_ptr()) == ERROR_SUCCESS }
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}
