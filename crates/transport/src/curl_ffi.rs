//! Thin safe wrapper around curl-sys for APIs not exposed by the `curl` crate.
//! Specifically: CURLINFO_CONN_ID, CURLINFO_QUEUE_TIME_T, and version checking.

use std::ffi::CStr;

/// Returns the runtime libcurl version string.
pub fn curl_version_string() -> String {
    unsafe {
        let ptr = curl_sys::curl_version();
        if ptr.is_null() {
            return "unknown".to_string();
        }
        CStr::from_ptr(ptr).to_string_lossy().to_string()
    }
}

/// Returns the runtime libcurl version info.
pub fn curl_version_info() -> (u32, u32, u32) {
    unsafe {
        let info = curl_sys::curl_version_info(curl_sys::CURLVERSION_NOW);
        if info.is_null() {
            return (0, 0, 0);
        }
        let major = ((*info).version_num >> 16) & 0xFF;
        let minor = ((*info).version_num >> 8) & 0xFF;
        let patch = (*info).version_num & 0xFF;
        (major, minor, patch)
    }
}

/// Check that the runtime libcurl version meets our minimum requirement (8.2.0 for CONN_ID).
pub fn check_minimum_version() -> Result<(), String> {
    let (major, minor, _patch) = curl_version_info();
    if major > 8 || (major == 8 && minor >= 2) {
        Ok(())
    } else {
        Err(format!(
            "libcurl {major}.{minor}.x is too old. Minimum required: 8.2.0 (for CURLINFO_CONN_ID). \
             Found: {}",
            curl_version_string()
        ))
    }
}

/// Extract CURLINFO_CONN_ID from a raw curl easy handle.
/// Returns the connection ID or -1 if unavailable.
///
/// # Safety
/// The handle must be a valid `*mut curl_sys::CURL` from an active or completed transfer.
pub unsafe fn get_conn_id(handle: *mut curl_sys::CURL) -> i64 {
    let mut conn_id: i64 = -1;
    // CURLINFO_CONN_ID = CURLINFO_OFF_T + 64 = 0x600000 + 64
    let info_code = 0x600000 + 64;
    let rc = curl_sys::curl_easy_getinfo(handle, info_code, &mut conn_id as *mut i64);
    if rc == curl_sys::CURLE_OK {
        conn_id
    } else {
        -1
    }
}

/// Extract CURLINFO_QUEUE_TIME_T from a raw curl easy handle.
/// Returns the queue time in microseconds or 0 if unavailable.
///
/// # Safety
/// The handle must be a valid `*mut curl_sys::CURL` from a completed transfer.
pub unsafe fn get_queue_time_us(handle: *mut curl_sys::CURL) -> i64 {
    let mut queue_time: i64 = 0;
    // CURLINFO_QUEUE_TIME_T = CURLINFO_OFF_T + 65 = 0x600000 + 65
    let info_code = 0x600000 + 65;
    let rc = curl_sys::curl_easy_getinfo(handle, info_code, &mut queue_time as *mut i64);
    if rc == curl_sys::CURLE_OK {
        queue_time
    } else {
        0
    }
}

/// Extract CURLINFO_LOCAL_IP from a raw curl easy handle.
///
/// # Safety
/// The handle must be a valid `*mut curl_sys::CURL` from a completed transfer.
pub unsafe fn get_local_ip(handle: *mut curl_sys::CURL) -> String {
    let mut ptr: *mut libc::c_char = std::ptr::null_mut();
    let rc = curl_sys::curl_easy_getinfo(
        handle,
        curl_sys::CURLINFO_LOCAL_IP,
        &mut ptr as *mut *mut libc::c_char,
    );
    if rc == curl_sys::CURLE_OK && !ptr.is_null() {
        CStr::from_ptr(ptr).to_string_lossy().to_string()
    } else {
        String::new()
    }
}

/// Extract CURLINFO_LOCAL_PORT from a raw curl easy handle.
///
/// # Safety
/// The handle must be a valid `*mut curl_sys::CURL` from a completed transfer.
pub unsafe fn get_local_port(handle: *mut curl_sys::CURL) -> u16 {
    let mut port: libc::c_long = 0;
    let rc = curl_sys::curl_easy_getinfo(
        handle,
        curl_sys::CURLINFO_LOCAL_PORT,
        &mut port as *mut libc::c_long,
    );
    if rc == curl_sys::CURLE_OK {
        port as u16
    } else {
        0
    }
}

/// Extract CURLINFO_PRIMARY_IP from a raw curl easy handle.
///
/// # Safety
/// The handle must be a valid `*mut curl_sys::CURL` from a completed transfer.
pub unsafe fn get_primary_ip(handle: *mut curl_sys::CURL) -> String {
    let mut ptr: *mut libc::c_char = std::ptr::null_mut();
    let rc = curl_sys::curl_easy_getinfo(
        handle,
        curl_sys::CURLINFO_PRIMARY_IP,
        &mut ptr as *mut *mut libc::c_char,
    );
    if rc == curl_sys::CURLE_OK && !ptr.is_null() {
        CStr::from_ptr(ptr).to_string_lossy().to_string()
    } else {
        String::new()
    }
}

/// Extract CURLINFO_PRIMARY_PORT from a raw curl easy handle.
///
/// # Safety
/// The handle must be a valid `*mut curl_sys::CURL` from a completed transfer.
pub unsafe fn get_primary_port(handle: *mut curl_sys::CURL) -> u16 {
    let mut port: libc::c_long = 0;
    let rc = curl_sys::curl_easy_getinfo(
        handle,
        curl_sys::CURLINFO_PRIMARY_PORT,
        &mut port as *mut libc::c_long,
    );
    if rc == curl_sys::CURLE_OK {
        port as u16
    } else {
        0
    }
}

#[allow(non_camel_case_types)]
mod libc {
    pub type c_long = i64;
    pub type c_char = i8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_string_not_empty() {
        let v = curl_version_string();
        assert!(!v.is_empty());
        assert!(v.contains("libcurl"), "Expected libcurl in version: {v}");
    }

    #[test]
    fn version_info_reasonable() {
        let (major, minor, _patch) = curl_version_info();
        assert!(major >= 7, "Expected major >= 7, got {major}");
        assert!(minor <= 99, "Minor version {minor} seems unreasonable");
    }

    #[test]
    fn minimum_version_check_passes() {
        // The vendored curl-sys is 8.19.0, so this should pass
        assert!(check_minimum_version().is_ok());
    }
}
