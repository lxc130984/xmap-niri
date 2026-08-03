//! Shared-memory buffer pool for the layer-surface frames.
//!
//! The pool keeps at most two buffers (double buffering: one being presented
//! by the compositor, one being drawn) and reuses them across frames, so a
//! steady-state redraw never allocates. Buffers that end up much larger than
//! the current widget size are destroyed on release so a shrunken minimap
//! gives its memory back.

use std::fs::File;
use std::os::fd::{AsFd, FromRawFd};

use anyhow::{bail, Result};
use log::{debug, warn};
use memmap2::MmapMut;
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_shm::{Format, WlShm},
    wl_shm_pool::WlShmPool,
};
use wayland_client::{QueueHandle, Proxy};

use crate::app::App;

/// Maximum number of shm buffers kept around (double buffering).
const MAX_BUFFERS: usize = 2;

struct ShmBuffer {
    id: u64,
    _file: File,
    mmap: MmapMut,
    pool: WlShmPool,
    buffer: WlBuffer,
    w: u32,
    h: u32,
    in_use: bool,
}

impl ShmBuffer {
    fn create(
        w: u32,
        h: u32,
        shm: &WlShm,
        qh: &QueueHandle<App>,
        id: u64,
    ) -> Result<Self> {
        let size = (w as usize) * (h as usize) * 4;
        let file = make_memfd()?;
        file.set_len(size as u64)?;
        let mmap = unsafe { MmapMut::map_mut(&file) }?;
        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            w as i32,
            h as i32,
            (w * 4) as i32,
            Format::Argb8888,
            qh,
            (),
        );
        Ok(ShmBuffer {
            id,
            _file: file,
            mmap,
            pool,
            buffer,
            w,
            h,
            in_use: false,
        })
    }
}

/// Owns the shm buffers attached to the layer surface.
pub struct BufferPool {
    shm: WlShm,
    qh: QueueHandle<App>,
    buffers: Vec<ShmBuffer>,
    next_id: u64,
}

impl BufferPool {
    pub fn new(shm: WlShm, qh: QueueHandle<App>) -> Self {
        BufferPool {
            shm,
            qh,
            buffers: Vec::new(),
            next_id: 1,
        }
    }

    /// Reserve a buffer of the given physical size, recycling a free one or
    /// creating a new slot when below [`MAX_BUFFERS`]. Returns its index.
    pub fn acquire(&mut self, w: u32, h: u32) -> Option<usize> {
        if let Some(i) = self
            .buffers
            .iter()
            .position(|b| !b.in_use && b.w == w && b.h == h)
        {
            self.buffers[i].in_use = true;
            return Some(i);
        }
        if let Some(i) = self.buffers.iter().position(|b| !b.in_use) {
            let id = self.buffers[i].id;
            self.buffers[i].buffer.destroy();
            self.buffers[i].pool.destroy();
            match ShmBuffer::create(w, h, &self.shm, &self.qh, id) {
                Ok(nb) => {
                    self.buffers[i] = nb;
                    self.buffers[i].in_use = true;
                    Some(i)
                }
                Err(err) => {
                    warn!("failed to resize shm buffer: {err:#}");
                    None
                }
            }
        } else if self.buffers.len() < MAX_BUFFERS {
            let id = self.next_id;
            self.next_id += 1;
            match ShmBuffer::create(w, h, &self.shm, &self.qh, id) {
                Ok(nb) => {
                    self.buffers.push(nb);
                    let i = self.buffers.len() - 1;
                    self.buffers[i].in_use = true;
                    Some(i)
                }
                Err(err) => {
                    warn!("failed to allocate shm buffer: {err:#}");
                    None
                }
            }
        } else {
            None
        }
    }

    /// Copy rendered pixels into a reserved buffer.
    pub fn copy_pixels(&mut self, i: usize, pixels: &[u8]) {
        let buf = &mut self.buffers[i];
        let dst = &mut buf.mmap[..pixels.len()];
        dst.copy_from_slice(pixels);
    }

    /// Copy a rectangular region of rendered pixels into a reserved buffer.
    ///
    /// `pixels` holds one full frame of `buf_w`-wide BGRA rows; only the
    /// region `(x, y)` of size `(rw, rh)` is written, so unchanged areas of
    /// the shared buffer keep their previous content and can be omitted from
    /// the surface damage.
    #[allow(clippy::too_many_arguments)]
    pub fn copy_region(
        &mut self,
        i: usize,
        pixels: &[u8],
        buf_w: u32,
        x: u32,
        y: u32,
        rw: u32,
        rh: u32,
    ) {
        let Some(buf) = self.buffers.get_mut(i) else {
            return;
        };
        let stride = buf_w as usize * 4;
        for row in 0..rh as usize {
            let src = (y as usize + row) * stride + x as usize * 4;
            let dst = (y as usize + row) * (buf.w as usize * 4) + x as usize * 4;
            let len = rw as usize * 4;
            buf.mmap[dst..dst + len].copy_from_slice(&pixels[src..src + len]);
        }
    }

    /// The `wl_buffer` to attach for a reserved slot.
    pub fn buffer(&self, i: usize) -> WlBuffer {
        self.buffers[i].buffer.clone()
    }

    /// Mark a buffer released by the compositor, reclaiming it if it is much
    /// larger than the current widget size.
    pub fn release(&mut self, proxy_id: u32, needed: (u32, u32)) {
        if let Some(b) = self
            .buffers
            .iter_mut()
            .find(|b| b.buffer.id().protocol_id() == proxy_id)
        {
            b.in_use = false;
        }
        self.reclaim_oversized(needed);
    }

    /// Destroy free buffers much larger than the current widget size so a
    /// shrunken minimap gives its memory back instead of hoarding it.
    pub fn reclaim_oversized(&mut self, needed: (u32, u32)) {
        let (nw, nh) = needed;
        let needed_area = (nw as u64) * (nh as u64);
        if needed_area == 0 {
            return;
        }
        let mut i = 0;
        while i < self.buffers.len() {
            let keep = {
                let b = &self.buffers[i];
                b.in_use || (b.w as u64) * (b.h as u64) <= needed_area * 2
            };
            if keep {
                i += 1;
            } else {
                let b = self.buffers.remove(i);
                b.buffer.destroy();
                b.pool.destroy();
                debug!("reclaimed oversized shm buffer ({}x{})", b.w, b.h);
            }
        }
    }
}

fn make_memfd() -> Result<File> {
    let name = c"nirimap-shm";
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("memfd_create failed: {}", std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
