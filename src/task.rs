use crate::context::{
    DEVICE_ADDRESS_BUFFER_BINDING, SPECIAL_BUFFER_BINDING, SPECIAL_IMAGE_BINDING,
};
use crate::device::{DeviceInner, MAX_FRAMES_IN_FLIGHT};
use crate::prelude::*;

use std::borrow::{Borrow, BorrowMut};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::ops;
use std::slice;
use std::sync::{Arc, Mutex};
use std::time;
use std::{mem, ptr};

use ash::vk;

use bitflags::bitflags;

pub struct Present {
    pub wait_semaphore: BinarySemaphore,
}

pub struct Submit {
    pub wait_semaphore: Option<BinarySemaphore>,
    pub signal_semaphore: Option<BinarySemaphore>,
}

pub struct RenderGraphInfo<'a> {
    pub swapchain: Swapchain,
    pub debug_name: &'a str,
}

impl Default for RenderGraphInfo<'_> {
    fn default() -> Self {
        Self {
            swapchain: u32::MAX.into(),
            debug_name: "RenderGraphBuilder",
        }
    }
}

pub struct RenderGraphBuilder<'a, T> {
    pub(crate) device: Arc<DeviceInner>,
    pub(crate) swapchain: Swapchain,
    pub(crate) nodes: Vec<Node<'a, T>>,
    pub(crate) debug_name: String,
}

impl<'a, T> RenderGraphBuilder<'a, T> {
    pub fn add<'b: 'a, F: ops::FnMut(&mut T, &mut Commands) -> Result<()> + Send + Sync + 'b>(
        &mut self,
        task: Task<T, F>,
    ) {
        let Task { task, resources } = task;

        self.nodes.push(Node {
            resources,
            task: Box::new(task),
        });
    }

    pub fn complete(self) -> Result<RenderGraph<'a, T>> {
        let RenderGraphBuilder {
            device,
            nodes,
            swapchain,
            ..
        } = self;

        let DeviceInner {
            logical_device,
            command_pool,
            resources,
            ..
        } = &*device;

        let resources = resources.lock().unwrap();

        let command_buffer_allocate_info = vk::CommandBufferAllocateInfo {
            command_pool: *command_pool,
            level: vk::CommandBufferLevel::PRIMARY,
            command_buffer_count: MAX_FRAMES_IN_FLIGHT as _,
            ..Default::default()
        };

        let command_buffers =
            unsafe { logical_device.allocate_command_buffers(&command_buffer_allocate_info) }
                .map_err(|_| Error::Creation)?;

        let fence_create_info = vk::FenceCreateInfo {
            flags: vk::FenceCreateFlags::SIGNALED,
            ..Default::default()
        };

        let mut fences = vec![];

        for i in 0..MAX_FRAMES_IN_FLIGHT {
            let fence = unsafe { logical_device.create_fence(&fence_create_info, None) }
                .map_err(|_| Error::Creation)?;

            fences.push(fence);
        }

        let current_instant = time::Instant::now();

        Ok(RenderGraph {
            inner: Arc::new(RenderGraphInner {
                device: device.clone(),
                command_buffers,
                fences,
                swapchain,
                modify: Mutex::new(RenderGraphModify {
                    nodes,
                    current_instant,
                    last_instant: current_instant,
                }),
            }),
        })
    }
}

pub struct RenderGraph<'a, T> {
    inner: Arc<RenderGraphInner<'a, T>>,
}

impl<'a, T> Clone for RenderGraph<'a, T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

pub struct RenderGraphInner<'a, T> {
    pub(crate) device: Arc<DeviceInner>,
    pub(crate) swapchain: Swapchain,
    pub(crate) command_buffers: Vec<vk::CommandBuffer>,
    pub(crate) fences: Vec<vk::Fence>,
    pub(crate) modify: Mutex<RenderGraphModify<'a, T>>,
}

pub struct RenderGraphModify<'a, T> {
    pub(crate) current_instant: time::Instant,
    pub(crate) last_instant: time::Instant,
    pub(crate) nodes: Vec<Node<'a, T>>,
}

impl<T> RenderGraph<'_, T> {
    pub fn frame_time(&self) -> time::Duration {
        let modify = self.inner.modify.lock().unwrap();

        modify.current_instant.duration_since(modify.last_instant)
    }
}

impl<T> RenderGraph<'_, T> {
    ///Executes the render graph.
    pub fn render(&mut self, home: &mut T) {
        profiling::scope!("RenderGraph", "ev");

        let RenderGraphInner {
            device,
            command_buffers,
            fences,
            modify,
            swapchain,
            ..
        } = &*self.inner;

        let mut modify = modify.lock().unwrap();

        let DeviceInner {
            logical_device,
            queue_family_indices,
            resources,
            #[cfg(all(feature = "bindless"))]
            bindless,
            ..
        } = &**device;

        let mut submit: Option<Submit> = None;
        let mut present: Option<Present> = None;

        let (swapchain_handle, current_frame) = {
            let resources = resources.lock().unwrap();

            let internal_swapchain = resources.swapchains.get(*swapchain).unwrap();

            let swapchain_handle = internal_swapchain.handle;

            let current_frame = internal_swapchain.current_frame;

            (swapchain_handle, current_frame)
        };

        let queue_family_index = queue_family_indices[0];

        let queue = unsafe { logical_device.get_device_queue(queue_family_index as _, 0) };

        {
            profiling::scope!("fence", "ev");
            unsafe {
                if let Err(vk::Result::TIMEOUT) =
                    logical_device.wait_for_fences(&[fences[current_frame]], true, 0)
                {
                    return;
                }
            }

            modify.last_instant = modify.current_instant;
            modify.current_instant = time::Instant::now();

            unsafe {
                logical_device.reset_fences(&[fences[current_frame]]);
            }
        }

        unsafe {
            logical_device
                .begin_command_buffer(command_buffers[current_frame], &Default::default());
        }

        #[cfg(all(feature = "bindless"))]
        {
            profiling::scope!("address book and descriptor set", "ev");

            let resources = resources.lock().unwrap();

            let mut addresses = vec![0u64; DESCRIPTOR_COUNT as usize];

            let mut descriptor_buffer_infos = vec![];
            let mut descriptor_image_infos = vec![];

            for i in 0..resources.buffers.count() as usize {
                if let Some(internal_buffer) = resources.buffers.get((i as u32).into()) {
                    let buffer_device_address_info = vk::BufferDeviceAddressInfo {
                        buffer: internal_buffer.buffer,
                        ..Default::default()
                    };

                    addresses[i] = unsafe {
                        logical_device.get_buffer_device_address(&buffer_device_address_info)
                    };

                    descriptor_buffer_infos.push(vk::DescriptorBufferInfo {
                        buffer: internal_buffer.buffer,
                        offset: 0,
                        range: internal_buffer.size as _,
                    })
                } else {
                    descriptor_buffer_infos.push(vk::DescriptorBufferInfo {
                        buffer: vk::Buffer::null(),
                        offset: 0,
                        range: vk::WHOLE_SIZE,
                        ..Default::default()
                    });
                }
            }

            for i in 0..resources.images.count() as usize {
                if let Some(internal_image) = resources.images.get((i as u32).into()) {
                    if internal_image.get_format().is_depth_or_stencil() {
                        descriptor_image_infos.push(vk::DescriptorImageInfo {
                            ..Default::default()
                        });
                        continue;
                    }
                    if let InternalImage::Swapchain { .. } = internal_image {
                        descriptor_image_infos.push(vk::DescriptorImageInfo {
                            ..Default::default()
                        });
                        continue;
                    }

                    descriptor_image_infos.push(vk::DescriptorImageInfo {
                        image_view: internal_image.get_image_view(),
                        image_layout: vk::ImageLayout::GENERAL,
                        ..Default::default()
                    });
                } else {
                    descriptor_image_infos.push(vk::DescriptorImageInfo {
                        ..Default::default()
                    });
                }
            }

            drop(resources);

            let address_buffer_size = (DESCRIPTOR_COUNT * mem::size_of::<u64>() as u32) as u64;

            let dst = unsafe {
                logical_device.map_memory(
                    *staging_address_memory,
                    0,
                    address_buffer_size as _,
                    vk::MemoryMapFlags::empty(),
                )
            }
            .unwrap();

            unsafe { slice::from_raw_parts_mut(dst as *mut _, addresses.len()) }
                .copy_from_slice(&addresses[..]);

            unsafe {
                logical_device.unmap_memory(*staging_address_memory);
            }

            let regions = [vk::BufferCopy {
                src_offset: 0,
                dst_offset: 0,
                size: address_buffer_size as _,
            }];

            unsafe {
                logical_device.cmd_copy_buffer(
                    command_buffers[current_frame],
                    *staging_address_buffer,
                    *general_address_buffer,
                    &regions,
                );
            }

            let descriptor_buffer_info = vk::DescriptorBufferInfo {
                buffer: *general_address_buffer,
                offset: 0,
                range: address_buffer_size as _,
            };

            let mut write_descriptor_sets = vec![];

            write_descriptor_sets.push({
                let p_buffer_info = &descriptor_buffer_info;

                vk::WriteDescriptorSet {
                    dst_set: *descriptor_set,
                    dst_binding: DEVICE_ADDRESS_BUFFER_BINDING, //MAGIC NUMBER SEE context.rs or hexane.glsl
                    descriptor_count: 1,
                    descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
                    p_buffer_info,
                    ..Default::default()
                }
            });

            if descriptor_buffer_infos.len() != 0 {
                write_descriptor_sets.push({
                    let p_buffer_info = descriptor_buffer_infos.as_ptr();

                    vk::WriteDescriptorSet {
                        dst_set: *descriptor_set,
                        dst_binding: SPECIAL_BUFFER_BINDING, //MAGIC NUMBER SEE context.rs or hexane.glsl
                        descriptor_count: descriptor_buffer_infos.len() as _,
                        descriptor_type: vk::DescriptorType::STORAGE_BUFFER,
                        p_buffer_info,
                        ..Default::default()
                    }
                });
            }

            if descriptor_image_infos.len() != 0 {
                write_descriptor_sets.push({
                    let p_image_info = descriptor_image_infos.as_ptr();

                    vk::WriteDescriptorSet {
                        dst_set: *descriptor_set,
                        dst_binding: SPECIAL_IMAGE_BINDING, //MAGIC NUMBER SEE context.rs or hexane.glsl
                        descriptor_count: descriptor_image_infos.len() as _,
                        descriptor_type: vk::DescriptorType::STORAGE_IMAGE,
                        p_image_info,
                        ..Default::default()
                    }
                });
            }
            unsafe {
                logical_device.update_descriptor_sets(&write_descriptor_sets, &[]);
            }
        }

        //TODO make auto sync smarter
        let mut last_image_access = HashMap::<Image, ImageAccess>::new();
        let mut last_buffer_access = HashMap::<Buffer, BufferAccess>::new();

        for (i, node) in modify.nodes.iter_mut().enumerate() {
            profiling::scope!("task", "ev");
            let qualifiers = node
                .resources
                .iter()
                .map(|resource| resource.resolve(home))
                .collect::<Vec<_>>();

            let mut naive_barriers = vec![];

            for (i, qualifier) in qualifiers.iter().enumerate() {
                match qualifier {
                    Qualifier::Buffer(buffer, dst) => {
                        let src = last_buffer_access.entry(*buffer).or_default();

                        let offset = 0;

                        let size = {
                            let resources = resources.lock().unwrap();

                            resources.buffers.get(*buffer).unwrap().size
                        };

                        naive_barriers.push(PipelineBarrier {
                            src_stage: (*src).into(),
                            dst_stage: (*dst).into(),
                            barriers: vec![Barrier::Buffer {
                                buffer: i,
                                offset,
                                size,
                                src_access: (*src).into(),
                                dst_access: (*dst).into(),
                            }],
                        });

                        last_buffer_access.insert(*buffer, *dst);
                    }
                    Qualifier::Image(image, dst, image_aspect) => {
                        let src = last_image_access.entry(*image).or_default();

                        u32::from(*image);

                        naive_barriers.push(PipelineBarrier {
                            src_stage: (*src).into(),
                            dst_stage: (*dst).into(),
                            barriers: vec![Barrier::Image {
                                image: i,
                                old_layout: (*src).into(),
                                new_layout: (*dst).into(),
                                src_access: (*src).into(),
                                dst_access: (*dst).into(),
                                image_aspect: (*image_aspect),
                            }],
                        });

                        last_image_access.insert(*image, *dst);
                    }
                }
            }

            let mut smart_barriers =
                HashMap::<(PipelineStage, PipelineStage), PipelineBarrier>::new();

            for new_barrier in naive_barriers {
                let key = (new_barrier.src_stage, new_barrier.dst_stage);

                if smart_barriers.contains_key(&key) {
                    smart_barriers
                        .get_mut(&key)
                        .unwrap()
                        .barriers
                        .extend(new_barrier.barriers);
                } else {
                    smart_barriers.insert(key, new_barrier);
                }
            }

            let mut commands = Commands {
                device: &device,
                qualifiers: &qualifiers,
                swapchain: &swapchain,
                command_buffer: &command_buffers[current_frame],
                submit: &mut submit,
                present: &mut present,
            };

            for (_, barrier) in smart_barriers {
                commands.pipeline_barrier(barrier).unwrap();
            }
            (node.task)(home, &mut commands).unwrap();
        }

        unsafe {
            logical_device.end_command_buffer(command_buffers[current_frame]);
        }

        if let Some(submit) = submit {
            profiling::scope!("submit", "ev");

            let resources = resources.lock().unwrap();

            let internal_swapchain = resources.swapchains.get(*swapchain).unwrap();

            let wait_dst_stage_mask = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];

            let submit_info = {
                let p_wait_dst_stage_mask = wait_dst_stage_mask.as_ptr();

                let wait_semaphore_count = submit.wait_semaphore.is_some() as u32;

                let p_wait_semaphores = submit.wait_semaphore.map(|x| {
                    &resources.binary_semaphores.get(x).unwrap().semaphores[current_frame]
                });

                let p_wait_semaphores = p_wait_semaphores
                    .map(|x| x as *const _)
                    .unwrap_or(ptr::null());

                let signal_semaphore_count = submit.signal_semaphore.is_some() as u32;

                let p_signal_semaphores = submit.signal_semaphore.map(|x| {
                    &resources.binary_semaphores.get(x).unwrap().semaphores[current_frame]
                });

                let p_signal_semaphores = p_signal_semaphores
                    .map(|x| x as *const _)
                    .unwrap_or(ptr::null());

                let command_buffer_count = 1;

                let p_command_buffers = &command_buffers[current_frame];

                vk::SubmitInfo {
                    p_wait_dst_stage_mask,
                    wait_semaphore_count,
                    p_wait_semaphores,
                    signal_semaphore_count,
                    p_signal_semaphores,
                    command_buffer_count,
                    p_command_buffers,
                    ..Default::default()
                }
            };

            unsafe {
                logical_device.queue_submit(queue, &[submit_info], fences[current_frame]);
            }
        }

        if let Some(present) = present {
            profiling::scope!("submit", "ev");

            let resources = resources.lock().unwrap();

            let internal_swapchain = resources.swapchains.get(*swapchain).unwrap();

            let image_index = internal_swapchain.last_acquisition_index.unwrap();

            let wait_semaphore = resources
                .binary_semaphores
                .get(present.wait_semaphore)
                .unwrap()
                .semaphores[current_frame];

            let present_info = {
                let swapchain_count = 1;

                let p_swapchains = &swapchain_handle;

                let wait_semaphore_count = 1;

                let p_wait_semaphores = &wait_semaphore;

                let p_image_indices = &image_index;

                vk::PresentInfoKHR {
                    wait_semaphore_count,
                    p_wait_semaphores,
                    swapchain_count,
                    p_swapchains,
                    p_image_indices,
                    ..Default::default()
                }
            };

            unsafe {
                internal_swapchain
                    .loader
                    .queue_present(queue, &present_info);
            }
        }

        {
            let mut resources = resources.lock().unwrap();

            let mut internal_swapchain = resources.swapchains.get_mut(*swapchain).unwrap();

            let current_frame = internal_swapchain.current_frame;

            internal_swapchain.current_frame = (current_frame + 1) % MAX_FRAMES_IN_FLIGHT;
            internal_swapchain.allow_acquisition = true;

            drop(resources);
        }
    }
}

#[derive(Clone, Copy, Default)]
pub enum ImageAccess {
    #[default]
    None,
    ShaderReadOnly,
    VertexShaderReadOnly,
    FragmentShaderReadOnly,
    ComputeShaderReadOnly,
    ShaderWriteOnly,
    VertexShaderWriteOnly,
    FragmentShaderWriteOnly,
    ComputeShaderWriteOnly,
    ShaderReadWrite,
    VertexShaderReadWrite,
    FragmentShaderReadWrite,
    ComputeShaderReadWrite,
    TransferRead,
    TransferWrite,
    ColorAttachment,
    DepthAttachment,
    DepthAttachmentReadOnly,
    DepthStencilAttachment,
    Present,
}

impl From<ImageAccess> for PipelineStage {
    fn from(access: ImageAccess) -> Self {
        match access {
            ImageAccess::None => PipelineStage::empty(),
            ImageAccess::Present => PipelineStage::ALL_COMMANDS,
            ImageAccess::TransferWrite | ImageAccess::TransferRead => PipelineStage::TRANSFER,
            ImageAccess::DepthAttachmentReadOnly
            | ImageAccess::DepthAttachment
            | ImageAccess::DepthStencilAttachment => {
                PipelineStage::EARLY_FRAGMENT_TESTS | PipelineStage::LATE_FRAGMENT_TESTS
            }
            ImageAccess::ShaderReadWrite
            | ImageAccess::ShaderWriteOnly
            | ImageAccess::ShaderReadOnly => {
                PipelineStage::ALL_GRAPHICS | PipelineStage::COMPUTE_SHADER
            }
            ImageAccess::VertexShaderReadWrite
            | ImageAccess::VertexShaderWriteOnly
            | ImageAccess::VertexShaderReadOnly => PipelineStage::VERTEX_SHADER,
            ImageAccess::FragmentShaderReadWrite
            | ImageAccess::FragmentShaderWriteOnly
            | ImageAccess::FragmentShaderReadOnly => PipelineStage::FRAGMENT_SHADER,
            ImageAccess::ComputeShaderReadWrite
            | ImageAccess::ComputeShaderWriteOnly
            | ImageAccess::ComputeShaderReadOnly => PipelineStage::COMPUTE_SHADER,
            ImageAccess::ColorAttachment => PipelineStage::COLOR_ATTACHMENT_OUTPUT,
        }
    }
}

impl From<ImageAccess> for ImageLayout {
    fn from(access: ImageAccess) -> Self {
        match access {
            ImageAccess::None => ImageLayout::Undefined,
            ImageAccess::Present => ImageLayout::Present,
            ImageAccess::TransferWrite => ImageLayout::TransferDstOptimal,
            ImageAccess::TransferRead => ImageLayout::TransferSrcOptimal,
            ImageAccess::DepthStencilAttachment => ImageLayout::AttachmentOptimal,
            ImageAccess::DepthAttachment => ImageLayout::AttachmentOptimal,
            ImageAccess::DepthAttachmentReadOnly
            | ImageAccess::VertexShaderReadOnly
            | ImageAccess::FragmentShaderReadOnly
            | ImageAccess::ComputeShaderReadOnly
            | ImageAccess::ShaderReadOnly => ImageLayout::ReadOnlyOptimal,
            ImageAccess::ShaderWriteOnly
            | ImageAccess::VertexShaderWriteOnly
            | ImageAccess::FragmentShaderWriteOnly
            | ImageAccess::ComputeShaderWriteOnly
            | ImageAccess::VertexShaderReadWrite
            | ImageAccess::FragmentShaderReadWrite
            | ImageAccess::ComputeShaderReadWrite
            | ImageAccess::ShaderReadWrite => ImageLayout::General,
            ImageAccess::ColorAttachment => ImageLayout::AttachmentOptimal,
        }
    }
}

impl From<ImageAccess> for Access {
    fn from(access: ImageAccess) -> Self {
        match access {
            ImageAccess::None => Access::empty(),
            ImageAccess::Present
            | ImageAccess::TransferRead
            | ImageAccess::DepthAttachmentReadOnly
            | ImageAccess::ShaderReadOnly
            | ImageAccess::VertexShaderReadOnly
            | ImageAccess::FragmentShaderReadOnly
            | ImageAccess::ComputeShaderReadOnly => Access::READ,
            ImageAccess::TransferWrite
            | ImageAccess::ShaderWriteOnly
            | ImageAccess::VertexShaderWriteOnly
            | ImageAccess::FragmentShaderWriteOnly
            | ImageAccess::ComputeShaderWriteOnly => Access::WRITE,
            ImageAccess::ColorAttachment
            | ImageAccess::DepthAttachment
            | ImageAccess::DepthStencilAttachment
            | ImageAccess::ShaderReadWrite
            | ImageAccess::VertexShaderReadWrite
            | ImageAccess::FragmentShaderReadWrite
            | ImageAccess::ComputeShaderReadWrite => Access::READ | Access::WRITE,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub enum BufferAccess {
    #[default]
    None,
    ShaderReadOnly,
    VertexShaderReadOnly,
    FragmentShaderReadOnly,
    ComputeShaderReadOnly,
    ShaderWriteOnly,
    VertexShaderWriteOnly,
    FragmentShaderWriteOnly,
    ComputeShaderWriteOnly,
    ShaderReadWrite,
    VertexShaderReadWrite,
    FragmentShaderReadWrite,
    ComputeShaderReadWrite,
    TransferRead,
    TransferWrite,
    HostTransferRead,
    HostTransferWrite,
}

impl From<BufferAccess> for PipelineStage {
    fn from(access: BufferAccess) -> Self {
        match access {
            BufferAccess::None => PipelineStage::empty(),
            BufferAccess::ShaderReadOnly => {
                PipelineStage::ALL_GRAPHICS | PipelineStage::COMPUTE_SHADER
            }
            BufferAccess::VertexShaderReadOnly => PipelineStage::VERTEX_SHADER,
            BufferAccess::FragmentShaderReadOnly => PipelineStage::FRAGMENT_SHADER,
            BufferAccess::ComputeShaderReadOnly => PipelineStage::COMPUTE_SHADER,
            BufferAccess::ShaderWriteOnly => {
                PipelineStage::ALL_GRAPHICS | PipelineStage::COMPUTE_SHADER
            }
            BufferAccess::VertexShaderWriteOnly => PipelineStage::VERTEX_SHADER,
            BufferAccess::FragmentShaderWriteOnly => PipelineStage::FRAGMENT_SHADER,
            BufferAccess::ComputeShaderWriteOnly => PipelineStage::COMPUTE_SHADER,
            BufferAccess::ShaderReadWrite => {
                PipelineStage::ALL_GRAPHICS | PipelineStage::COMPUTE_SHADER
            }
            BufferAccess::VertexShaderReadWrite => PipelineStage::VERTEX_SHADER,
            BufferAccess::FragmentShaderReadWrite => PipelineStage::FRAGMENT_SHADER,
            BufferAccess::ComputeShaderReadWrite => PipelineStage::COMPUTE_SHADER,
            BufferAccess::TransferRead => PipelineStage::TRANSFER,
            BufferAccess::TransferWrite => PipelineStage::TRANSFER,
            BufferAccess::HostTransferRead => PipelineStage::HOST,
            BufferAccess::HostTransferWrite => PipelineStage::HOST,
        }
    }
}

impl From<BufferAccess> for Access {
    fn from(access: BufferAccess) -> Self {
        match access {
            BufferAccess::None => Access::empty(),
            BufferAccess::HostTransferRead
            | BufferAccess::TransferRead
            | BufferAccess::ShaderReadOnly
            | BufferAccess::VertexShaderReadOnly
            | BufferAccess::FragmentShaderReadOnly
            | BufferAccess::ComputeShaderReadOnly => Access::READ,
            BufferAccess::HostTransferWrite
            | BufferAccess::TransferWrite
            | BufferAccess::ShaderWriteOnly
            | BufferAccess::VertexShaderWriteOnly
            | BufferAccess::FragmentShaderWriteOnly
            | BufferAccess::ComputeShaderWriteOnly => Access::WRITE,
            BufferAccess::ShaderReadWrite
            | BufferAccess::VertexShaderReadWrite
            | BufferAccess::FragmentShaderReadWrite
            | BufferAccess::ComputeShaderReadWrite => Access::READ | Access::WRITE,
        }
    }
}

pub enum Resource<T> {
    Buffer(
        Box<dyn ops::Fn(&mut T) -> Buffer + Send + Sync>,
        BufferAccess,
    ),
    Image(
        Box<dyn ops::Fn(&mut T) -> Image + Send + Sync>,
        ImageAccess,
        ImageAspect,
    ),
}

impl<T> Resource<T> {
    pub(crate) fn resolve(&self, t: &mut T) -> Qualifier {
        match self {
            Resource::Buffer(call, access) => Qualifier::Buffer((call)(t), *access),
            Resource::Image(call, access, aspect) => Qualifier::Image((call)(t), *access, *aspect),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Qualifier {
    Buffer(Buffer, BufferAccess),
    Image(Image, ImageAccess, ImageAspect),
}

pub struct Task<T, F: ops::FnMut(&mut T, &mut Commands) -> Result<()> + Send + Sync> {
    pub resources: Vec<Resource<T>>,
    pub task: F,
}

pub struct Node<'a, T> {
    pub resources: Vec<Resource<T>>,
    pub task: Box<dyn ops::FnMut(&mut T, &mut Commands) -> Result<()> + Send + Sync + 'a>,
}
