use draco_rs::prelude::*;
use std::io;

use draco_rs::pointcloud::{PointCloud, PointCloudBuilder};

fn gen_points() -> Vec<[f64; 3]> {
    // This function returns a vector of points
    // In a real application, you would load this data from a file or other source
    vec![
        [0.0, 0.1, 0.2],
        [0.1, 0.2, 0.3],
        [0.2, 0.3, 0.4],
        [0.3, 0.4, 0.5],
        [0.4, 0.5, 0.6],
        [0.5, 0.6, 0.7],
        [0.6, 0.7, 0.8],
        [0.7, 0.8, 0.9],
        [0.8, 0.9, 1.0],
        [0.9, 1.0, 1.1],
    ]
}

fn print_pc(pc: &mut PointCloud, attr_id: AttrId) {
    let mut container: [f64; 3] = [0.0; 3];
    for i in 0..pc.len() {
        pc.get_point(attr_id, i, &mut container);
        println!("Point {}: {:?}", i, container);
    }
}

fn main() -> io::Result<()> {
    let points = gen_points();

    ////////// BUINDING //////////
    let mut builder = PointCloudBuilder::new(points.len() as u32);

    let attr_id = builder.add_attribute(
        ffi::draco::GeometryAttribute_Type::POSITION,
        3,
        ffi::draco::DataType::DT_FLOAT64,
    );

    for (i, point) in points.iter().enumerate() {
        builder.add_point(attr_id, i, point);
    }
    let mut pc = builder.build(false);

    println!("after building");
    print_pc(&mut pc, attr_id);

    ////////// ENCODE //////////
    let mut encoder = Encoder::new()
        .set_speed_options(5, 5)
        .set_attribute_quantization(ffi::draco::GeometryAttribute_Type::POSITION, 14);

    if let Ok(mut buffer) = pc.to_buffer(&mut encoder) {
        ////////// DECODE //////////
        let mut buf = DecoderBuffer::from_encoder_buffer(&mut buffer);
        let pc_decoded = PointCloud::from_buffer(&mut Decoder::new(), &mut buf);

        if let Ok(mut pc_decoded) = pc_decoded {
            println!("after decoding");
            print_pc(&mut pc_decoded, attr_id);
        } else {
            println!("Failed to decode point cloud");
        }
    }
    Ok(())
}
