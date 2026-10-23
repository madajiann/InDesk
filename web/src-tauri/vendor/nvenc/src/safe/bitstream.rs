use std::{ffi::c_void, sync::Arc};

use crate::{safe::encoder::EncoderInternal, sys::result::NVencError};

pub struct BitStream {
    pub(crate) buffer: *mut c_void,
    pub(crate) encoder: Arc<EncoderInternal>,
}

impl Drop for BitStream {
    fn drop(&mut self) {
        // [InDesk 加固] 去掉 println!（每帧回收缓冲都会刷屏）；
        // unwrap 改忽略 —— 析构路径上 panic 会直接炸掉调用方。
        let _ = self.encoder.destroy_bitstream_buffer(self.buffer);
    }
}

unsafe impl Send for BitStream {}

impl BitStream {
    /// Attempts to lock the bit stream, if `wait` is true it will wait
    /// otherwise a `LockBusy` Error may be returned, in which case the
    /// client should retry in a few milliseconds
    pub fn try_lock(&self, wait: bool) -> Result<BitStreamLockGuard<'_>, NVencError> {
        let lock = self.encoder.lock_bit_stream_buffer(self.buffer, wait)?;
        Ok(BitStreamLockGuard {
            buffer: self,
            data_ptr: lock.bitstream_buffer,
            data_len: lock.bitstream_size_in_bytes,
        })
    }
}

/// Holds a reference to the `BitStream` and holds the data and associated fields
pub struct BitStreamLockGuard<'a> {
    buffer: &'a BitStream,
    data_ptr: *mut c_void,
    data_len: u32,
}

impl BitStreamLockGuard<'_> {
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.data_ptr as _, self.data_len as _) }
    }
}

impl<'a> Drop for BitStreamLockGuard<'a> {
    fn drop(&mut self) {
        let _ = self
            .buffer
            .encoder
            .unlock_bit_stream_buffer(self.buffer.buffer);
    }
}
