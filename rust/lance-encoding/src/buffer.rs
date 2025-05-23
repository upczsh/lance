// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Utilities for byte arrays

use std::{ops::Deref, panic::RefUnwindSafe, ptr::NonNull, sync::Arc};

use arrow_buffer::{ArrowNativeType, Buffer, MutableBuffer, ScalarBuffer};
use itertools::Either;
use snafu::location;

use lance_core::{utils::bit::is_pwr_two, Error, Result};

/// A copy-on-write byte buffer
///
/// It can be created from read-only buffers (e.g. bytes::Bytes or arrow_buffer::Buffer), e.g. "borrowed"
/// or from writeable buffers (e.g. Vec<u8>), e.g. "owned"
///
/// The buffer can switch to borrowed mode without a copy of the data
///
/// LanceBuffer does not implement Clone because doing could potentially silently trigger a copy of the data
/// and we want to make sure that the user is aware of this operation.
///
/// If you need to clone a LanceBuffer you can use borrow_and_clone() which will make sure that the buffer
/// is in borrowed mode before cloning.  This is a zero copy operation (but requires &mut self).
pub enum LanceBuffer {
    Borrowed(Buffer),
    Owned(Vec<u8>),
}

// Compares equality of the buffers, ignoring owned / unowned status
impl PartialEq for LanceBuffer {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Borrowed(l0), Self::Borrowed(r0)) => l0 == r0,
            (Self::Owned(l0), Self::Owned(r0)) => l0 == r0,
            (Self::Borrowed(l0), Self::Owned(r0)) => l0.as_slice() == r0.as_slice(),
            (Self::Owned(l0), Self::Borrowed(r0)) => l0.as_slice() == r0.as_slice(),
        }
    }
}

impl Eq for LanceBuffer {}

impl std::fmt::Debug for LanceBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let preview = if self.len() > 10 {
            format!("0x{}...", hex::encode_upper(&self[..10]))
        } else {
            format!("0x{}", hex::encode_upper(self.as_ref()))
        };
        match self {
            Self::Borrowed(buffer) => write!(
                f,
                "LanceBuffer::Borrowed(bytes={} #bytes={})",
                preview,
                buffer.len()
            ),
            Self::Owned(buffer) => {
                write!(
                    f,
                    "LanceBuffer::Owned(bytes={} #bytes={})",
                    preview,
                    buffer.len()
                )
            }
        }
    }
}

impl LanceBuffer {
    /// Convert into a mutable buffer.  If this is a borrowed buffer, the data will be copied.
    pub fn into_owned(self) -> Vec<u8> {
        match self {
            Self::Borrowed(buffer) => buffer.to_vec(),
            Self::Owned(buffer) => buffer,
        }
    }

    /// Convert into an Arrow buffer.  Never copies data.
    pub fn into_buffer(self) -> Buffer {
        match self {
            Self::Borrowed(buffer) => buffer,
            Self::Owned(buffer) => Buffer::from_vec(buffer),
        }
    }

    /// Returns an owned buffer of the given size with all bits set to 0
    pub fn all_unset(len: usize) -> Self {
        Self::Owned(vec![0; len])
    }

    /// Returns an owned buffer of the given size with all bits set to 1
    pub fn all_set(len: usize) -> Self {
        Self::Owned(vec![0xff; len])
    }

    /// Creates an empty buffer
    pub fn empty() -> Self {
        Self::Owned(Vec::new())
    }

    /// Converts the buffer into a hex string
    pub fn as_hex(&self) -> String {
        hex::encode_upper(self)
    }

    /// Combine multiple buffers into a single buffer
    ///
    /// This does involve a data copy (and allocation of a new buffer)
    pub fn concat(buffers: &[Self]) -> Self {
        let total_len = buffers.iter().map(|b| b.len()).sum();
        let mut data = Vec::with_capacity(total_len);
        for buffer in buffers {
            data.extend_from_slice(buffer.as_ref());
        }
        Self::Owned(data)
    }

    /// Converts the buffer into a hex string, inserting a space
    /// between words
    pub fn as_spaced_hex(&self, bytes_per_word: u32) -> String {
        let hex = self.as_hex();
        let chars_per_word = bytes_per_word as usize * 2;
        let num_words = hex.len() / chars_per_word;
        let mut spaced_hex = String::with_capacity(hex.len() + num_words);
        for (i, c) in hex.chars().enumerate() {
            if i % chars_per_word == 0 && i != 0 {
                spaced_hex.push(' ');
            }
            spaced_hex.push(c);
        }
        spaced_hex
    }

    /// Create a LanceBuffer from a bytes::Bytes object
    ///
    /// The alignment must be specified (as `bytes_per_value`) since we want to make
    /// sure we can safely reinterpret the buffer.
    ///
    /// If the buffer is properly aligned this will be zero-copy.  If not, a copy
    /// will be made and an owned buffer returned.
    ///
    /// If `bytes_per_value` is not a power of two, then we assume the buffer is
    /// never going to be reinterpret into another type and we can safely
    /// ignore the alignment.
    pub fn from_bytes(bytes: bytes::Bytes, bytes_per_value: u64) -> Self {
        if is_pwr_two(bytes_per_value) && bytes.as_ptr().align_offset(bytes_per_value as usize) != 0
        {
            // The original buffer is not aligned, cannot zero-copy
            let mut buf = Vec::with_capacity(bytes.len());
            buf.extend_from_slice(&bytes);
            Self::Owned(buf)
        } else {
            // The original buffer is aligned, can zero-copy
            // SAFETY: the alignment is correct we can make this conversion
            unsafe {
                Self::Borrowed(Buffer::from_custom_allocation(
                    NonNull::new(bytes.as_ptr() as _).expect("should be a valid pointer"),
                    bytes.len(),
                    Arc::new(bytes),
                ))
            }
        }
    }

    /// Convert a buffer into a bytes::Bytes object
    pub fn into_bytes(self) -> bytes::Bytes {
        match self {
            Self::Owned(buf) => buf.into(),
            Self::Borrowed(buf) => buf.into_vec::<u8>().unwrap().into(),
        }
    }

    /// Convert into a borrowed buffer, this is a zero-copy operation
    ///
    /// This is often called before cloning the buffer
    pub fn into_borrowed(self) -> Self {
        match self {
            Self::Borrowed(_) => self,
            Self::Owned(buffer) => Self::Borrowed(Buffer::from_vec(buffer)),
        }
    }

    /// Creates an owned copy of the buffer, will always involve a full copy of the bytes
    pub fn to_owned(&self) -> Self {
        match self {
            Self::Borrowed(buffer) => Self::Owned(buffer.to_vec()),
            Self::Owned(buffer) => Self::Owned(buffer.clone()),
        }
    }

    /// Creates a clone of the buffer but also puts the buffer into borrowed mode
    ///
    /// This is a zero-copy operation
    pub fn borrow_and_clone(&mut self) -> Self {
        match self {
            Self::Borrowed(buffer) => Self::Borrowed(buffer.clone()),
            Self::Owned(buffer) => {
                let buf_data = std::mem::take(buffer);
                let buffer = Buffer::from_vec(buf_data);
                *self = Self::Borrowed(buffer.clone());
                Self::Borrowed(buffer)
            }
        }
    }

    /// Clones the buffer but fails if the buffer is in owned mode
    pub fn try_clone(&self) -> Result<Self> {
        match self {
            Self::Borrowed(buffer) => Ok(Self::Borrowed(buffer.clone())),
            Self::Owned(_) => Err(Error::Internal {
                message: "try_clone called on an owned buffer".to_string(),
                location: location!(),
            }),
        }
    }

    /// Make an owned copy of the buffer (always does a copy of the data)
    pub fn deep_copy(&self) -> Self {
        match self {
            Self::Borrowed(buffer) => Self::Owned(buffer.to_vec()),
            Self::Owned(buffer) => Self::Owned(buffer.clone()),
        }
    }

    /// Reinterprets a Vec<T> as a LanceBuffer
    ///
    /// Note that this creates a borrowed buffer.  It is not possible to safely
    /// reinterpret a Vec<T> into a Vec<u8> in rust due to this constraint from
    /// [`Vec::from_raw_parts`]:
    ///
    /// > `T` needs to have the same alignment as what `ptr` was allocated with.
    /// > (`T` having a less strict alignment is not sufficient, the alignment really
    /// > needs to be equal to satisfy the [`dealloc`] requirement that memory must be
    /// > allocated and deallocated with the same layout.)
    ///
    /// However, we can safely reinterpret Vec<T> into &[u8] which is what happens here.
    pub fn reinterpret_vec<T: ArrowNativeType>(vec: Vec<T>) -> Self {
        Self::Borrowed(Buffer::from_vec(vec))
    }

    /// Reinterprets Arc<[T]> as a LanceBuffer
    ///
    /// This is similar to [`Self::reinterpret_vec`] but for Arc<[T]> instead of Vec<T>
    ///
    /// The same alignment constraints apply
    pub fn reinterpret_slice<T: ArrowNativeType + RefUnwindSafe>(arc: Arc<[T]>) -> Self {
        let slice = arc.as_ref();
        let data = NonNull::new(slice.as_ptr() as _).unwrap_or(NonNull::dangling());
        let len = std::mem::size_of_val(slice);
        // SAFETY: the ptr will be valid for len items if the Arc<[T]> is valid
        let buffer = unsafe { Buffer::from_custom_allocation(data, len, Arc::new(arc)) };
        Self::Borrowed(buffer)
    }

    /// Reinterprets a LanceBuffer into a Vec<T>
    ///
    /// If the underlying buffer is not properly aligned, this will involve a copy of the data
    ///
    /// Note: doing this sort of re-interpretation generally makes assumptions about the endianness
    /// of the data.  Lance does not support big-endian machines so this is safe.  However, if we end
    /// up supporting big-endian machines in the future, then any use of this method will need to be
    /// carefully reviewed.
    pub fn borrow_to_typed_slice<T: ArrowNativeType>(&mut self) -> ScalarBuffer<T> {
        let align = std::mem::align_of::<T>();
        let is_aligned = self.as_ptr().align_offset(align) == 0;
        if self.len() % std::mem::size_of::<T>() != 0 {
            panic!("attempt to borrow_to_typed_slice to data type of size {} but we have {} bytes which isn't evenly divisible", std::mem::size_of::<T>(), self.len());
        }

        if is_aligned {
            ScalarBuffer::<T>::from(self.borrow_and_clone().into_buffer())
        } else {
            let num_values = self.len() / std::mem::size_of::<T>();
            let vec = Vec::<T>::with_capacity(num_values);
            let mut bytes = MutableBuffer::from(vec);
            bytes.extend_from_slice(self);
            ScalarBuffer::<T>::from(Buffer::from(bytes))
        }
    }

    /// Concatenates multiple buffers into a single buffer, consuming the input buffers
    ///
    /// If there is only one buffer, it will be returned as is
    pub fn concat_into_one(buffers: Vec<Self>) -> Self {
        if buffers.len() == 1 {
            return buffers.into_iter().next().unwrap();
        }

        let mut total_len = 0;
        for buffer in &buffers {
            total_len += buffer.len();
        }

        let mut data = Vec::with_capacity(total_len);
        for buffer in buffers {
            data.extend_from_slice(buffer.as_ref());
        }

        Self::Owned(data)
    }

    /// Zips multiple buffers into a single buffer, consuming the input buffers
    ///
    /// Unlike concat_into_one this "zips" the buffers, interleaving the values
    pub fn zip_into_one(buffers: Vec<(Self, u64)>, num_values: u64) -> Result<Self> {
        let bytes_per_value = buffers.iter().map(|(_, bits_per_value)| {
            if bits_per_value % 8 == 0 {
                Ok(bits_per_value / 8)
            } else {
                Err(Error::InvalidInput { source: format!("LanceBuffer::zip_into_one only supports full-byte buffers currently and received a buffer with {} bits per value", bits_per_value).into(), location: location!() })
            }
        }).collect::<Result<Vec<_>>>()?;
        let total_bytes_per_value = bytes_per_value.iter().sum::<u64>();
        let total_bytes = (total_bytes_per_value * num_values) as usize;

        let mut zipped = vec![0_u8; total_bytes];
        let mut buffer_ptrs = buffers
            .iter()
            .zip(bytes_per_value)
            .map(|((buffer, _), bytes_per_value)| (buffer.as_ptr(), bytes_per_value as usize))
            .collect::<Vec<_>>();

        let mut zipped_ptr = zipped.as_mut_ptr();
        unsafe {
            let end = zipped_ptr.add(total_bytes);
            while zipped_ptr < end {
                for (buf, bytes_per_value) in buffer_ptrs.iter_mut() {
                    std::ptr::copy_nonoverlapping(*buf, zipped_ptr, *bytes_per_value);
                    zipped_ptr = zipped_ptr.add(*bytes_per_value);
                    *buf = buf.add(*bytes_per_value);
                }
            }
        }

        Ok(Self::Owned(zipped))
    }

    /// Create a LanceBuffer from a slice
    ///
    /// This is NOT a zero-copy operation.  We can't even create a borrowed buffer because
    /// we have no way of extending the lifetime of the slice.
    pub fn copy_slice(slice: &[u8]) -> Self {
        Self::Owned(slice.to_vec())
    }

    /// Create a LanceBuffer from an array (fixed-size slice)
    ///
    /// This is NOT a zero-copy operation.  The slice memory could be on the stack and
    /// thus we can't forget it.
    pub fn copy_array<const N: usize>(array: [u8; N]) -> Self {
        Self::Owned(Vec::from(array))
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        match self {
            Self::Borrowed(buffer) => buffer.len(),
            Self::Owned(buffer) => buffer.len(),
        }
    }

    /// Returns a new [LanceBuffer] that is a slice of this buffer starting at `offset`,
    /// with `length` bytes.
    /// Doing so allows the same memory region to be shared between lance buffers.
    /// # Panics
    /// Panics if `(offset + length)` is larger than the existing length.
    /// If the buffer is owned this method will require a copy.
    pub fn slice_with_length(&self, offset: usize, length: usize) -> Self {
        let original_buffer_len = self.len();
        assert!(
            offset.saturating_add(length) <= original_buffer_len,
            "the offset + length of the sliced Buffer cannot exceed the existing length"
        );
        match self {
            Self::Borrowed(buffer) => Self::Borrowed(buffer.slice_with_length(offset, length)),
            Self::Owned(buffer) => Self::Owned(buffer[offset..offset + length].to_vec()),
        }
    }

    // Backport of https://github.com/apache/arrow-rs/pull/6707
    fn arrow_bit_slice(
        buf: &arrow_buffer::Buffer,
        offset: usize,
        len: usize,
    ) -> arrow_buffer::Buffer {
        if offset % 8 == 0 {
            return buf.slice_with_length(offset / 8, len.div_ceil(8));
        }

        arrow_buffer::bitwise_unary_op_helper(buf, offset, len, |a| a)
    }

    /// Returns a new [LanceBuffer] that is a slice of this buffer starting at bit `offset`
    /// with `length` bits.
    ///
    /// Unlike `slice_with_length`, this method allows for slicing at a bit level but always
    /// requires a copy of the data (unless offset is byte-aligned)
    ///
    /// This method also converts to a borrowed buffer for convenience, but that could be optimized
    /// away in the future if needed.
    ///
    /// This method performs the bit slice using the Arrow convention of *bitwise* little-endian
    ///
    /// This means, given the bit buffer 0bABCDEFGH_HIJKLMNOP and the slice starting at bit 3 and
    /// with length 8, the result will be 0bNOPABCDE
    pub fn bit_slice_le_with_length(&mut self, offset: usize, length: usize) -> Self {
        let Self::Borrowed(borrowed) = self.borrow_and_clone() else {
            unreachable!()
        };
        // Use this and remove backport once we upgrade to arrow-rs 54
        // let sliced = borrowed.bit_slice(offset, length);
        let sliced = Self::arrow_bit_slice(&borrowed, offset, length);
        Self::Borrowed(sliced)
    }
}

impl AsRef<[u8]> for LanceBuffer {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Borrowed(buffer) => buffer.as_slice(),
            Self::Owned(buffer) => buffer.as_slice(),
        }
    }
}

impl Deref for LanceBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

// All `From` implementations are zero-copy

impl From<Vec<u8>> for LanceBuffer {
    fn from(buffer: Vec<u8>) -> Self {
        Self::Owned(buffer)
    }
}

impl From<Buffer> for LanceBuffer {
    fn from(buffer: Buffer) -> Self {
        Self::Borrowed(buffer)
    }
}

// An iterator that keeps a clone of a borrowed LanceBuffer so we
// can have a 'static lifetime
pub struct BorrowedBufferIter {
    buffer: arrow_buffer::Buffer,
    index: usize,
}

impl Iterator for BorrowedBufferIter {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.buffer.len() {
            None
        } else {
            // SAFETY: we just checked that index is in bounds
            let byte = unsafe { self.buffer.get_unchecked(self.index) };
            self.index += 1;
            Some(*byte)
        }
    }
}

impl IntoIterator for LanceBuffer {
    type Item = u8;
    type IntoIter = Either<std::vec::IntoIter<u8>, BorrowedBufferIter>;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::Borrowed(buffer) => Either::Right(BorrowedBufferIter { buffer, index: 0 }),
            Self::Owned(buffer) => Either::Left(buffer.into_iter()),
        }
    }
}

#[cfg(test)]
mod tests {
    use arrow_buffer::Buffer;

    use super::LanceBuffer;

    #[test]
    fn test_eq() {
        let buf = LanceBuffer::Borrowed(Buffer::from_vec(vec![1_u8, 2, 3]));
        let buf2 = LanceBuffer::Owned(vec![1, 2, 3]);
        assert_eq!(buf, buf2);
    }

    #[test]
    fn test_reinterpret_vec() {
        let vec = vec![1_u32, 2, 3];
        let mut buf = LanceBuffer::reinterpret_vec(vec);

        let mut expected = Vec::with_capacity(12);
        expected.extend_from_slice(&1_u32.to_ne_bytes());
        expected.extend_from_slice(&2_u32.to_ne_bytes());
        expected.extend_from_slice(&3_u32.to_ne_bytes());
        let expected = LanceBuffer::Owned(expected);

        assert_eq!(expected, buf);
        assert_eq!(buf.borrow_to_typed_slice::<u32>().as_ref(), vec![1, 2, 3]);
    }

    #[test]
    fn test_concat() {
        let buf1 = LanceBuffer::Owned(vec![1_u8, 2, 3]);
        let buf2 = LanceBuffer::Owned(vec![4_u8, 5, 6]);
        let buf3 = LanceBuffer::Owned(vec![7_u8, 8, 9]);

        let expected = LanceBuffer::Owned(vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            expected,
            LanceBuffer::concat_into_one(vec![buf1, buf2, buf3])
        );

        let empty = LanceBuffer::empty();
        assert_eq!(
            LanceBuffer::empty(),
            LanceBuffer::concat_into_one(vec![empty])
        );

        let expected = LanceBuffer::Owned(vec![1, 2, 3]);
        assert_eq!(
            expected,
            LanceBuffer::concat_into_one(vec![expected.deep_copy(), LanceBuffer::empty()])
        );
    }

    #[test]
    fn test_zip() {
        let buf1 = LanceBuffer::Owned(vec![1_u8, 2, 3]);
        let buf2 = LanceBuffer::reinterpret_vec(vec![1_u16, 2, 3]);
        let buf3 = LanceBuffer::reinterpret_vec(vec![1_u32, 2, 3]);

        let zipped = LanceBuffer::zip_into_one(vec![(buf1, 8), (buf2, 16), (buf3, 32)], 3).unwrap();

        assert_eq!(zipped.len(), 21);

        let mut expected = Vec::with_capacity(21);
        for i in 1..4 {
            expected.push(i as u8);
            expected.extend_from_slice(&(i as u16).to_ne_bytes());
            expected.extend_from_slice(&(i as u32).to_ne_bytes());
        }
        let expected = LanceBuffer::Owned(expected);

        assert_eq!(expected, zipped);
    }

    #[test]
    fn test_hex() {
        let buf = LanceBuffer::Owned(vec![1, 2, 15, 20]);
        assert_eq!("01020F14", buf.as_hex());
    }

    #[test]
    #[should_panic]
    fn test_to_typed_slice_invalid() {
        let mut buf = LanceBuffer::Owned(vec![0, 1, 2]);
        buf.borrow_to_typed_slice::<u16>();
    }

    #[test]
    fn test_to_typed_slice() {
        // Buffer is aligned, no copy will be made, both calls
        // should get same ptr
        let mut buf = LanceBuffer::Owned(vec![0, 1]);
        let borrow = buf.borrow_to_typed_slice::<u16>();
        let view_ptr = borrow.as_ref().as_ptr();
        let borrow2 = buf.borrow_to_typed_slice::<u16>();
        let view_ptr2 = borrow2.as_ref().as_ptr();

        assert_eq!(view_ptr, view_ptr2);

        let bytes = bytes::Bytes::from(vec![0, 1, 2]);
        let sliced = bytes.slice(1..3);
        // Intentionally LYING about alignment here to trigger test
        let mut buf = LanceBuffer::from_bytes(sliced, 1);
        let borrow = buf.borrow_to_typed_slice::<u16>();
        let view_ptr = borrow.as_ref().as_ptr();
        let borrow2 = buf.borrow_to_typed_slice::<u16>();
        let view_ptr2 = borrow2.as_ref().as_ptr();

        assert_ne!(view_ptr, view_ptr2);
    }

    #[test]
    fn test_bit_slice_le() {
        let mut buf = LanceBuffer::Owned(vec![0x0F, 0x0B]);

        // Keep in mind that validity buffers are *bitwise* little-endian
        assert_eq!(buf.bit_slice_le_with_length(0, 4).as_ref(), &[0x0F]);
        assert_eq!(buf.bit_slice_le_with_length(4, 4).as_ref(), &[0x00]);
        assert_eq!(buf.bit_slice_le_with_length(3, 8).as_ref(), &[0x61]);
        assert_eq!(buf.bit_slice_le_with_length(0, 8).as_ref(), &[0x0F]);
        assert_eq!(buf.bit_slice_le_with_length(4, 8).as_ref(), &[0xB0]);
        assert_eq!(buf.bit_slice_le_with_length(4, 12).as_ref(), &[0xB0, 0x00]);
    }
}
