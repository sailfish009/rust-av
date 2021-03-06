use std::fs::File;
use std::net::TcpStream;
use std::io::{Read,Write,Seek,SeekFrom};
use std::{mem, slice};
use std::os::raw::{self, c_void, c_int};
use ffi;
use util::PtrTakeExt;

pub trait AVSeek: Sized + Send + 'static {
    /// Seek to `pos`. Returns `Some(new_pos)` on success
    /// and `None` on error.
    fn seek(&mut self, pos: SeekFrom) -> Option<u64>;
    /// The size of the data. It is optional to support this.
    fn size(&self) -> Option<u64> {
        None
    }
}

/// Implementors of AVRead can be used as custom input source.
pub trait AVRead: AVSeek + Sized + Send + 'static {
    /// Fill the buffer.
    /// Returns the number of bytes read.
    /// `None` or `Some(0)` indicates **EOF**.
    fn read_packet(&mut self, buf: &mut [u8]) -> Option<usize>;
    /// The buffer size is very important for performance.
    /// For protocols with fixed blocksize it should be set to this blocksize.
    /// For others a typical size is a cache page, e.g. 4kb.
    ///
    /// Default: 4kb.
    fn buffer_size() -> c_int { 4 * 1024 }
}

/// Implementors of AVWrite can be used as custom output source.
pub trait AVWrite: AVSeek + Sized + Send + 'static {
    /// Write the buffer to the output.
    /// Returns the number of bytes written.
    /// `None` or `Some(0)` indicates failure.
    fn write_packet(&mut self, buf: &[u8]) -> Option<usize>;
    /// The buffer size is very important for performance.
    /// For protocols with fixed blocksize it should be set to this blocksize.
    /// For others a typical size is a cache page, e.g. 4kb.
    ///
    /// Default: 4kb.
    fn buffer_size() -> c_int { 4 * 1024 }
}

#[doc(hidden)]
pub struct IOContext {
    ptr: *mut ffi::AVIOContext,
    io_dropper: IODropper,
}

impl IOContext {
    pub fn as_mut_ptr(&mut self) -> *mut ffi::AVIOContext {
        self.ptr
    }

    pub fn from_reader<R: AVRead>(mut input: R) -> IOContext  {
        unsafe {
            let buffer_size = R::buffer_size();
            let buffer = ffi::av_malloc(buffer_size as usize * mem::size_of::<u8>()) as _;
            let write_flag = 0; // Make buffer read-only for ffmpeg
            let read_packet = Some(ffi_read_packet::<R> as _);
            let write_packet = None;
            let seek = input.seek(SeekFrom::Current(0)).map(|_| ffi_seek::<R> as _);
            let this = Box::into_raw(Box::new(input)) as *mut c_void;
            let avio_ctx = ffi::avio_alloc_context(
                buffer,
                buffer_size,
                write_flag,
                this,
                read_packet,
                write_packet,
                seek
            );

            assert!(!avio_ctx.is_null(), "Could not allocate AVIO context");

            IOContext {
                ptr: avio_ctx,
                io_dropper: io_dropper::<R>,
            }
        }
    }

    pub fn from_writer<W: AVWrite>(mut output: W) -> IOContext  {
        unsafe {
            let buffer_size = W::buffer_size();
            let buffer = ffi::av_malloc(buffer_size as usize * mem::size_of::<u8>()) as _;
            let write_flag = 1; // Make buffer writable for ffmpeg
            let read_packet = None;
            let write_packet = Some(ffi_write_packet::<W> as _);
            let seek = output.seek(SeekFrom::Current(0)).map(|_| ffi_seek::<W> as _);;
            let this = Box::into_raw(Box::new(output)) as *mut c_void;
            let avio_ctx = ffi::avio_alloc_context(
                buffer,
                buffer_size,
                write_flag,
                this,
                read_packet,
                write_packet,
                seek
            );

            assert!(!avio_ctx.is_null(), "Could not allocate AVIO context");

            IOContext {
                ptr: avio_ctx,
                io_dropper: io_dropper::<W>,
            }
        }
    }

    // pub unsafe fn close_with<F: FnMut()>(self, mut closer: F) {
    //     let io = (*self.ptr).opaque;
    //     closer();
    //     (self.io_dropper)(io);
    // }
}

impl Drop for IOContext {
    fn drop(&mut self) {
        unsafe {
            if !self.ptr.is_null() {
                let this = &mut (*self.ptr);
                (self.io_dropper)(this.opaque.take());
                this.read_packet.take();
                this.write_packet.take();
                this.seek.take();
                ffi::av_free(this.buffer.take() as _);
            }
            ffi::av_free(self.ptr.take() as _);
        }
    }
}

type IODropper = unsafe fn(*mut c_void);

unsafe fn io_dropper<T>(io: *mut c_void) {
    Box::from_raw(io as *mut T);
}

extern fn ffi_read_packet<R: AVRead>(this: *mut c_void, buf: *mut u8, buf_size: c_int) -> c_int {
    let this = unsafe { &mut *(this as *mut R) };
    let buf = unsafe { slice::from_raw_parts_mut(buf, buf_size as usize) };
    let eof = -1;
    this.read_packet(buf).map(|n_read| n_read as c_int).unwrap_or(eof)
}

extern fn ffi_write_packet<W: AVWrite>(this: *mut c_void, buf: *mut u8, buf_size: c_int) -> c_int {
    let this = unsafe { &mut *(this as *mut W) };
    let buf = unsafe { slice::from_raw_parts(buf as *const _, buf_size as usize) };
    let eof = -1;
    this.write_packet(buf).map(|n_written| n_written as c_int).unwrap_or(eof)
}

unsafe extern fn ffi_seek<S: AVSeek>(this: *mut c_void, offset: i64, whence: c_int) -> i64 {
    let this = &mut *(this as *mut S);

    if whence == ffi::AVSEEK_SIZE as c_int {
        return this.size().and_then(u64_into_i64).unwrap_or(-1);
    }

    let pos = match whence as u32 {
        ffi::SEEK_SET => match i64_into_u64(offset) {
            Some(offset) => SeekFrom::Start(offset),
            None => return -1,
        },
        ffi::SEEK_CUR => SeekFrom::Current(offset),
        ffi::SEEK_END => SeekFrom::End(offset),
        _ => return -1,
    };

    this.seek(pos).and_then(u64_into_i64).unwrap_or(-1)
}

fn u64_into_i64(n: u64) -> Option<i64> {
    if n <= i64::max_value() as u64 {
        Some(n as i64)
    } else {
        None
    }
}

fn i64_into_u64(n: i64) -> Option<u64> {
    if n >= 0 {
        Some(n as u64)
    } else {
        None
    }
}

impl AVSeek for File {
    fn seek(&mut self, pos: SeekFrom) -> Option<u64> {
        Seek::seek(self, pos).ok()
    }
    fn size(&self) -> Option<u64> {
        self.metadata().map(|m| m.len()).ok()
    }
}

impl AVRead for File {
    fn read_packet(&mut self, buf: &mut [u8]) -> Option<usize> {
        self.read(buf).ok()
    }
}

impl AVWrite for File {
    fn write_packet(&mut self, buf: &[u8]) -> Option<usize> {
        self.write(buf).ok()
    }
}

impl AVSeek for TcpStream {
    fn seek(&mut self, _pos: SeekFrom) -> Option<u64> {
        None
    }
    fn size(&self) -> Option<u64> {
        None
    }
}

impl AVRead for TcpStream {
    fn read_packet(&mut self, buf: &mut [u8]) -> Option<usize> {
        self.read(buf).ok()
    }
}

impl AVWrite for TcpStream {
    fn write_packet(&mut self, buf: &[u8]) -> Option<usize> {
        self.write(buf).ok()
    }
}
