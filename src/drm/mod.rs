use crate::format::Format;

pub mod dma;
pub mod drm;
pub mod gbm;

pub type Modifier = u64;

pub const INVALID_MODIFIER: Modifier = 0x00ff_ffff_ffff_ffff;

pub struct ModifiedFormat {
    pub format: &'static Format,
    pub modifier: Modifier,
}
