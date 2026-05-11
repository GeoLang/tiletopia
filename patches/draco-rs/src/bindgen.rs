#[allow(unused_imports)]
use autocxx::prelude::*;

// create a shared struct that can be used in both C++ and Rust.
// This MUST be done in the earlier that the `bindgen_extra` module
// as its `extra.h` header depends on structs defined here.
// (no longer true)
#[cxx::bridge(namespace = "draco_extra")]
pub mod ffi2 {
    // // Shared structs with fields visible to both languages.
    // struct BlobMetadata {
    //     size: usize,
    //     tags: Vec<String>,
    // }

    // C++ types and signatures exposed to Rust.
    unsafe extern "C++" {
        include!("extra.h");
    }
}

autocxx::include_cpp! {
    // name!(ffi_draco)
    // C++ headers we want to include.
    #include "draco/compression/config/compression_shared.h"
    #include "draco/compression/encode.h"
    #include "draco/compression/decode.h"
    #include "draco/compression/expert_encode.h"
    #include "draco/point_cloud/point_cloud_builder.h"
    #include "draco/core/encoder_buffer.h"
    #include "draco/core/cycle_timer.h"
    #include "draco/io/file_utils.h"
    #include "draco/io/mesh_io.h"
    #include "draco/io/point_cloud_io.h"

    #include "draco/io/stdio_file_reader.h"
    #include "extra.h"

    // Safety policy. We are marking that this whole C++ inclusion is unsafe
    // which means the functions themselves do not need to be marked
    // as unsafe. Other policies are possible.
    safety!(unsafe_ffi)
    // What types and functions we want to generate
    // generate!("draco::PointCloud")
    // generate!("draco::EncoderBuffer")
    // generate!("draco::Options")
    // generate!("draco::ExpertEncoder")
    // generate!("draco::ReadPointCloudFromFile")
    // generate!("draco::ReadMeshFromFile")
    // // generate!("draco::StatusOr")
    // generate!("draco::Mesh")
    // generate!("draco::FaceIndex")
    // generate!("draco::IndexValueType")
    // generate!("draco::GeometryAttribute")

    // generate!("draco::Status")

    ////////////////////////////////////////////////////////////////////////////////
    // force the rust bind to generate file reader translation unit
    // because they are using a static register function to register the file reader (sigh)
    // (no longer neede if we pull in the whole namespace)
    generate!("draco::StdioFileReader")
    ////////////////////////////////////////////////////////////////////////////////

    generate_pod!("draco::PointIndexIndexType")
    generate_pod!("draco::AttributeValueIndexIndexType")
    generate_pod!("draco::PointIndexIndexType")
    generate_pod!("draco::VertexIndexIndexType")
    generate_pod!("draco::CornerIndexIndexType")
    generate_pod!("draco::FaceIndexIndexType")

    generate_ns!("draco")
    // the following is a templated class
    block!("draco::EncoderBase")

    generate_ns!("draco_extra")
}

pub mod prelude {

    pub mod ffi {
        pub use crate::bindgen::ffi::*;
    }
}
