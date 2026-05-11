use crate::{encode::EncoderBuffer, prelude::*};
use autocxx::prelude::*;

pub type DecoderBuffer = WrappedDracoObject<ffi::draco::DecoderBuffer>;

impl DecoderBuffer {
    pub fn new() -> Self {
        Self(ffi::draco::DecoderBuffer::new().within_unique_ptr())
    }

    /// Initializes the decoder buffer with a raw data pointer and size
    ///
    /// # Safety
    ///
    /// The data pointer must be valid for the lifetime of the decoder buffer
    pub unsafe fn from_buffer_ptr(data_ptr: *const std::os::raw::c_char, size: usize) -> Self {
        let mut buffer = Self::new();
        buffer.0.pin_mut().Init(data_ptr, size);
        buffer
    }

    /// Initializes the decoder buffer with a slice of data
    ///
    /// # Safety
    ///
    /// The given slice must be valid for the lifetime of the decoder buffer
    pub fn from_buffer(data_buffer: &[u8]) -> Self {
        unsafe {
            Self::from_buffer_ptr(
                data_buffer.as_ptr() as *const std::os::raw::c_char,
                data_buffer.len(),
            )
        }
    }

    /// Initializes the decoder buffer with the data from the encoder buffer
    pub fn from_encoder_buffer(encoder_buffer: &mut EncoderBuffer) -> Self {
        Self::from_buffer(encoder_buffer.as_slice())
    }
}

impl Default for DecoderBuffer {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Decoder {
    pub(crate) decoder: UniquePtr<ffi::draco::Decoder>,
}

impl Decoder {
    pub fn new() -> Self {
        let decoder = ffi::draco::Decoder::new().within_unique_ptr();
        Self { decoder }
    }
}
impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}
