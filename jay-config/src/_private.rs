pub mod client;
pub mod ipc;
mod logging;

use {bincode::Options, std::marker::PhantomData};

pub const VERSION: u32 = 1;

#[repr(C)]
pub struct ConfigEntry {
    pub version: u32,
    pub init: unsafe extern "C" fn(
        srv_data: *const u8,
        srv_unref: unsafe extern "C" fn(data: *const u8),
        srv_handler: unsafe extern "C" fn(data: *const u8, msg: *const u8, size: usize),
        msg: *const u8,
        size: usize,
    ) -> *const u8,
    pub unref: unsafe extern "C" fn(data: *const u8),
    pub handle_msg: unsafe extern "C" fn(data: *const u8, msg: *const u8, size: usize),
}

pub struct ConfigEntryGen<T> {
    _phantom: PhantomData<T>,
}

impl<T: Config> ConfigEntryGen<T> {}

pub fn bincode_ops() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .with_no_limit()
}

pub trait Config {
    extern "C" fn configure();
}
