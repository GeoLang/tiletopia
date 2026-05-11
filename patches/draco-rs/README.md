# draco-rs

Rust bindings for the *forked* [Draco](https://github.com/soraxas/draco) library, providing efficient compression and decompression of 3D meshes and point clouds.

## Features

- Encode and decode 3D geometry (meshes & point clouds)
- Direct, low-overhead mapping to core Draco constructs
- Support for custom attributes and per-point data

**Status**: idiomatic Rust API will only be added on a as-needed basis. If you need to access some non-exposed draco API, call the `wrapped_draco_obj.get_inner[_mut]()` function to manipulate the underlying unique ptr.

## Installation

Add `draco-rs` to your `Cargo.toml`:

```toml
[dependencies]
draco-rs = "x.x.x"
```

## Quick Start

```rust
use draco_rs::{prelude::{*, ffi::draco::{GeometryAttribute_Type, DataType}}, pointcloud::*};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Build a point cloud with 3 components per point
    let mut builder = PointCloudBuilder::new(1);
    let attr_id = builder.add_attribute(GeometryAttribute_Type::POSITION, 3, DataType::DT_FLOAT32);
    builder.add_point(attr_id, 0, &[0.0f32, 1.0, 2.0]);
    let cloud = builder.build(false);

    // Encode to a buffer
    let mut encoded = cloud.to_buffer(&mut Encoder::default())?;
    // note: the decoder buffer does not take ownership of the given buffer, so the buffer must be valid for the lifetime of the decode process.
    let mut decoder_buffer = DecoderBuffer::from_encoder_buffer(&mut encoded);

    // Decode back to a PointCloud
    let mut decoded = PointCloud::from_buffer(&mut Decoder::default(), &mut decoder_buffer)?;
    assert_eq!(decoded.get_point_alloc::<f32, 3>(attr_id, 0), [0.0, 1.0, 2.0]);

    Ok(())
}
```

## License

Distributed under the MIT License. See [LICENSE](LICENSE) for details.

[autocxx]: https://github.com/google/autocxx
