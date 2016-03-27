// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use std::iter::Empty;
use std::ops::Range;
use std::sync::Arc;
use smallvec::SmallVec;

use command_buffer::Submission;
use device::Device;
use format::FormatDesc;
use image::sys::Dimensions;
use image::sys::Layout;
use image::sys::UnsafeImage;
use image::sys::UnsafeImageView;
use image::sys::Usage;
use image::traits::AccessRange;
use image::traits::GpuAccessResult;
use image::traits::Image;
use image::traits::ImageContent;
use image::traits::ImageView;
use instance::QueueFamily;
use memory::DeviceMemory;
use sync::Sharing;

use OomError;

/// Image whose purpose is to be used for read-only purposes. You can write to the image once,
/// but then you must only ever read from it. TODO: clarify because of blit operations
// TODO: type (2D, 3D, array, etc.) as template parameter
pub struct ImmutableImage<F> {
    image: UnsafeImage,
    view: UnsafeImageView,
    memory: DeviceMemory,
    format: F,
}

impl<F> ImmutableImage<F> {
    pub fn new<'a, I>(device: &Arc<Device>, dimensions: Dimensions, format: F, queue_families: I)
                      -> Result<Arc<ImmutableImage<F>>, OomError>
        where F: FormatDesc, I: IntoIterator<Item = QueueFamily<'a>>
    {
        let usage = Usage {
            transfer_source: true,  // for blits
            transfer_dest: true,
            sampled: true,
            .. Usage::none()
        };

        let queue_families = queue_families.into_iter().map(|f| f.id())
                                           .collect::<SmallVec<[u32; 4]>>();

        let (image, mem_reqs) = unsafe {
            let sharing = if queue_families.len() >= 2 {
                Sharing::Concurrent(queue_families.iter().cloned())
            } else {
                Sharing::Exclusive
            };

            try!(UnsafeImage::new(device, &usage, format.format(), dimensions,
                                  1, 1, Sharing::Exclusive::<Empty<u32>>, false, false))
        };

        let mem_ty = {
            let device_local = device.physical_device().memory_types()
                                     .filter(|t| (mem_reqs.memory_type_bits & (1 << t.id())) != 0)
                                     .filter(|t| t.is_device_local());
            let any = device.physical_device().memory_types()
                            .filter(|t| (mem_reqs.memory_type_bits & (1 << t.id())) != 0);
            device_local.chain(any).next().unwrap()
        };

        // note: alignment doesn't need to be checked because allocating memory is guaranteed to
        //       fulfill any alignment requirement

        let mem = try!(DeviceMemory::alloc(device, &mem_ty, mem_reqs.size));
        unsafe { try!(image.bind_memory(&mem, 0 .. mem_reqs.size)); }

        let view = unsafe {
            try!(UnsafeImageView::new(&image))
        };

        Ok(Arc::new(ImmutableImage {
            image: image,
            view: view,
            memory: mem,
            format: format,
        }))
    }

    #[inline]
    pub fn dimensions(&self) -> Dimensions {
        self.image.dimensions()
    }
}

unsafe impl<F> Image for ImmutableImage<F> {
    #[inline]
    fn inner_image(&self) -> &UnsafeImage {
        &self.image
    }

    #[inline]
    fn blocks(&self, _: Range<u32>, _: Range<u32>) -> Vec<(u32, u32)> {
        vec![(0, 0)]
    }

    #[inline]
    fn block_mipmap_levels_range(&self, block: (u32, u32)) -> Range<u32> {
        0 .. 1
    }

    #[inline]
    fn block_array_layers_range(&self, block: (u32, u32)) -> Range<u32> {
        0 .. 1
    }

    #[inline]
    fn initial_layout(&self, _: (u32, u32), first_usage: Layout) -> (Layout, bool, bool) {
        let l = if first_usage == Layout::TransferDstOptimal {
            Layout::Undefined
        } else {
            Layout::ShaderReadOnlyOptimal
        };

        (l, false, false)
    }

    #[inline]
    fn final_layout(&self, _: (u32, u32), _: Layout) -> (Layout, bool, bool) {
        (Layout::ShaderReadOnlyOptimal, false, false)
    }

    fn needs_fence(&self, access: &mut Iterator<Item = AccessRange>) -> Option<bool> {
        Some(false)
    }

    unsafe fn gpu_access(&self, access: &mut Iterator<Item = AccessRange>,
                         submission: &Arc<Submission>) -> GpuAccessResult
    {
        GpuAccessResult {
            dependencies: vec![],
            additional_wait_semaphore: None,
            additional_signal_semaphore: None,
            before_transitions: vec![],
            after_transitions: vec![],
        }
    }
}

unsafe impl<P, F> ImageContent<P> for ImmutableImage<F> {
    #[inline]
    fn matches_format(&self) -> bool {
        true        // FIXME:
    }
}

unsafe impl<F: 'static> ImageView for ImmutableImage<F> {
    #[inline]
    fn parent(&self) -> &Image {
        self
    }

    #[inline]
    fn parent_arc(me: &Arc<Self>) -> Arc<Image> where Self: Sized {
        me.clone() as Arc<_>
    }

    #[inline]
    fn inner_view(&self) -> &UnsafeImageView {
        &self.view
    }

    #[inline]
    fn descriptor_set_storage_image_layout(&self, _: AccessRange) -> Layout {
        Layout::ShaderReadOnlyOptimal
    }

    #[inline]
    fn descriptor_set_combined_image_sampler_layout(&self, _: AccessRange) -> Layout {
        Layout::ShaderReadOnlyOptimal
    }

    #[inline]
    fn descriptor_set_sampled_image_layout(&self, _: AccessRange) -> Layout {
        Layout::ShaderReadOnlyOptimal
    }

    #[inline]
    fn descriptor_set_input_attachment_layout(&self, _: AccessRange) -> Layout {
        Layout::ShaderReadOnlyOptimal
    }

    #[inline]
    fn identity_swizzle(&self) -> bool {
        true
    }
}