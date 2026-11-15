//! DMA-BUF plane imports on wgpu's EGL/GLES backend. Vulkan and unsupported
//! EGL drivers use VA-API readback into the existing NV12/P010 upload path.
use super::{texture_formats, Frame, PixelFormat};
use egui_wgpu::wgpu::{self, hal};
use glow::HasContext;
use latch_stream::video::DmaBufPlane as Plane;
use std::ffi::c_void;

const EXTENSIONS: i32 = 0x3055;
const NONE: i32 = 0x3038;
const DMA_BUF: u32 = 0x3270;
const INVALID_MODIFIER: u64 = 0x00ff_ffff_ffff_ffff;

type CreateImage =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type DestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type BindImage = unsafe extern "C" fn(u32, *mut c_void);

fn attributes(plane: Plane, width: u32, height: u32, modifiers: bool) -> Result<Vec<i32>, String> {
    if !modifiers && !matches!(plane.modifier, 0 | INVALID_MODIFIER) {
        return Err("EGL cannot import this DRM modifier".into());
    }
    if width == 0
        || height == 0
        || width > i32::MAX as u32
        || height > i32::MAX as u32
        || plane.offset > i32::MAX as u32
        || plane.pitch > i32::MAX as u32
    {
        return Err("DMA-BUF plane dimensions exceed EGL limits".into());
    }
    let mut attrs = vec![
        0x3057,
        width as i32,
        0x3056,
        height as i32, // EGL_WIDTH / HEIGHT
        0x3271,
        plane.format as i32, // EGL_LINUX_DRM_FOURCC_EXT
        0x3272,
        plane.fd,
        0x3273,
        plane.offset as i32,
        0x3274,
        plane.pitch as i32,
    ];
    if modifiers && plane.modifier != INVALID_MODIFIER {
        attrs.extend([
            0x3443,
            plane.modifier as i32,
            0x3444,
            (plane.modifier >> 32) as i32,
        ]);
    }
    attrs.push(NONE);
    Ok(attrs)
}

pub(super) fn supported(device: &wgpu::Device) -> bool {
    // SAFETY: only inspect the device's own EGL extension string and symbols.
    unsafe {
        device.as_hal::<hal::api::Gles, _, _>(|d| {
            let Some(d) = d else {
                return false;
            };
            let context = d.context();
            let (Some(egl), Some(display)) = (context.egl_instance(), context.raw_display()) else {
                return false;
            };
            egl.query_string(Some(*display), EXTENSIONS).is_ok_and(|s| {
                s.to_string_lossy()
                    .split_whitespace()
                    .any(|e| e == "EGL_EXT_image_dma_buf_import")
            }) && [
                "eglCreateImageKHR",
                "eglDestroyImageKHR",
                "glEGLImageTargetTexture2DOES",
            ]
            .iter()
            .all(|name| egl.get_proc_address(name).is_some())
        })
    }
}

pub(super) fn import(device: &wgpu::Device, frame: &Frame) -> Result<[wgpu::Texture; 2], String> {
    let native = frame.native.as_ref().ok_or("no DMA-BUF surface")?;
    let (yf, uvf) = texture_formats(frame.format);
    let expected = if frame.format == PixelFormat::P010 {
        [u32::from_le_bytes(*b"R16 "), u32::from_le_bytes(*b"GR32")]
    } else {
        [u32::from_le_bytes(*b"R8  "), u32::from_le_bytes(*b"GR88")]
    };
    let mut textures = Vec::new();
    for (i, plane) in native.planes().iter().copied().enumerate() {
        if plane.format != expected[i] {
            return Err(format!(
                "unsupported DMA-BUF plane format {:#x}",
                plane.format
            ));
        }
        let size = wgpu::Extent3d {
            width: if i == 0 {
                frame.width
            } else {
                frame.width.div_ceil(2)
            },
            height: if i == 0 {
                frame.height
            } else {
                frame.height.div_ceil(2)
            },
            depth_or_array_layers: 1,
        };
        let format = if i == 0 { yf } else { uvf };
        // SAFETY: the VA surface has completed its writes. The decoder's
        // lease prevents reuse until the renderer releases this frame after
        // submitted work completes. Only the matching EGL context is used.
        let raw = unsafe {
            device.as_hal::<hal::api::Gles, _, _>(|d| -> Result<_, String> {
                let d = d.ok_or("the window does not draw with EGL/GLES")?;
                let context = d.context();
                let egl = context.egl_instance().ok_or("no EGL instance")?;
                let display = *context.raw_display().ok_or("no EGL display")?;
                let extensions = egl
                    .query_string(Some(display), EXTENSIONS)
                    .map_err(|e| e.to_string())?;
                let modifiers = extensions
                    .to_string_lossy()
                    .split_whitespace()
                    .any(|e| e == "EGL_EXT_image_dma_buf_import_modifiers");
                let attrs = attributes(plane, size.width, size.height, modifiers)?;
                let create: CreateImage = std::mem::transmute(
                    egl.get_proc_address("eglCreateImageKHR")
                        .ok_or("no eglCreateImageKHR")?,
                );
                let destroy: DestroyImage = std::mem::transmute(
                    egl.get_proc_address("eglDestroyImageKHR")
                        .ok_or("no eglDestroyImageKHR")?,
                );
                let bind: BindImage = std::mem::transmute(
                    egl.get_proc_address("glEGLImageTargetTexture2DOES")
                        .ok_or("no glEGLImageTargetTexture2DOES")?,
                );
                let gl = context.lock();
                let image = create(
                    display.as_ptr(),
                    std::ptr::null_mut(),
                    DMA_BUF,
                    std::ptr::null_mut(),
                    attrs.as_ptr(),
                );
                if image.is_null() {
                    return Err("EGL refused the DMA-BUF plane".into());
                }
                let texture = match gl.create_texture() {
                    Ok(texture) => texture,
                    Err(e) => {
                        destroy(display.as_ptr(), image);
                        return Err(e);
                    }
                };
                let previous = std::num::NonZeroU32::new(
                    gl.get_parameter_i32(glow::TEXTURE_BINDING_2D) as u32,
                )
                .map(glow::NativeTexture);
                gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                bind(glow::TEXTURE_2D, image);
                let error = gl.get_error();
                gl.bind_texture(glow::TEXTURE_2D, previous);
                destroy(display.as_ptr(), image);
                if error != glow::NO_ERROR {
                    gl.delete_texture(texture);
                    return Err(format!("GL refused the EGL image ({error:#x})"));
                }
                Ok(d.texture_from_raw(
                    texture.0,
                    &hal::TextureDescriptor {
                        label: Some("latch DMA-BUF plane"),
                        size,
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format,
                        usage: hal::TextureUses::RESOURCE,
                        memory_flags: hal::MemoryFlags::empty(),
                        view_formats: vec![],
                    },
                    None,
                ))
            })
        }?;
        // SAFETY: the imported GL texture is a single plane of this exact
        // format and extent, and wgpu owns its GL name from here on.
        textures.push(unsafe {
            device.create_texture_from_hal::<hal::api::Gles>(
                raw,
                &wgpu::TextureDescriptor {
                    label: Some("latch DMA-BUF plane"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
            )
        });
    }
    Ok([textures.remove(0), textures.remove(0)])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dma_buf_attributes_preserve_modifiers_and_reject_unsupported_layouts() {
        let plane = Plane {
            fd: 12,
            format: u32::from_le_bytes(*b"R8  "),
            offset: 4096,
            pitch: 256,
            modifier: 0x0100_0000_0000_0002,
        };
        assert!(attributes(plane, 128, 64, false).is_err());
        let attrs = attributes(plane, 128, 64, true).unwrap();
        assert_eq!(&attrs[12..], &[0x3443, 2, 0x3444, 0x0100_0000, NONE]);
        assert_eq!(
            attributes(
                Plane {
                    modifier: INVALID_MODIFIER,
                    ..plane
                },
                128,
                64,
                false
            )
            .unwrap()
            .last(),
            Some(&NONE)
        );
        assert!(attributes(
            Plane {
                pitch: u32::MAX,
                ..plane
            },
            128,
            64,
            true
        )
        .is_err());
    }
}
