// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use std::error;
use std::fmt;
use std::mem;
use std::ptr;
use std::sync::Arc;
use smallvec::SmallVec;

use device::Device;
use format::FormatDesc;
use framebuffer::RenderPass;
use framebuffer::LayoutAttachmentDescription;
use framebuffer::LayoutPassDescription;
use framebuffer::LayoutPassDependencyDescription;
use image::traits::Image;
use image::traits::ImageView;

use Error;
use OomError;
use VulkanObject;
use VulkanPointers;
use check_errors;
use vk;

/// Defines the layout of multiple subpasses.
pub struct UnsafeRenderPass {
    renderpass: vk::RenderPass,
    device: Arc<Device>,
}

impl UnsafeRenderPass {
    /// Builds a new renderpass.
    ///
    /// # Safety
    ///
    /// This function doesn't check whether all the restrictions in the attachments, passes and
    /// passes dependencies were enforced.
    ///
    /// See the documentation of the structs of this module for more info about these restrictions.
    ///
    /// # Panic
    ///
    /// - Can panick if it detects some violations in the restrictions. Only unexpensive checks are
    /// performed. `debug_assert!` is used, so some restrictions are only checked in debug
    /// mode.
    ///
    pub unsafe fn new<Ia, Ip, Id>(device: &Arc<Device>, attachments: Ia, passes: Ip,
                                  pass_dependencies: Id)
                                  -> Result<UnsafeRenderPass, RenderPassCreationError>
        where Ia: ExactSizeIterator<Item = LayoutAttachmentDescription> + Clone,        // with specialization we can handle the "Clone" restriction internally
              Ip: ExactSizeIterator<Item = LayoutPassDescription> + Clone,      // with specialization we can handle the "Clone" restriction internally
              Id: ExactSizeIterator<Item = LayoutPassDependencyDescription>
    {
        let vk = device.pointers();

        // TODO: check the validity of the renderpass layout with debug_assert!
        //   - If the first use of an attachment in this render pass is as an input attachment, and the attachment is not also used as a color or depth/stencil attachment in the same subpass, then loadOp must not be VK_ATTACHMENT_LOAD_OP_CLEAR

        let attachments = attachments.clone().map(|attachment| {
            debug_assert!(attachment.samples.is_power_of_two());

            vk::AttachmentDescription {
                flags: 0,       // FIXME: may alias flag
                format: attachment.format as u32,
                samples: attachment.samples,
                loadOp: attachment.load as u32,
                storeOp: attachment.store as u32,
                stencilLoadOp: attachment.load as u32,       // TODO: allow user to choose
                stencilStoreOp: attachment.store as u32,      // TODO: allow user to choose
                initialLayout: attachment.initial_layout as u32,
                finalLayout: attachment.final_layout as u32,
            }
        }).collect::<SmallVec<[_; 16]>>();

        // We need to pass pointers to vkAttachmentReference structs when creating the renderpass.
        // Therefore we need to allocate them in advance.
        //
        // This block allocates, for each pass, in order, all color attachment references, then all
        // input attachment references, then all resolve attachment references, then the depth
        // stencil attachment reference.
        let attachment_references = passes.clone().flat_map(|pass| {
            // Performing some validation with debug asserts.
            debug_assert!(pass.resolve_attachments.is_empty() ||
                          pass.resolve_attachments.len() == pass.color_attachments.len());
            debug_assert!(pass.resolve_attachments.iter().all(|a| {
                              attachments[a.0].samples == 1
                          }));
            debug_assert!(pass.resolve_attachments.is_empty() ||
                          pass.color_attachments.iter().all(|a| {
                              attachments[a.0].samples > 1
                          }));
            debug_assert!(pass.resolve_attachments.is_empty() ||
                          pass.resolve_attachments.iter().zip(pass.color_attachments.iter())
                              .all(|(r, c)| {
                                  attachments[r.0].format == attachments[c.0].format
                              }));
            debug_assert!(pass.color_attachments.iter().cloned()
                              .chain(pass.depth_stencil.clone().into_iter())
                              .chain(pass.input_attachments.iter().cloned())
                              .chain(pass.resolve_attachments.iter().cloned())
                              .all(|(a, _)| {
                                  pass.preserve_attachments.iter().find(|&&b| a == b).is_none()
                              }));
            debug_assert!(pass.color_attachments.iter().cloned()
                              .chain(pass.depth_stencil.clone().into_iter())
                              .all(|(atch, layout)| {
                                  if let Some(r) = pass.input_attachments.iter()
                                                                         .find(|r| r.0 == atch)
                                  {
                                      r.1 == layout
                                  } else {
                                      true
                                  }
                              }));

            let resolve = pass.resolve_attachments.into_iter().map(|(offset, img_la)| {
                debug_assert!(offset < attachments.len());
                vk::AttachmentReference { attachment: offset as u32, layout: img_la as u32, }
            });

            let color = pass.color_attachments.into_iter().map(|(offset, img_la)| {
                debug_assert!(offset < attachments.len());
                vk::AttachmentReference { attachment: offset as u32, layout: img_la as u32, }
            });

            let input = pass.input_attachments.into_iter().map(|(offset, img_la)| {
                debug_assert!(offset < attachments.len());
                vk::AttachmentReference { attachment: offset as u32, layout: img_la as u32, }
            });

            let depthstencil = if let Some((offset, img_la)) = pass.depth_stencil {
                Some(vk::AttachmentReference { attachment: offset as u32, layout: img_la as u32, })
            } else {
                None
            }.into_iter();

            color.chain(input).chain(resolve).chain(depthstencil)
        }).collect::<SmallVec<[_; 16]>>();

        // Same as `attachment_references` but only for the preserve attachments.
        // This is separate because attachment references are u32s and not `vkAttachmentReference`
        // structs.
        let preserve_attachments_references = passes.clone().flat_map(|pass| {
            pass.preserve_attachments.into_iter().map(|offset| offset as u32)
        }).collect::<SmallVec<[_; 16]>>();

        // Now iterating over passes.
        let passes = {
            // `ref_index` and `preserve_ref_index` are increased during the loop and point to the
            // next element to use in respectively `attachment_references` and
            // `preserve_attachments_references`.
            let mut ref_index = 0usize;
            let mut preserve_ref_index = 0usize;
            let mut out: SmallVec<[_; 16]> = SmallVec::new();

            for pass in passes.clone() {
                if pass.color_attachments.len() as u32 >
                   device.physical_device().limits().max_color_attachments()
                {
                    return Err(RenderPassCreationError::ColorAttachmentsLimitExceeded);
                }

                let color_attachments = attachment_references.as_ptr().offset(ref_index as isize);
                ref_index += pass.color_attachments.len();
                let input_attachments = attachment_references.as_ptr().offset(ref_index as isize);
                ref_index += pass.input_attachments.len();
                let resolve_attachments = attachment_references.as_ptr().offset(ref_index as isize);
                ref_index += pass.resolve_attachments.len();
                let depth_stencil = if pass.depth_stencil.is_some() {
                    let a = attachment_references.as_ptr().offset(ref_index as isize);
                    ref_index += 1;
                    a
                } else {
                    ptr::null()
                };

                let preserve_attachments = preserve_attachments_references.as_ptr()
                                                              .offset(preserve_ref_index as isize);
                preserve_ref_index += pass.preserve_attachments.len();

                out.push(vk::SubpassDescription {
                    flags: 0,   // reserved
                    pipelineBindPoint: vk::PIPELINE_BIND_POINT_GRAPHICS,
                    inputAttachmentCount: pass.input_attachments.len() as u32,
                    pInputAttachments: if pass.input_attachments.is_empty() { ptr::null() }
                                       else { input_attachments },
                    colorAttachmentCount: pass.color_attachments.len() as u32,
                    pColorAttachments: if pass.color_attachments.is_empty() { ptr::null() }
                                       else { color_attachments },
                    pResolveAttachments: if pass.resolve_attachments.is_empty() { ptr::null() }
                                         else { resolve_attachments },
                    pDepthStencilAttachment: depth_stencil,
                    preserveAttachmentCount: pass.preserve_attachments.len() as u32,
                    pPreserveAttachments: if pass.preserve_attachments.is_empty() { ptr::null() }
                                          else { preserve_attachments },
                });
            }

            assert!(!out.is_empty());
            // If these assertions fails, there's a serious bug in the code above ^.
            debug_assert!(ref_index == attachment_references.len());
            debug_assert!(preserve_ref_index == preserve_attachments_references.len());

            out
        };

        let dependencies = pass_dependencies.map(|dependency| {
            debug_assert!(dependency.source_subpass < passes.len());
            debug_assert!(dependency.destination_subpass < passes.len());

            vk::SubpassDependency {
                srcSubpass: dependency.source_subpass as u32,
                dstSubpass: dependency.destination_subpass as u32,
                srcStageMask: vk::PIPELINE_STAGE_ALL_GRAPHICS_BIT,      // FIXME:
                dstStageMask: vk::PIPELINE_STAGE_ALL_GRAPHICS_BIT,      // FIXME:
                srcAccessMask: 0x0001FFFF,       // FIXME:
                dstAccessMask: 0x0001FFFF,       // FIXME:
                dependencyFlags: if dependency.by_region { vk::DEPENDENCY_BY_REGION_BIT } else { 0 },
            }
        }).collect::<SmallVec<[_; 16]>>();

        let renderpass = {
            let infos = vk::RenderPassCreateInfo {
                sType: vk::STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
                pNext: ptr::null(),
                flags: 0,   // reserved
                attachmentCount: attachments.len() as u32,
                pAttachments: if attachments.is_empty() { ptr::null() }
                              else { attachments.as_ptr() },
                subpassCount: passes.len() as u32,
                pSubpasses: if passes.is_empty() { ptr::null() } else { passes.as_ptr() },
                dependencyCount: dependencies.len() as u32,
                pDependencies: if dependencies.is_empty() { ptr::null() }
                               else { dependencies.as_ptr() },
            };

            let mut output = mem::uninitialized();
            try!(check_errors(vk.CreateRenderPass(device.internal_object(), &infos,
                                                  ptr::null(), &mut output)));
            output
        };

        Ok(UnsafeRenderPass {
            device: device.clone(),
            renderpass: renderpass,
        })
    }

    /// Returns the device that was used to create this render pass.
    #[inline]
    pub fn device(&self) -> &Arc<Device> {
        &self.device
    }
}

unsafe impl VulkanObject for UnsafeRenderPass {
    type Object = vk::RenderPass;

    #[inline]
    fn internal_object(&self) -> vk::RenderPass {
        self.renderpass
    }
}

unsafe impl RenderPass for UnsafeRenderPass {
    #[inline]
    fn render_pass(&self) -> &UnsafeRenderPass {
        self
    }
}

impl Drop for UnsafeRenderPass {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            let vk = self.device.pointers();
            vk.DestroyRenderPass(self.device.internal_object(), self.renderpass, ptr::null());
        }
    }
}

/// Error that can happen when creating a compute pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderPassCreationError {
    /// Not enough memory.
    OomError(OomError),
    /// The maximum number of color attachments has been exceeded.
    ColorAttachmentsLimitExceeded,
}

impl error::Error for RenderPassCreationError {
    #[inline]
    fn description(&self) -> &str {
        match *self {
            RenderPassCreationError::OomError(_) => "not enough memory available",
            RenderPassCreationError::ColorAttachmentsLimitExceeded => {
                "the maximum number of color attachments has been exceeded"
            },
        }
    }

    #[inline]
    fn cause(&self) -> Option<&error::Error> {
        match *self {
            RenderPassCreationError::OomError(ref err) => Some(err),
            _ => None
        }
    }
}

impl fmt::Display for RenderPassCreationError {
    #[inline]
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        write!(fmt, "{}", error::Error::description(self))
    }
}

impl From<OomError> for RenderPassCreationError {
    #[inline]
    fn from(err: OomError) -> RenderPassCreationError {
        RenderPassCreationError::OomError(err)
    }
}

impl From<Error> for RenderPassCreationError {
    #[inline]
    fn from(err: Error) -> RenderPassCreationError {
        match err {
            err @ Error::OutOfHostMemory => {
                RenderPassCreationError::OomError(OomError::from(err))
            },
            err @ Error::OutOfDeviceMemory => {
                RenderPassCreationError::OomError(OomError::from(err))
            },
            _ => panic!("unexpected error: {:?}", err)
        }
    }
}

#[cfg(test)]
mod tests {
    // TODO:
}
