mod allocator;
mod command;
mod descriptor;
mod device;
mod fence;
mod format;
mod image;
mod instance;
mod pipeline;
mod renderer;
mod sampler;
mod semaphore;
mod shaders;
mod staging;
mod util;

use {
    crate::{
        async_engine::AsyncEngine,
        format::Format,
        gfx_api::{
            GfxContext, GfxError, GfxFormat, GfxFramebuffer, GfxImage, GfxTexture, ResetStatus,
        },
        gfx_apis::vulkan::{
            image::VulkanImageMemory, instance::VulkanInstance, renderer::VulkanRenderer,
        },
        io_uring::IoUring,
        utils::oserror::OsError,
        video::{
            dmabuf::DmaBuf,
            drm::{Drm, DrmError},
            gbm::{GbmDevice, GbmError},
        },
    },
    ahash::AHashMap,
    ash::{vk, LoadingError},
    gpu_alloc::{AllocationError, MapError},
    jay_config::video::GfxApi,
    once_cell::sync::Lazy,
    std::{
        cell::Cell,
        ffi::{CStr, CString},
        rc::Rc,
        sync::Arc,
    },
    thiserror::Error,
    uapi::c::dev_t,
};

#[derive(Debug, Error)]
pub enum VulkanError {
    #[error("Could not create a GBM device")]
    Gbm(#[source] GbmError),
    #[error("Could not load libvulkan.so")]
    Load(#[source] Arc<LoadingError>),
    #[error("Could not list instance extensions")]
    InstanceExtensions(#[source] vk::Result),
    #[error("Could not list instance layers")]
    InstanceLayers(#[source] vk::Result),
    #[error("Could not list device extensions")]
    DeviceExtensions(#[source] vk::Result),
    #[error("Could not create the device")]
    CreateDevice(#[source] vk::Result),
    #[error("Could not create a semaphore")]
    CreateSemaphore(#[source] vk::Result),
    #[error("Could not create a fence")]
    CreateFence(#[source] vk::Result),
    #[error("Could not create the buffer")]
    CreateBuffer(#[source] vk::Result),
    #[error("Could not create a shader module")]
    CreateShaderModule(#[source] vk::Result),
    #[error("Missing required instance extension {0:?}")]
    MissingInstanceExtension(&'static CStr),
    #[error("Could not allocate a command pool")]
    AllocateCommandPool(#[source] vk::Result),
    #[error("Could not allocate a command buffer")]
    AllocateCommandBuffer(#[source] vk::Result),
    #[error("Device does not have a graphics queue")]
    NoGraphicsQueue,
    #[error("Missing required device extension {0:?}")]
    MissingDeviceExtension(&'static CStr),
    #[error("Could not create an instance")]
    CreateInstance(#[source] vk::Result),
    #[error("Could not create a debug-utils messenger")]
    Messenger(#[source] vk::Result),
    #[error("Could not fstat the DRM FD")]
    Fstat(#[source] OsError),
    #[error("Could not enumerate the physical devices")]
    EnumeratePhysicalDevices(#[source] vk::Result),
    #[error("Could not find a vulkan device that matches dev_t {0}")]
    NoDeviceFound(dev_t),
    #[error("Could not load image properties")]
    LoadImageProperties(#[source] vk::Result),
    #[error("Device does not support rending and texturing from the XRGB8888 format")]
    XRGB8888,
    #[error("Device does not support syncobj import")]
    SyncobjImport,
    #[error("Could not start a command buffer")]
    BeginCommandBuffer(vk::Result),
    #[error("Could not end a command buffer")]
    EndCommandBuffer(vk::Result),
    #[error("Could not submit a command buffer")]
    Submit(vk::Result),
    #[error("Could not create a sampler")]
    CreateSampler(#[source] vk::Result),
    #[error("Could not create a pipeline layout")]
    CreatePipelineLayout(#[source] vk::Result),
    #[error("Could not create a descriptor set layout")]
    CreateDescriptorSetLayout(#[source] vk::Result),
    #[error("Could not create a pipeline")]
    CreatePipeline(#[source] vk::Result),
    #[error("The format is not supported")]
    FormatNotSupported,
    #[error("The modifier is not supported")]
    ModifierNotSupported,
    #[error("The modifier does not support this use-case")]
    ModifierUseNotSupported,
    #[error("The image has a non-positive size")]
    NonPositiveImageSize,
    #[error("The image is too large")]
    ImageTooLarge,
    #[error("Could not retrieve device properties")]
    GetDeviceProperties(#[source] vk::Result),
    #[error("The dmabuf has an incorrect number of planes")]
    BadPlaneCount,
    #[error("The dmabuf is disjoint but the modifier does not support disjoint buffers")]
    DisjointNotSupported,
    #[error("Could not create the image")]
    CreateImage(#[source] vk::Result),
    #[error("Could not create an image view")]
    CreateImageView(#[source] vk::Result),
    #[error("Could not query the memory fd properties")]
    MemoryFdProperties(#[source] vk::Result),
    #[error("There is no matching memory type")]
    MemoryType,
    #[error("Could not duplicate the DRM fd")]
    Dupfd(#[source] OsError),
    #[error("Could not allocate memory")]
    AllocateMemory(#[source] vk::Result),
    #[error("Could not allocate memory")]
    AllocateMemory2(#[source] AllocationError),
    #[error("Could not bind memory to the image")]
    BindImageMemory(#[source] vk::Result),
    #[error("The format does not support shared memory images")]
    ShmNotSupported,
    #[error("Could not bind memory to the buffer")]
    BindBufferMemory(#[source] vk::Result),
    #[error("Could not map the memory")]
    MapMemory(#[source] MapError),
    #[error("Could not flush modified memory")]
    FlushMemory(#[source] vk::Result),
    #[error("Could not export a sync file from a dma-buf")]
    IoctlExportSyncFile(#[source] OsError),
    #[error("Could not import a sync file into a semaphore")]
    ImportSyncFile(#[source] vk::Result),
    #[error("Could not import a sync file into a dma-buf")]
    IoctlImportSyncFile(#[source] OsError),
    #[error("Could not export a sync file from a semaphore")]
    ExportSyncFile(#[source] vk::Result),
    #[error("Could not fetch the render node of the device")]
    FetchRenderNode(#[source] DrmError),
    #[error("Device has no render node")]
    NoRenderNode,
    #[error("Overflow while calculating shm buffer size")]
    ShmOverflow,
    #[error("Shm stride does not match format or width")]
    InvalidStride,
    #[error("Shm stride and height do not match buffer size")]
    InvalidBufferSize,
    #[error("The shm parameters are invalid x={x}, y={y}, width={width}, height={height}, stride={stride}")]
    InvalidShmParameters {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: i32,
    },
}

impl From<VulkanError> for GfxError {
    fn from(value: VulkanError) -> Self {
        Self(Box::new(value))
    }
}

pub static VULKAN_VALIDATION: Lazy<bool> =
    Lazy::new(|| std::env::var("JAY_VULKAN_VALIDATION").ok().as_deref() == Some("1"));

pub fn create_graphics_context(
    eng: &Rc<AsyncEngine>,
    ring: &Rc<IoUring>,
    drm: &Drm,
) -> Result<Rc<dyn GfxContext>, GfxError> {
    let instance = VulkanInstance::new(eng, ring, *VULKAN_VALIDATION)?;
    let device = instance.create_device(drm)?;
    let renderer = device.create_renderer()?;
    Ok(Rc::new(Context(renderer)))
}

#[derive(Debug)]
struct Context(Rc<VulkanRenderer>);

impl GfxContext for Context {
    fn reset_status(&self) -> Option<ResetStatus> {
        None
    }

    fn render_node(&self) -> Rc<CString> {
        self.0.device.render_node.clone()
    }

    fn formats(&self) -> Rc<AHashMap<u32, GfxFormat>> {
        self.0.formats.clone()
    }

    fn dmabuf_img(self: Rc<Self>, buf: &DmaBuf) -> Result<Rc<dyn GfxImage>, GfxError> {
        self.0
            .import_dmabuf(buf)
            .map(|v| v as _)
            .map_err(|e| e.into())
    }

    fn shmem_texture(
        self: Rc<Self>,
        old: Option<Rc<dyn GfxTexture>>,
        data: &[Cell<u8>],
        format: &'static Format,
        width: i32,
        height: i32,
        stride: i32,
    ) -> Result<Rc<dyn GfxTexture>, GfxError> {
        if let Some(old) = old {
            let old = old.into_vk(&self.0.device.device);
            let shm = match &old.ty {
                VulkanImageMemory::DmaBuf(_) => unreachable!(),
                VulkanImageMemory::Internal(shm) => shm,
            };
            if old.width as i32 == width
                && old.height as i32 == height
                && shm.stride as i32 == stride
                && old.format.vk_format == format.vk_format
            {
                shm.upload(data)?;
                return Ok(old);
            }
        }
        let tex = self
            .0
            .create_shm_texture(format, width, height, stride, data, false)?;
        Ok(tex as _)
    }

    fn gbm(&self) -> &GbmDevice {
        &self.0.device.gbm
    }

    fn gfx_api(&self) -> GfxApi {
        GfxApi::Vulkan
    }

    fn create_fb(
        self: Rc<Self>,
        width: i32,
        height: i32,
        stride: i32,
        format: &'static Format,
    ) -> Result<Rc<dyn GfxFramebuffer>, GfxError> {
        let fb = self
            .0
            .create_shm_texture(format, width, height, stride, &[], true)?;
        Ok(fb)
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        self.0.on_drop();
    }
}
