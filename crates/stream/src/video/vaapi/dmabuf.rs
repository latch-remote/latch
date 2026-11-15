//! Exported, read-only VA surfaces. File descriptors and a surface lease stay
//! alive until the renderer's submitted work has finished.
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub(super) static ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable export only after a renderer finds a DMA-BUF import path. A failed
/// export or import turns it off and subsequent pictures use readback.
pub fn render_on_dmabuf(enabled: bool) {
    ENABLED.store(enabled, Ordering::Release);
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct Object {
    pub fd: i32,
    pub size: u32,
    pub modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct Layer {
    pub format: u32,
    pub num_planes: u32,
    pub object_index: [u32; 4],
    pub offset: [u32; 4],
    pub pitch: [u32; 4],
}

#[repr(C)]
#[derive(Default)]
pub(super) struct Descriptor {
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub num_objects: u32,
    pub objects: [Object; 4],
    pub num_layers: u32,
    pub layers: [Layer; 4],
}

#[derive(Clone, Copy, Debug)]
pub struct Plane {
    pub fd: i32,
    pub format: u32,
    pub offset: u32,
    pub pitch: u32,
    pub modifier: u64,
}

#[derive(Debug)]
struct Export {
    _fds: Vec<OwnedFd>,
    planes: [Plane; 2],
    _lease: Arc<()>,
}

#[derive(Clone, Debug)]
pub struct NativeFrame(Arc<Export>);

impl NativeFrame {
    pub fn planes(&self) -> &[Plane; 2] {
        &self.0.planes
    }

    /// Own every returned fd before validating the descriptor, so unsupported
    /// layouts also release the exported handles.
    pub(super) fn new(desc: Descriptor, lease: Arc<()>) -> anyhow::Result<Self> {
        let fds: Vec<_> = desc
            .objects
            .iter()
            .take(desc.num_objects as usize)
            .filter(|o| o.fd >= 0)
            .map(|o| {
                // SAFETY: a successful vaExportSurfaceHandle transfers each fd.
                unsafe { OwnedFd::from_raw_fd(o.fd) }
            })
            .collect();
        anyhow::ensure!(
            desc.num_objects <= 4 && fds.len() == desc.num_objects as usize && desc.num_layers == 2,
            "unsupported VA-API DMA-BUF layout"
        );
        let mut planes = Vec::new();
        for layer in desc.layers.iter().take(2) {
            let i = layer.object_index[0] as usize;
            anyhow::ensure!(
                layer.num_planes == 1 && i < fds.len() && layer.pitch[0] > 0,
                "unsupported VA-API DMA-BUF plane"
            );
            planes.push(Plane {
                fd: fds[i].as_raw_fd(),
                format: layer.format,
                offset: layer.offset[0],
                pitch: layer.pitch[0],
                modifier: desc.objects[i].modifier,
            });
        }
        Ok(Self(Arc::new(Export {
            _fds: fds,
            planes: [planes[0], planes[1]],
            _lease: lease,
        })))
    }
}

const _: () = {
    assert!(std::mem::size_of::<Descriptor>() == 312);
    assert!(std::mem::size_of::<Layer>() == 56);
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;

    #[test]
    fn exported_handles_keep_the_surface_leased_until_the_last_clone_is_gone() {
        let lease = Arc::new(());
        let file = std::fs::File::open("/dev/null").unwrap();
        let fd = file.into_raw_fd();
        let mut descriptor = Descriptor {
            num_objects: 1,
            num_layers: 2,
            ..Default::default()
        };
        descriptor.objects[0] = Object {
            fd,
            size: 4096,
            modifier: 0,
        };
        descriptor.layers[0] = Layer {
            format: u32::from_le_bytes(*b"R8  "),
            num_planes: 1,
            pitch: [128, 0, 0, 0],
            ..Default::default()
        };
        descriptor.layers[1] = Layer {
            format: u32::from_le_bytes(*b"GR88"),
            num_planes: 1,
            offset: [2048, 0, 0, 0],
            pitch: [128, 0, 0, 0],
            ..Default::default()
        };
        let native = NativeFrame::new(descriptor, lease.clone()).unwrap();
        let retained = native.clone();
        assert_eq!(Arc::strong_count(&lease), 2);
        assert_eq!(retained.planes()[1].offset, 2048);
        drop(native);
        assert_eq!(Arc::strong_count(&lease), 2);
        drop(retained);
        assert_eq!(Arc::strong_count(&lease), 1);
    }
}
