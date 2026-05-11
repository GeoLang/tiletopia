mod attribute;
mod bindgen;
mod converter;
pub mod decode;
pub mod encode;
pub mod pointcloud;
mod utils;

pub mod prelude {
    pub mod ffi {
        pub use crate::bindgen::prelude::ffi::*;
        // pub use crate::bindgen::prelude::ffi::draco_extra;
        // pub use crate::bindgen_extra::prelude::ffi::draco_extra;
    }
    // pub use crate::attribute::NonOwningPointAttribute;
    pub use crate::converter::StatusOr;
    pub use crate::decode::{Decoder, DecoderBuffer};
    pub use crate::encode::{Encoder, EncoderBuffer};
    pub use crate::utils::*;
}
