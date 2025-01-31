#![allow(non_camel_case_types, improper_ctypes)]

mod consts;

include!(concat!(env!("OUT_DIR"), "/xkbcommon_tys.rs"));

pub use consts::*;
use {
    bstr::{BStr, ByteSlice},
    isnt::std_1::primitive::IsntConstPtrExt,
    std::{ffi::CStr, io::Write, ops::Deref, ptr, rc::Rc},
};

use {
    crate::utils::{ptr_ext::PtrExt, trim::AsciiTrim},
    thiserror::Error,
    uapi::{c, OwnedFd},
};

#[derive(Debug, Error)]
pub enum XkbCommonError {
    #[error("Could not create an xkbcommon context")]
    CreateContext,
    #[error("Could not create an xkbcommon state")]
    CreateState,
    #[error("Could not create keymap from buffer")]
    KeymapFromBuffer,
    #[error("Could not convert the keymap to a string")]
    AsStr,
}

struct xkb_context;
struct xkb_keymap;
struct xkb_state;

type xkb_keycode_t = u32;
type xkb_layout_index_t = u32;
type xkb_level_index_t = u32;
type xkb_keysym_t = u32;

#[repr(C)]
struct xkb_rule_names {
    rules: *const c::c_char,
    model: *const c::c_char,
    layout: *const c::c_char,
    variant: *const c::c_char,
    options: *const c::c_char,
}

impl Default for xkb_rule_names {
    fn default() -> Self {
        Self {
            rules: ptr::null(),
            model: ptr::null(),
            layout: ptr::null(),
            variant: ptr::null(),
            options: ptr::null(),
        }
    }
}

#[link(name = "xkbcommon")]
extern "C" {
    fn xkb_context_new(flags: xkb_context_flags) -> *mut xkb_context;
    fn xkb_context_unref(context: *mut xkb_context);
    fn xkb_context_set_log_verbosity(context: *mut xkb_context, verbosity: c::c_int);
    fn xkb_context_set_log_fn(context: *mut xkb_context, log_fn: unsafe extern "C" fn());
    fn xkb_keymap_new_from_buffer(
        context: *mut xkb_context,
        buffer: *const u8,
        length: usize,
        format: xkb_keymap_format,
        flags: xkb_keymap_compile_flags,
    ) -> *mut xkb_keymap;
    fn xkb_keymap_get_as_string(
        keymap: *mut xkb_keymap,
        format: xkb_keymap_format,
    ) -> *mut c::c_char;
    fn xkb_keymap_unref(keymap: *mut xkb_keymap);
    // fn xkb_keymap_ref(keymap: *mut xkb_keymap) -> *mut xkb_keymap;
    fn xkb_keymap_key_get_syms_by_level(
        keymap: *mut xkb_keymap,
        key: xkb_keycode_t,
        layout: xkb_layout_index_t,
        level: xkb_level_index_t,
        syms_out: *mut *const xkb_keysym_t,
    ) -> c::c_int;
    fn xkb_state_unref(state: *mut xkb_state);
    fn xkb_state_new(keymap: *mut xkb_keymap) -> *mut xkb_state;
    #[allow(dead_code)]
    fn xkb_state_update_key(
        state: *mut xkb_state,
        key: u32,
        direction: xkb_key_direction,
    ) -> xkb_state_component;
    #[allow(dead_code)]
    fn xkb_state_serialize_mods(state: *mut xkb_state, components: xkb_state_component) -> u32;
    #[allow(dead_code)]
    fn xkb_state_serialize_layout(state: *mut xkb_state, components: xkb_state_component) -> u32;
}

pub struct XkbContext {
    context: *mut xkb_context,
}

extern "C" {
    fn jay_xkbcommon_log_handler_bridge();
}

impl XkbContext {
    pub fn new() -> Result<Self, XkbCommonError> {
        let res = unsafe { xkb_context_new(XKB_CONTEXT_NO_FLAGS.raw() as _) };
        if res.is_null() {
            return Err(XkbCommonError::CreateContext);
        }
        unsafe {
            xkb_context_set_log_verbosity(res, 10);
            xkb_context_set_log_fn(res, jay_xkbcommon_log_handler_bridge);
        }
        Ok(Self { context: res })
    }

    fn raw_to_map(raw: *mut xkb_keymap) -> Result<Rc<XkbKeymap>, XkbCommonError> {
        let res = unsafe { xkb_keymap_get_as_string(raw, XKB_KEYMAP_FORMAT_TEXT_V1.raw() as _) };
        if res.is_null() {
            unsafe {
                xkb_keymap_unref(raw);
            }
            return Err(XkbCommonError::AsStr);
        }
        let str = XkbKeymapStr {
            s: unsafe { CStr::from_ptr(res).to_bytes().as_bstr() },
        };
        let mut memfd =
            uapi::memfd_create("keymap", c::MFD_CLOEXEC | c::MFD_ALLOW_SEALING).unwrap();
        memfd.write_all(str.as_bytes()).unwrap();
        memfd.write_all(&[0]).unwrap();
        uapi::lseek(memfd.raw(), 0, c::SEEK_SET).unwrap();
        uapi::fcntl_add_seals(
            memfd.raw(),
            c::F_SEAL_SEAL | c::F_SEAL_GROW | c::F_SEAL_SHRINK | c::F_SEAL_WRITE,
        )
        .unwrap();
        Ok(Rc::new(XkbKeymap {
            keymap: raw,
            map: Rc::new(memfd),
            map_len: str.len() + 1,
        }))
    }

    pub fn keymap_from_str(&self, s: &str) -> Result<Rc<XkbKeymap>, XkbCommonError> {
        unsafe {
            let keymap = xkb_keymap_new_from_buffer(
                self.context,
                s.as_bytes().as_ptr(),
                s.len(),
                XKB_KEYMAP_FORMAT_TEXT_V1.raw(),
                0,
            );
            if keymap.is_null() {
                return Err(XkbCommonError::KeymapFromBuffer);
            }
            Self::raw_to_map(keymap)
        }
    }
}

impl Drop for XkbContext {
    fn drop(&mut self) {
        unsafe {
            xkb_context_unref(self.context);
        }
    }
}

pub struct XkbKeymap {
    keymap: *mut xkb_keymap,
    pub map: Rc<OwnedFd>,
    pub map_len: usize,
}

impl XkbKeymap {
    pub fn state(self: &Rc<Self>) -> Result<XkbState, XkbCommonError> {
        let res = unsafe { xkb_state_new(self.keymap) };
        if res.is_null() {
            return Err(XkbCommonError::CreateState);
        }
        Ok(XkbState {
            map: self.clone(),
            state: res,
            mods: ModifierState {
                mods_depressed: 0,
                mods_latched: 0,
                mods_locked: 0,
                mods_effective: 0,
                group: 0,
            },
        })
    }
}

impl Drop for XkbKeymap {
    fn drop(&mut self) {
        unsafe {
            xkb_keymap_unref(self.keymap);
        }
    }
}

pub struct XkbKeymapStr {
    s: *const BStr,
}

impl Deref for XkbKeymapStr {
    type Target = BStr;

    fn deref(&self) -> &Self::Target {
        unsafe { self.s.deref() }
    }
}

impl Drop for XkbKeymapStr {
    fn drop(&mut self) {
        unsafe { c::free(self.s as _) }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct ModifierState {
    pub mods_depressed: u32,
    pub mods_latched: u32,
    pub mods_locked: u32,
    pub mods_effective: u32,
    pub group: u32,
}

pub struct XkbState {
    map: Rc<XkbKeymap>,
    state: *mut xkb_state,
    mods: ModifierState,
}

impl XkbState {
    pub fn mods(&self) -> ModifierState {
        self.mods
    }

    #[allow(dead_code)]
    pub fn update(&mut self, key: u32, direction: XkbKeyDirection) -> Option<ModifierState> {
        unsafe {
            let changes = xkb_state_update_key(self.state, key + 8, direction.raw() as _);
            if changes != 0 {
                self.mods.mods_depressed =
                    xkb_state_serialize_mods(self.state, XKB_STATE_MODS_DEPRESSED.raw() as _);
                self.mods.mods_latched =
                    xkb_state_serialize_mods(self.state, XKB_STATE_MODS_LATCHED.raw() as _);
                self.mods.mods_locked =
                    xkb_state_serialize_mods(self.state, XKB_STATE_MODS_LOCKED.raw() as _);
                self.mods.mods_effective =
                    self.mods.mods_depressed | self.mods.mods_latched | self.mods.mods_locked;
                self.mods.group =
                    xkb_state_serialize_layout(self.state, XKB_STATE_LAYOUT_EFFECTIVE.raw() as _);
                Some(self.mods)
            } else {
                None
            }
        }
    }

    pub fn unmodified_keysyms(&self, key: u32) -> &[xkb_keysym_t] {
        let mut res = ptr::null();
        unsafe {
            let num = xkb_keymap_key_get_syms_by_level(
                self.map.keymap,
                key + 8,
                self.mods.group,
                0,
                &mut res,
            );
            if num > 0 {
                std::slice::from_raw_parts(res, num as usize)
            } else {
                &[]
            }
        }
    }
}

impl Drop for XkbState {
    fn drop(&mut self) {
        unsafe {
            xkb_state_unref(self.state);
        }
    }
}

#[no_mangle]
unsafe extern "C" fn jay_xkbcommon_log_handler(
    _ctx: *mut xkb_context,
    level: xkb_log_level,
    line: *const c::c_char,
) {
    assert!(line.is_not_null());
    let buf = CStr::from_ptr(line);
    let level = match XkbLogLevel(level) {
        XKB_LOG_LEVEL_CRITICAL | XKB_LOG_LEVEL_ERROR => log::Level::Error,
        XKB_LOG_LEVEL_WARNING => log::Level::Warn,
        XKB_LOG_LEVEL_INFO => log::Level::Info,
        XKB_LOG_LEVEL_DEBUG => log::Level::Debug,
        _ => log::Level::Error,
    };
    log::log!(level, "xkbcommon: {}", buf.to_bytes().trim_end().as_bstr());
}
