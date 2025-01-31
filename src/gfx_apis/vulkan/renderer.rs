use {
    crate::{
        async_engine::SpawnedFuture,
        format::Format,
        gfx_api::{GfxApiOpt, GfxFormat, GfxFramebuffer, GfxTexture},
        gfx_apis::vulkan::{
            allocator::VulkanAllocator,
            command::{VulkanCommandBuffer, VulkanCommandPool},
            device::VulkanDevice,
            fence::VulkanFence,
            image::{VulkanImage, VulkanImageMemory},
            pipeline::{PipelineCreateInfo, VulkanPipeline},
            semaphore::VulkanSemaphore,
            shaders::{
                FillFragPushConstants, FillVertPushConstants, TexVertPushConstants, FILL_FRAG,
                FILL_VERT, TEX_FRAG, TEX_VERT,
            },
            staging::VulkanStagingBuffer,
            VulkanError,
        },
        io_uring::IoUring,
        theme::Color,
        utils::{copyhashmap::CopyHashMap, errorfmt::ErrorFmt, numcell::NumCell, stack::Stack},
        video::dmabuf::{
            dma_buf_export_sync_file, dma_buf_import_sync_file, DMA_BUF_SYNC_READ,
            DMA_BUF_SYNC_WRITE,
        },
    },
    ahash::AHashMap,
    ash::{
        vk::{
            AccessFlags2, AttachmentLoadOp, AttachmentStoreOp, BufferImageCopy, BufferImageCopy2,
            BufferMemoryBarrier2, ClearColorValue, ClearValue, CommandBuffer,
            CommandBufferBeginInfo, CommandBufferSubmitInfo, CommandBufferUsageFlags,
            CopyBufferToImageInfo2, DependencyInfo, DependencyInfoKHR, DescriptorImageInfo,
            DescriptorType, Extent2D, Extent3D, Fence, ImageAspectFlags, ImageLayout,
            ImageMemoryBarrier2, ImageMemoryBarrier2Builder, ImageSubresourceLayers,
            ImageSubresourceRange, PipelineBindPoint, PipelineStageFlags2, Rect2D,
            RenderingAttachmentInfo, RenderingInfo, SemaphoreSubmitInfo, SemaphoreSubmitInfoKHR,
            ShaderStageFlags, SubmitInfo2, Viewport, WriteDescriptorSet, QUEUE_FAMILY_FOREIGN_EXT,
        },
        Device,
    },
    isnt::std_1::collections::IsntHashMapExt,
    std::{
        cell::{Cell, RefCell},
        fmt::{Debug, Formatter},
        mem, ptr,
        rc::Rc,
        slice,
    },
    uapi::OwnedFd,
};

pub struct VulkanRenderer {
    pub(super) formats: Rc<AHashMap<u32, GfxFormat>>,
    pub(super) device: Rc<VulkanDevice>,
    pub(super) fill_pipeline: Rc<VulkanPipeline>,
    pub(super) tex_pipeline: Rc<VulkanPipeline>,
    pub(super) command_pool: Rc<VulkanCommandPool>,
    pub(super) command_buffers: Stack<Rc<VulkanCommandBuffer>>,
    pub(super) wait_semaphores: Stack<Rc<VulkanSemaphore>>,
    pub(super) total_buffers: NumCell<usize>,
    pub(super) memory: RefCell<Memory>,
    pub(super) pending_frames: CopyHashMap<u64, Rc<PendingFrame>>,
    pub(super) allocator: Rc<VulkanAllocator>,
    pub(super) last_point: NumCell<u64>,
}

#[derive(Default)]
pub(super) struct Memory {
    sample: Vec<Rc<VulkanImage>>,
    flush: Vec<Rc<VulkanImage>>,
    flush_staging: Vec<(Rc<VulkanImage>, VulkanStagingBuffer)>,
    textures: Vec<Rc<VulkanImage>>,
    image_barriers: Vec<ImageMemoryBarrier2>,
    shm_barriers: Vec<BufferMemoryBarrier2>,
    wait_semaphores: Vec<Rc<VulkanSemaphore>>,
    wait_semaphore_infos: Vec<SemaphoreSubmitInfo>,
    release_fence: Option<Rc<VulkanFence>>,
    release_syncfile: Option<Rc<OwnedFd>>,
}

pub(super) struct PendingFrame {
    point: u64,
    renderer: Rc<VulkanRenderer>,
    cmd: Cell<Option<Rc<VulkanCommandBuffer>>>,
    _textures: Vec<Rc<VulkanImage>>,
    _staging: Vec<(Rc<VulkanImage>, VulkanStagingBuffer)>,
    wait_semaphores: Cell<Vec<Rc<VulkanSemaphore>>>,
    waiter: Cell<Option<SpawnedFuture<()>>>,
    _release_fence: Option<Rc<VulkanFence>>,
}

impl VulkanDevice {
    pub fn create_renderer(self: &Rc<Self>) -> Result<Rc<VulkanRenderer>, VulkanError> {
        let fill_pipeline = self.create_pipeline::<FillVertPushConstants, FillFragPushConstants>(
            PipelineCreateInfo {
                vert: self.create_shader(FILL_VERT)?,
                frag: self.create_shader(FILL_FRAG)?,
                alpha: true,
                frag_descriptor_set_layout: None,
            },
        )?;
        let sampler = self.create_sampler()?;
        let tex_descriptor_set_layout = self.create_descriptor_set_layout(&sampler)?;
        let tex_pipeline =
            self.create_pipeline::<TexVertPushConstants, ()>(PipelineCreateInfo {
                vert: self.create_shader(TEX_VERT)?,
                frag: self.create_shader(TEX_FRAG)?,
                alpha: true,
                frag_descriptor_set_layout: Some(tex_descriptor_set_layout.clone()),
            })?;
        let command_pool = self.create_command_pool()?;
        let formats: AHashMap<u32, _> = self
            .formats
            .iter()
            .map(|(drm, vk)| {
                (
                    *drm,
                    GfxFormat {
                        format: vk.format,
                        read_modifiers: vk
                            .modifiers
                            .values()
                            .filter(|m| m.texture_max_extents.is_some())
                            .map(|m| m.modifier)
                            .collect(),
                        write_modifiers: vk
                            .modifiers
                            .values()
                            .filter(|m| m.render_max_extents.is_some())
                            .map(|m| m.modifier)
                            .collect(),
                    },
                )
            })
            .collect();
        let allocator = self.create_allocator()?;
        Ok(Rc::new(VulkanRenderer {
            formats: Rc::new(formats),
            device: self.clone(),
            fill_pipeline,
            tex_pipeline,
            command_pool,
            command_buffers: Default::default(),
            wait_semaphores: Default::default(),
            total_buffers: Default::default(),
            memory: Default::default(),
            pending_frames: Default::default(),
            allocator,
            last_point: Default::default(),
        }))
    }
}

impl VulkanRenderer {
    fn collect_memory(&self, opts: &[GfxApiOpt]) {
        let mut memory = self.memory.borrow_mut();
        memory.sample.clear();
        memory.flush.clear();
        for cmd in opts {
            if let GfxApiOpt::CopyTexture(c) = cmd {
                let tex = c.tex.clone().into_vk(&self.device.device);
                match &tex.ty {
                    VulkanImageMemory::DmaBuf(_) => memory.sample.push(tex.clone()),
                    VulkanImageMemory::Internal(shm) => {
                        if shm.to_flush.borrow_mut().is_some() {
                            memory.flush.push(tex.clone());
                        }
                    }
                }
                memory.textures.push(tex);
            }
        }
    }

    fn begin_command_buffer(&self, buf: CommandBuffer) -> Result<(), VulkanError> {
        let begin_info =
            CommandBufferBeginInfo::builder().flags(CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            self.device
                .device
                .begin_command_buffer(buf, &begin_info)
                .map_err(VulkanError::BeginCommandBuffer)
        }
    }

    fn write_shm_staging_buffers(self: &Rc<Self>) -> Result<(), VulkanError> {
        let mut memory = self.memory.borrow_mut();
        let memory = &mut *memory;
        memory.flush_staging.clear();
        for img in &memory.flush {
            let shm = match &img.ty {
                VulkanImageMemory::DmaBuf(_) => unreachable!(),
                VulkanImageMemory::Internal(s) => s,
            };
            let staging = self.create_staging_buffer(shm.size, true, false, true)?;
            let to_flush = shm.to_flush.borrow_mut();
            let to_flush = to_flush.as_ref().unwrap();
            staging.upload(|mem, size| unsafe {
                let size = size.min(to_flush.len());
                ptr::copy_nonoverlapping(to_flush.as_ptr(), mem, size);
            })?;
            memory.flush_staging.push((img.clone(), staging));
        }
        Ok(())
    }

    fn initial_barriers(&self, buf: CommandBuffer, fb: &VulkanImage) {
        let mut memory = self.memory.borrow_mut();
        let memory = &mut *memory;
        memory.image_barriers.clear();
        memory.shm_barriers.clear();
        let fb_image_memory_barrier = image_barrier()
            .src_queue_family_index(QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(self.device.graphics_queue_idx)
            .image(fb.image)
            .old_layout(if fb.is_undefined.get() {
                ImageLayout::UNDEFINED
            } else {
                ImageLayout::GENERAL
            })
            .new_layout(ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .dst_access_mask(AccessFlags2::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .build();
        memory.image_barriers.push(fb_image_memory_barrier);
        for img in &memory.sample {
            let image_memory_barrier = image_barrier()
                .src_queue_family_index(QUEUE_FAMILY_FOREIGN_EXT)
                .dst_queue_family_index(self.device.graphics_queue_idx)
                .image(img.image)
                .old_layout(ImageLayout::GENERAL)
                .new_layout(ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .dst_access_mask(AccessFlags2::SHADER_SAMPLED_READ)
                .dst_stage_mask(PipelineStageFlags2::FRAGMENT_SHADER)
                .build();
            memory.image_barriers.push(image_memory_barrier);
        }
        for (img, staging) in &memory.flush_staging {
            let image_memory_barrier = image_barrier()
                .image(img.image)
                .old_layout(if img.is_undefined.get() {
                    ImageLayout::UNDEFINED
                } else {
                    ImageLayout::SHADER_READ_ONLY_OPTIMAL
                })
                .new_layout(ImageLayout::TRANSFER_DST_OPTIMAL)
                .dst_access_mask(AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(PipelineStageFlags2::TRANSFER)
                .build();
            memory.image_barriers.push(image_memory_barrier);
            let buffer_memory_barrier = BufferMemoryBarrier2::builder()
                .buffer(staging.buffer)
                .offset(0)
                .size(staging.size)
                .src_access_mask(AccessFlags2::HOST_WRITE)
                .src_stage_mask(PipelineStageFlags2::HOST)
                .dst_access_mask(AccessFlags2::TRANSFER_READ)
                .dst_stage_mask(PipelineStageFlags2::TRANSFER)
                .build();
            memory.shm_barriers.push(buffer_memory_barrier);
        }
        let dep_info = DependencyInfoKHR::builder()
            .buffer_memory_barriers(&memory.shm_barriers)
            .image_memory_barriers(&memory.image_barriers);
        unsafe {
            self.device.device.cmd_pipeline_barrier2(buf, &dep_info);
        }
    }

    fn copy_shm_to_image(&self, cmd: CommandBuffer) {
        let memory = self.memory.borrow_mut();
        for (img, staging) in &memory.flush_staging {
            let cpy = BufferImageCopy2::builder()
                .buffer_image_height(img.height)
                .buffer_row_length(img.stride / img.format.bpp)
                .image_extent(Extent3D {
                    width: img.width,
                    height: img.height,
                    depth: 1,
                })
                .image_subresource(ImageSubresourceLayers {
                    aspect_mask: ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .build();
            let info = CopyBufferToImageInfo2::builder()
                .src_buffer(staging.buffer)
                .dst_image(img.image)
                .dst_image_layout(ImageLayout::TRANSFER_DST_OPTIMAL)
                .regions(slice::from_ref(&cpy));
            unsafe {
                self.device.device.cmd_copy_buffer_to_image2(cmd, &info);
            }
        }
    }

    fn secondary_barriers(&self, buf: CommandBuffer) {
        let mut memory = self.memory.borrow_mut();
        let memory = &mut *memory;
        if memory.flush.is_empty() {
            return;
        }
        memory.image_barriers.clear();
        for img in &memory.flush {
            let image_memory_barrier = image_barrier()
                .image(img.image)
                .old_layout(ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(AccessFlags2::TRANSFER_WRITE)
                .src_stage_mask(PipelineStageFlags2::TRANSFER)
                .dst_access_mask(AccessFlags2::SHADER_SAMPLED_READ)
                .dst_stage_mask(PipelineStageFlags2::FRAGMENT_SHADER)
                .build();
            memory.image_barriers.push(image_memory_barrier);
        }
        let dep_info = DependencyInfoKHR::builder().image_memory_barriers(&memory.image_barriers);
        unsafe {
            self.device.device.cmd_pipeline_barrier2(buf, &dep_info);
        }
    }

    fn begin_rendering(&self, buf: CommandBuffer, fb: &VulkanImage, clear: Option<&Color>) {
        let rendering_attachment_info = {
            let mut rai = RenderingAttachmentInfo::builder()
                .image_view(fb.render_view.unwrap_or(fb.texture_view))
                .image_layout(ImageLayout::GENERAL)
                .load_op(AttachmentLoadOp::LOAD)
                .store_op(AttachmentStoreOp::STORE);
            if let Some(clear) = clear {
                rai = rai
                    .clear_value(ClearValue {
                        color: ClearColorValue {
                            float32: clear.to_array_srgb(),
                        },
                    })
                    .load_op(AttachmentLoadOp::CLEAR);
            }
            rai
        };
        let rendering_info = RenderingInfo::builder()
            .render_area(Rect2D {
                offset: Default::default(),
                extent: Extent2D {
                    width: fb.width,
                    height: fb.height,
                },
            })
            .layer_count(1)
            .color_attachments(slice::from_ref(&rendering_attachment_info));
        unsafe {
            self.device.device.cmd_begin_rendering(buf, &rendering_info);
        }
    }

    fn set_viewport(&self, buf: CommandBuffer, fb: &VulkanImage) {
        let viewport = Viewport {
            x: 0.0,
            y: 0.0,
            width: fb.width as _,
            height: fb.height as _,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        let scissor = Rect2D {
            offset: Default::default(),
            extent: Extent2D {
                width: fb.width,
                height: fb.height,
            },
        };
        unsafe {
            self.device
                .device
                .cmd_set_viewport(buf, 0, slice::from_ref(&viewport));
            self.device
                .device
                .cmd_set_scissor(buf, 0, slice::from_ref(&scissor));
        }
    }

    fn record_draws(&self, buf: CommandBuffer, opts: &[GfxApiOpt]) -> Result<(), VulkanError> {
        let dev = &self.device.device;
        let mut current_pipeline = None;
        let mut bind = |pipeline: &VulkanPipeline| {
            if current_pipeline != Some(pipeline.pipeline) {
                current_pipeline = Some(pipeline.pipeline);
                unsafe {
                    dev.cmd_bind_pipeline(buf, PipelineBindPoint::GRAPHICS, pipeline.pipeline);
                }
            }
        };
        for opt in opts {
            match opt {
                GfxApiOpt::Sync => {}
                GfxApiOpt::FillRect(r) => {
                    bind(&self.fill_pipeline);
                    let vert = FillVertPushConstants {
                        pos: r.rect.to_points(),
                    };
                    let frag = FillFragPushConstants {
                        color: r.color.to_array_srgb(),
                    };
                    unsafe {
                        dev.cmd_push_constants(
                            buf,
                            self.fill_pipeline.pipeline_layout,
                            ShaderStageFlags::VERTEX,
                            0,
                            uapi::as_bytes(&vert),
                        );
                        dev.cmd_push_constants(
                            buf,
                            self.fill_pipeline.pipeline_layout,
                            ShaderStageFlags::FRAGMENT,
                            self.fill_pipeline.frag_push_offset,
                            uapi::as_bytes(&frag),
                        );
                        dev.cmd_draw(buf, 4, 1, 0, 0);
                    }
                }
                GfxApiOpt::CopyTexture(c) => {
                    let tex = c.tex.as_vk(&self.device.device);
                    bind(&self.tex_pipeline);
                    let vert = TexVertPushConstants {
                        pos: c.target.to_points(),
                        tex_pos: c.source.to_points(),
                    };
                    let image_info = DescriptorImageInfo::builder()
                        .image_view(tex.texture_view)
                        .image_layout(ImageLayout::SHADER_READ_ONLY_OPTIMAL);
                    let write_descriptor_set = WriteDescriptorSet::builder()
                        .descriptor_type(DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(slice::from_ref(&image_info))
                        .build();
                    unsafe {
                        self.device.push_descriptor.cmd_push_descriptor_set(
                            buf,
                            PipelineBindPoint::GRAPHICS,
                            self.tex_pipeline.pipeline_layout,
                            0,
                            slice::from_ref(&write_descriptor_set),
                        );
                        dev.cmd_push_constants(
                            buf,
                            self.tex_pipeline.pipeline_layout,
                            ShaderStageFlags::VERTEX,
                            0,
                            uapi::as_bytes(&vert),
                        );
                        dev.cmd_draw(buf, 4, 1, 0, 0);
                    }
                }
            }
        }
        Ok(())
    }

    fn end_rendering(&self, buf: CommandBuffer) {
        unsafe {
            self.device.device.cmd_end_rendering(buf);
        }
    }

    fn final_barriers(&self, buf: CommandBuffer, fb: &VulkanImage) {
        let mut memory = self.memory.borrow_mut();
        let memory = &mut *memory;
        memory.image_barriers.clear();
        memory.shm_barriers.clear();
        let fb_image_memory_barrier = image_barrier()
            .src_queue_family_index(self.device.graphics_queue_idx)
            .dst_queue_family_index(QUEUE_FAMILY_FOREIGN_EXT)
            .image(fb.image)
            .old_layout(ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(ImageLayout::GENERAL)
            .src_access_mask(
                AccessFlags2::COLOR_ATTACHMENT_WRITE | AccessFlags2::COLOR_ATTACHMENT_READ,
            )
            .src_stage_mask(PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)
            .build();
        memory.image_barriers.push(fb_image_memory_barrier);
        for img in &memory.sample {
            let image_memory_barrier = image_barrier()
                .src_queue_family_index(self.device.graphics_queue_idx)
                .dst_queue_family_index(QUEUE_FAMILY_FOREIGN_EXT)
                .old_layout(ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(ImageLayout::GENERAL)
                .image(img.image)
                .src_access_mask(AccessFlags2::SHADER_SAMPLED_READ)
                .src_stage_mask(PipelineStageFlags2::FRAGMENT_SHADER)
                .build();
            memory.image_barriers.push(image_memory_barrier);
        }
        let dep_info = DependencyInfoKHR::builder()
            .image_memory_barriers(&memory.image_barriers)
            .buffer_memory_barriers(&memory.shm_barriers);
        unsafe {
            self.device.device.cmd_pipeline_barrier2(buf, &dep_info);
        }
    }

    fn end_command_buffer(&self, buf: CommandBuffer) -> Result<(), VulkanError> {
        unsafe {
            self.device
                .device
                .end_command_buffer(buf)
                .map_err(VulkanError::EndCommandBuffer)
        }
    }

    fn create_wait_semaphores(&self, fb: &VulkanImage) -> Result<(), VulkanError> {
        let mut memory = self.memory.borrow_mut();
        let memory = &mut *memory;
        memory.wait_semaphore_infos.clear();
        let import = |infos: &mut Vec<SemaphoreSubmitInfoKHR>,
                      semaphores: &mut Vec<Rc<VulkanSemaphore>>,
                      img: &VulkanImage,
                      flag: u32|
         -> Result<(), VulkanError> {
            if let VulkanImageMemory::DmaBuf(buf) = &img.ty {
                for plane in &buf.template.dmabuf.planes {
                    let fd = dma_buf_export_sync_file(&plane.fd, flag)
                        .map_err(VulkanError::IoctlExportSyncFile)?;
                    let semaphore = self.allocate_semaphore()?;
                    semaphore.import_syncfile(fd)?;
                    infos.push(
                        SemaphoreSubmitInfo::builder()
                            .semaphore(semaphore.semaphore)
                            .stage_mask(PipelineStageFlags2::TOP_OF_PIPE)
                            .build(),
                    );
                    semaphores.push(semaphore);
                }
            }
            Ok(())
        };
        for texture in &memory.textures {
            import(
                &mut memory.wait_semaphore_infos,
                &mut memory.wait_semaphores,
                texture,
                DMA_BUF_SYNC_READ,
            )?;
        }
        import(
            &mut memory.wait_semaphore_infos,
            &mut memory.wait_semaphores,
            fb,
            DMA_BUF_SYNC_WRITE,
        )?;
        Ok(())
    }

    fn import_release_semaphore(&self, fb: &VulkanImage) {
        let memory = self.memory.borrow();
        let syncfile = match memory.release_syncfile.as_ref() {
            Some(syncfile) => syncfile,
            _ => return,
        };
        let import = |img: &VulkanImage, flag: u32| {
            if let VulkanImageMemory::DmaBuf(buf) = &img.ty {
                for plane in &buf.template.dmabuf.planes {
                    let res = dma_buf_import_sync_file(&plane.fd, flag, &syncfile)
                        .map_err(VulkanError::IoctlImportSyncFile);
                    if let Err(e) = res {
                        log::error!("Could not import syncfile into dmabuf: {}", ErrorFmt(e));
                        log::warn!("Relying on implicit sync");
                    }
                }
            }
        };
        for texture in &memory.textures {
            import(texture, DMA_BUF_SYNC_WRITE);
        }
        import(fb, DMA_BUF_SYNC_READ | DMA_BUF_SYNC_WRITE);
    }

    fn submit(&self, buf: CommandBuffer) -> Result<(), VulkanError> {
        let mut memory = self.memory.borrow_mut();
        let release_fence = self.device.create_fence()?;
        let command_buffer_info = CommandBufferSubmitInfo::builder()
            .command_buffer(buf)
            .build();
        let submit_info = SubmitInfo2::builder()
            .wait_semaphore_infos(&memory.wait_semaphore_infos)
            .command_buffer_infos(slice::from_ref(&command_buffer_info))
            .build();
        unsafe {
            self.device
                .device
                .queue_submit2(
                    self.device.graphics_queue,
                    slice::from_ref(&submit_info),
                    release_fence.fence,
                )
                .map_err(VulkanError::Submit)?;
        }
        let release_syncfile = match release_fence.export_syncfile() {
            Ok(s) => Some(s),
            Err(e) => {
                log::error!("Could not export syncfile from fence: {}", ErrorFmt(e));
                None
            }
        };
        memory.release_fence = Some(release_fence);
        memory.release_syncfile = release_syncfile;
        Ok(())
    }

    fn store_layouts(&self, fb: &VulkanImage) {
        fb.is_undefined.set(false);
        let memory = self.memory.borrow_mut();
        for img in &memory.flush {
            img.is_undefined.set(false);
            let shm = match &img.ty {
                VulkanImageMemory::DmaBuf(_) => unreachable!(),
                VulkanImageMemory::Internal(s) => s,
            };
            shm.to_flush.take();
        }
    }

    fn create_pending_frame(self: &Rc<Self>, buf: Rc<VulkanCommandBuffer>) {
        let point = self.last_point.fetch_add(1) + 1;
        let mut memory = self.memory.borrow_mut();
        let frame = Rc::new(PendingFrame {
            point,
            renderer: self.clone(),
            cmd: Cell::new(Some(buf)),
            _textures: mem::take(&mut memory.textures),
            _staging: mem::take(&mut memory.flush_staging),
            wait_semaphores: Cell::new(mem::take(&mut memory.wait_semaphores)),
            waiter: Cell::new(None),
            _release_fence: memory.release_fence.take(),
        });
        self.pending_frames.set(frame.point, frame.clone());
        let future = self.device.instance.eng.spawn(await_release(
            memory.release_syncfile.take(),
            self.device.instance.ring.clone(),
            frame.clone(),
            self.clone(),
        ));
        frame.waiter.set(Some(future));
    }

    pub fn read_pixels(
        self: &Rc<Self>,
        tex: &Rc<VulkanImage>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: &'static Format,
        dst: &[Cell<u8>],
    ) -> Result<(), VulkanError> {
        if x < 0 || y < 0 || width <= 0 || height <= 0 || stride <= 0 {
            return Err(VulkanError::InvalidShmParameters {
                x,
                y,
                width,
                height,
                stride,
            });
        }
        let width = width as u32;
        let height = height as u32;
        let stride = stride as u32;
        if x == 0 && y == 0 && width == tex.width && height == tex.height && format == tex.format {
            return self.read_all_pixels(tex, stride, dst);
        }
        let tmp_tex = self.create_shm_texture(
            format,
            width as i32,
            height as i32,
            stride as i32,
            &[],
            true,
        )?;
        (&*tmp_tex as &dyn GfxFramebuffer).copy_texture(&(tex.clone() as _), x, y);
        self.read_all_pixels(&tmp_tex, stride, dst)
    }

    fn read_all_pixels(
        self: &Rc<Self>,
        tex: &VulkanImage,
        stride: u32,
        dst: &[Cell<u8>],
    ) -> Result<(), VulkanError> {
        if stride < tex.width * tex.format.bpp || stride % tex.format.bpp != 0 {
            return Err(VulkanError::InvalidStride);
        }
        let size = stride as u64 * tex.height as u64;
        if size != dst.len() as u64 {
            return Err(VulkanError::InvalidBufferSize);
        }
        let region = BufferImageCopy::builder()
            .buffer_row_length(stride / tex.format.bpp)
            .buffer_image_height(tex.height)
            .image_subresource(ImageSubresourceLayers {
                aspect_mask: ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(Extent3D {
                width: tex.width,
                height: tex.height,
                depth: 1,
            })
            .build();
        let staging = self.create_staging_buffer(size, false, true, true)?;
        let initial_tex_barrier = image_barrier()
            .src_queue_family_index(QUEUE_FAMILY_FOREIGN_EXT)
            .dst_queue_family_index(self.device.graphics_queue_idx)
            .image(tex.image)
            .old_layout(ImageLayout::GENERAL)
            .new_layout(ImageLayout::TRANSFER_SRC_OPTIMAL)
            .dst_access_mask(AccessFlags2::TRANSFER_READ)
            .dst_stage_mask(PipelineStageFlags2::TRANSFER);
        let initial_buffer_barrier = BufferMemoryBarrier2::builder()
            .buffer(staging.buffer)
            .offset(0)
            .size(staging.size)
            .dst_access_mask(AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(PipelineStageFlags2::TRANSFER);
        let initial_barriers = DependencyInfo::builder()
            .buffer_memory_barriers(slice::from_ref(&initial_buffer_barrier))
            .image_memory_barriers(slice::from_ref(&initial_tex_barrier));
        let final_tex_barrier = image_barrier()
            .src_queue_family_index(self.device.graphics_queue_idx)
            .dst_queue_family_index(QUEUE_FAMILY_FOREIGN_EXT)
            .image(tex.image)
            .old_layout(ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(ImageLayout::GENERAL)
            .src_access_mask(AccessFlags2::TRANSFER_READ)
            .src_stage_mask(PipelineStageFlags2::TRANSFER);
        let final_buffer_barrier = BufferMemoryBarrier2::builder()
            .buffer(staging.buffer)
            .offset(0)
            .size(staging.size)
            .src_access_mask(AccessFlags2::TRANSFER_WRITE)
            .src_stage_mask(PipelineStageFlags2::TRANSFER)
            .dst_access_mask(AccessFlags2::HOST_READ)
            .dst_stage_mask(PipelineStageFlags2::HOST);
        let final_barriers = DependencyInfo::builder()
            .buffer_memory_barriers(slice::from_ref(&final_buffer_barrier))
            .image_memory_barriers(slice::from_ref(&final_tex_barrier));
        let buf = self.allocate_command_buffer()?;
        let mut semaphores = vec![];
        let mut semaphore_infos = vec![];
        if let VulkanImageMemory::DmaBuf(buf) = &tex.ty {
            for plane in &buf.template.dmabuf.planes {
                let fd = dma_buf_export_sync_file(&plane.fd, DMA_BUF_SYNC_READ)
                    .map_err(VulkanError::IoctlExportSyncFile)?;
                let semaphore = self.allocate_semaphore()?;
                semaphore.import_syncfile(fd)?;
                let semaphore_info = SemaphoreSubmitInfo::builder()
                    .semaphore(semaphore.semaphore)
                    .stage_mask(PipelineStageFlags2::TOP_OF_PIPE)
                    .build();
                semaphores.push(semaphore);
                semaphore_infos.push(semaphore_info);
            }
        }
        let command_buffer_info = CommandBufferSubmitInfo::builder().command_buffer(buf.buffer);
        let submit_info = SubmitInfo2::builder()
            .wait_semaphore_infos(&semaphore_infos)
            .command_buffer_infos(slice::from_ref(&command_buffer_info));
        let begin_info =
            CommandBufferBeginInfo::builder().flags(CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        unsafe {
            self.device
                .device
                .begin_command_buffer(buf.buffer, &begin_info)
                .map_err(VulkanError::BeginCommandBuffer)?;
            self.device
                .device
                .cmd_pipeline_barrier2(buf.buffer, &initial_barriers);
            self.device.device.cmd_copy_image_to_buffer(
                buf.buffer,
                tex.image,
                ImageLayout::TRANSFER_SRC_OPTIMAL,
                staging.buffer,
                &[region],
            );
            self.device
                .device
                .cmd_pipeline_barrier2(buf.buffer, &final_barriers);
            self.device
                .device
                .end_command_buffer(buf.buffer)
                .map_err(VulkanError::EndCommandBuffer)?;
            self.device
                .device
                .queue_submit2(
                    self.device.graphics_queue,
                    slice::from_ref(&submit_info),
                    Fence::null(),
                )
                .map_err(VulkanError::Submit)?;
        }
        self.block();
        self.command_buffers.push(buf);
        for semaphore in semaphores {
            self.wait_semaphores.push(semaphore);
        }
        staging.download(|mem, size| unsafe {
            ptr::copy_nonoverlapping(mem, dst.as_ptr() as _, size);
        })?;
        Ok(())
    }

    pub fn execute(
        self: &Rc<Self>,
        fb: &VulkanImage,
        opts: &[GfxApiOpt],
        clear: Option<&Color>,
    ) -> Result<(), VulkanError> {
        let res = self.try_execute(fb, opts, clear);
        {
            let mut memory = self.memory.borrow_mut();
            memory.flush.clear();
            memory.textures.clear();
            memory.flush_staging.clear();
            memory.sample.clear();
            memory.wait_semaphores.clear();
            memory.release_fence.take();
            memory.release_syncfile.take();
        }
        res
    }

    fn allocate_command_buffer(&self) -> Result<Rc<VulkanCommandBuffer>, VulkanError> {
        let buf = match self.command_buffers.pop() {
            Some(b) => b,
            _ => {
                self.total_buffers.fetch_add(1);
                self.command_pool.allocate_buffer()?
            }
        };
        Ok(buf)
    }

    fn allocate_semaphore(&self) -> Result<Rc<VulkanSemaphore>, VulkanError> {
        let semaphore = match self.wait_semaphores.pop() {
            Some(s) => s,
            _ => self.device.create_semaphore()?,
        };
        Ok(semaphore)
    }

    fn try_execute(
        self: &Rc<Self>,
        fb: &VulkanImage,
        opts: &[GfxApiOpt],
        clear: Option<&Color>,
    ) -> Result<(), VulkanError> {
        let buf = self.allocate_command_buffer()?;
        self.collect_memory(opts);
        self.begin_command_buffer(buf.buffer)?;
        self.write_shm_staging_buffers()?;
        self.initial_barriers(buf.buffer, fb);
        self.copy_shm_to_image(buf.buffer);
        self.secondary_barriers(buf.buffer);
        self.begin_rendering(buf.buffer, fb, clear);
        self.set_viewport(buf.buffer, fb);
        self.record_draws(buf.buffer, opts)?;
        self.end_rendering(buf.buffer);
        self.final_barriers(buf.buffer, fb);
        self.end_command_buffer(buf.buffer)?;
        self.create_wait_semaphores(fb)?;
        self.submit(buf.buffer)?;
        self.import_release_semaphore(fb);
        self.store_layouts(fb);
        self.create_pending_frame(buf);
        Ok(())
    }

    fn block(&self) {
        log::warn!("Blocking.");
        unsafe {
            if let Err(e) = self.device.device.device_wait_idle() {
                log::error!("Could not wait for device idle: {}", ErrorFmt(e));
            }
        }
    }

    pub fn on_drop(&self) {
        let mut pending_frames = self.pending_frames.lock();
        if pending_frames.is_not_empty() {
            log::warn!("Context dropped with pending frames.");
            self.block();
        }
        pending_frames.values().for_each(|f| {
            f.waiter.take();
        });
        pending_frames.clear();
    }
}

impl Debug for VulkanRenderer {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VulkanRenderer").finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct TmpShmTexture(pub i32, pub i32);

impl VulkanImage {
    fn assert_device(&self, device: &Device) {
        assert_eq!(
            self.renderer.device.device.handle(),
            device.handle(),
            "Mixed vulkan device use"
        );
    }
}

impl dyn GfxTexture {
    fn as_vk(&self, device: &Device) -> &VulkanImage {
        let img: &VulkanImage = self
            .as_any()
            .downcast_ref()
            .expect("Non-vulkan texture passed into vulkan");
        img.assert_device(device);
        img
    }

    pub(super) fn into_vk(self: Rc<Self>, device: &Device) -> Rc<VulkanImage> {
        let img: Rc<VulkanImage> = self
            .into_any()
            .downcast()
            .expect("Non-vulkan texture passed into vulkan");
        img.assert_device(device);
        img
    }
}

fn image_barrier() -> ImageMemoryBarrier2Builder<'static> {
    ImageMemoryBarrier2::builder().subresource_range(
        ImageSubresourceRange::builder()
            .aspect_mask(ImageAspectFlags::COLOR)
            .layer_count(1)
            .level_count(1)
            .build(),
    )
}

async fn await_release(
    syncfile: Option<Rc<OwnedFd>>,
    ring: Rc<IoUring>,
    frame: Rc<PendingFrame>,
    renderer: Rc<VulkanRenderer>,
) {
    let mut is_released = false;
    if let Some(syncfile) = syncfile {
        if let Err(e) = ring.readable(&syncfile).await {
            log::error!(
                "Could not wait for release semaphore to be signaled: {}",
                ErrorFmt(e)
            );
        } else {
            is_released = true;
        }
    }
    if !is_released {
        frame.renderer.block();
    }
    if let Some(buf) = frame.cmd.take() {
        frame.renderer.command_buffers.push(buf);
    }
    for wait_semaphore in frame.wait_semaphores.take() {
        frame.renderer.wait_semaphores.push(wait_semaphore);
    }
    renderer.pending_frames.remove(&frame.point);
}
