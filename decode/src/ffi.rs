//! C ABI over the decode core, for the DLT Viewer (C++) plugin and other native
//! embedders. The matching C header is `include/bindfetto_decode.h`.
//!
//! Ownership contract:
//! * [`bf_decoder_new`] returns an owned handle; free it exactly once with
//!   [`bf_decoder_free`].
//! * [`bf_decode_line`] returns an owned C string; free it exactly once with
//!   [`bf_string_free`].
//! * Every `const char *` input must be a NUL-terminated UTF-8 string; it is only
//!   borrowed for the duration of the call.
//!
//! All functions are null-tolerant and never unwind across the boundary (the core is
//! panic-free on valid input).

use std::ffi::{c_char, CStr, CString};

use crate::Decoder;

/// Build a decoder from NUL-terminated UTF-8 catalog JSON. Returns NULL if
/// `catalog_json` is NULL, is not UTF-8, or is not valid catalog JSON.
#[no_mangle]
pub extern "C" fn bf_decoder_new(catalog_json: *const c_char) -> *mut Decoder {
    if catalog_json.is_null() {
        return std::ptr::null_mut();
    }
    let Ok(json) = (unsafe { CStr::from_ptr(catalog_json) }).to_str() else {
        return std::ptr::null_mut();
    };
    match Decoder::from_catalog_json(json) {
        Ok(decoder) => Box::into_raw(Box::new(decoder)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Free a decoder created by [`bf_decoder_new`]. NULL is ignored.
///
/// # Safety
/// `decoder` must be a pointer returned by [`bf_decoder_new`] and not already freed.
#[no_mangle]
pub unsafe extern "C" fn bf_decoder_free(decoder: *mut Decoder) {
    if !decoder.is_null() {
        drop(Box::from_raw(decoder));
    }
}

/// Decode one line. Returns a newly-allocated NUL-terminated UTF-8 string the caller
/// must free with [`bf_string_free`]. Returns NULL if either argument is NULL or
/// `line` is not UTF-8.
///
/// # Safety
/// `decoder` must be a live handle from [`bf_decoder_new`]; `line` a valid
/// NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn bf_decode_line(
    decoder: *const Decoder,
    line: *const c_char,
) -> *mut c_char {
    if decoder.is_null() || line.is_null() {
        return std::ptr::null_mut();
    }
    let decoder = &*decoder;
    let Ok(line) = CStr::from_ptr(line).to_str() else {
        return std::ptr::null_mut();
    };
    // `decode_line` yields no interior NULs for log text, so `CString::new` succeeds.
    match CString::new(decoder.decode_line(line).as_ref()) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Free a string returned by [`bf_decode_line`]. NULL is ignored.
///
/// # Safety
/// `s` must be a pointer returned by [`bf_decode_line`] and not already freed.
#[no_mangle]
pub unsafe extern "C" fn bf_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}
