use draco_rs::prelude::*;

use cxx::let_cxx_string;

fn main() {
    let_cxx_string!(fname = "/home/soraxas/git-repos/draco_ori/testdata/bun_zipper.ply");
    let mut maybe_pc = ffi::draco::ReadMeshFromFile(&fname);

    if let Some(pc) = maybe_pc.as_mut() {
        if pc.ok() {
            let mut mesh = pc.value();

            // num_faces can already be accessed without mut
            mesh.num_faces();

            if let Some(mesh) = mesh.as_mut() {
                println!("loaded ok num faces: {}", mesh.num_faces());
            } else {
                println!("Failed to get mesh mut");
            }
        } else {
            println!("Draco error: {}", pc.error_msg());
        }
    } else {
        println!("Failed to read mesh from file");
    }
}
