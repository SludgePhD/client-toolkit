//! A raw shared memory pool handler.
//!
//! This is intended as a safe building block for higher level shared memory pool abstractions and is not
//! encouraged for most library users.

use std::{
    fs::File,
    io,
    os::unix::prelude::{FromRawFd, RawFd},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use memmap2::MmapMut;
use nix::{
    errno::Errno,
    fcntl,
    sys::{mman, stat},
    unistd,
};
use wayland_client::{
    backend::{InvalidId, ObjectData},
    protocol::{wl_buffer, wl_shm, wl_shm_pool},
    Dispatch, Proxy, QueueHandle, WEnum,
};

use crate::{error::GlobalError, globals::ProvidesBoundGlobal};

use super::CreatePoolError;

/// A raw handler for file backed shared memory pools.
///
/// This type of pool will create the SHM memory pool and provide a way to resize the pool.
///
/// This pool does not release buffers. If you need this, use one of the higher level pools.
#[derive(Debug)]
pub struct RawPool {
    pool: wl_shm_pool::WlShmPool,
    len: usize,
    mem_file: File,
    mmap: MmapMut,
}

impl RawPool {
    pub fn new(
        len: usize,
        shm: &impl ProvidesBoundGlobal<wl_shm::WlShm, 1>,
    ) -> Result<RawPool, CreatePoolError> {
        let shm = shm.bound_global()?;
        let shm_fd = RawPool::create_shm_fd()?;
        let mem_file = unsafe { File::from_raw_fd(shm_fd) };
        mem_file.set_len(len as u64)?;

        let pool = shm
            .send_constructor(
                wl_shm::Request::CreatePool { fd: shm_fd, size: len as i32 },
                Arc::new(ShmPoolData),
            )
            .map_err(GlobalError::from)?;
        let mmap = unsafe { MmapMut::map_mut(&mem_file)? };

        Ok(RawPool { pool, len, mem_file, mmap })
    }

    /// Resizes the memory pool, notifying the server the pool has changed in size.
    ///
    /// The wl_shm protocol only allows the pool to be made bigger. If the new size is smaller than the
    /// current size of the pool, this function will do nothing.
    pub fn resize(&mut self, size: usize) -> io::Result<()> {
        if size > self.len {
            self.len = size;
            self.mem_file.set_len(size as u64)?;
            self.pool.resize(size as i32);
            self.mmap = unsafe { MmapMut::map_mut(&self.mem_file) }?;
        }

        Ok(())
    }

    /// Returns a reference to the underlying shared memory file using the memmap2 crate.
    pub fn mmap(&mut self) -> &mut MmapMut {
        &mut self.mmap
    }

    /// Returns the size of the mempool
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Create a new buffer to this pool.
    ///
    /// ## Parameters
    /// - `offset`: the offset (in bytes) from the beginning of the pool at which this buffer starts.
    /// - `width` and `height`: the width and height of the buffer in pixels.
    /// - `stride`: distance (in bytes) between the beginning of a row and the next one.
    /// - `format`: the encoding format of the pixels.
    ///
    /// The encoding format of the pixels must be supported by the compositor or else a protocol error is
    /// risen. You can ensure the format is supported by listening to [`ShmState::formats`](crate::shm::ShmState::formats).
    ///
    /// Note this function only creates the wl_buffer object, you will need to write to the pixels using the
    /// [`io::Write`] implementation or [`RawPool::mmap`].
    #[allow(clippy::too_many_arguments)]
    pub fn create_buffer<D, U>(
        &mut self,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: wl_shm::Format,
        udata: U,
        qh: &QueueHandle<D>,
    ) -> Result<wl_buffer::WlBuffer, InvalidId>
    where
        D: Dispatch<wl_buffer::WlBuffer, U> + 'static,
        U: Send + Sync + 'static,
    {
        let buffer = self.pool.create_buffer(offset, width, height, stride, format, qh, udata)?;

        Ok(buffer)
    }

    /// Create a new buffer to this pool.
    ///
    /// This is identical to [Self::create_buffer], but allows using a custom [ObjectData]
    /// implementation instead of relying on the [Dispatch] interface.
    #[allow(clippy::too_many_arguments)]
    pub fn create_buffer_raw(
        &mut self,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: wl_shm::Format,
        data: Arc<dyn ObjectData + 'static>,
    ) -> Result<wl_buffer::WlBuffer, InvalidId> {
        self.pool.send_constructor(
            wl_shm_pool::Request::CreateBuffer {
                offset,
                width,
                height,
                stride,
                format: WEnum::Value(format),
            },
            data,
        )
    }

    /// Returns the pool object used to communicate with the server.
    pub fn pool(&self) -> &wl_shm_pool::WlShmPool {
        &self.pool
    }
}

impl io::Write for RawPool {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut self.mem_file, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.mem_file)
    }
}

impl io::Seek for RawPool {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        io::Seek::seek(&mut self.mem_file, pos)
    }
}

impl RawPool {
    fn create_shm_fd() -> io::Result<RawFd> {
        #[cfg(target_os = "linux")]
        {
            match RawPool::create_memfd() {
                Ok(fd) => return Ok(fd),

                // Not supported, use fallback.
                Err(Errno::ENOSYS) => (),

                Err(err) => return Err(Into::<io::Error>::into(err)),
            };
        }

        let time = SystemTime::now();
        let mut mem_file_handle = format!(
            "/smithay-client-toolkit-{}",
            time.duration_since(UNIX_EPOCH).unwrap().subsec_nanos()
        );

        loop {
            let flags = fcntl::OFlag::O_CREAT
                | fcntl::OFlag::O_EXCL
                | fcntl::OFlag::O_RDWR
                | fcntl::OFlag::O_CLOEXEC;

            let mode = stat::Mode::S_IRUSR | stat::Mode::S_IWUSR;

            match mman::shm_open(mem_file_handle.as_str(), flags, mode) {
                Ok(fd) => match mman::shm_unlink(mem_file_handle.as_str()) {
                    Ok(_) => return Ok(fd),

                    Err(errno) => {
                        unistd::close(fd)?;

                        return Err(errno.into());
                    }
                },

                Err(Errno::EEXIST) => {
                    // Change the handle if we happen to be duplicate.
                    let time = SystemTime::now();

                    mem_file_handle = format!(
                        "/smithay-client-toolkit-{}",
                        time.duration_since(UNIX_EPOCH).unwrap().subsec_nanos()
                    );

                    continue;
                }

                Err(Errno::EINTR) => continue,

                Err(err) => return Err(err.into()),
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn create_memfd() -> nix::Result<RawFd> {
        use std::ffi::CStr;

        use nix::{
            fcntl::{FcntlArg, SealFlag},
            sys::memfd::{self, MemFdCreateFlag},
        };

        loop {
            let name = CStr::from_bytes_with_nul(b"smithay-client-toolkit\0").unwrap();
            let flags = MemFdCreateFlag::MFD_ALLOW_SEALING | MemFdCreateFlag::MFD_CLOEXEC;

            match memfd::memfd_create(name, flags) {
                Ok(fd) => {
                    let arg =
                        FcntlArg::F_ADD_SEALS(SealFlag::F_SEAL_SHRINK | SealFlag::F_SEAL_SEAL);

                    // We only need to seal for the purposes of optimization, ignore the errors.
                    let _ = fcntl::fcntl(fd, arg);

                    return Ok(fd);
                }

                Err(Errno::EINTR) => continue,

                Err(err) => return Err(err),
            }
        }
    }
}

impl Drop for RawPool {
    fn drop(&mut self) {
        self.pool.destroy();
    }
}

#[derive(Debug)]
struct ShmPoolData;

impl ObjectData for ShmPoolData {
    fn event(
        self: Arc<Self>,
        _: &wayland_client::backend::Backend,
        _: wayland_client::backend::protocol::Message<wayland_client::backend::ObjectId>,
    ) -> Option<Arc<(dyn ObjectData + 'static)>> {
        unreachable!("wl_shm_pool has no events")
    }

    fn destroyed(&self, _: wayland_client::backend::ObjectId) {}
}
