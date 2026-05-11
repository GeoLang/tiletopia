#pragma once

#include <memory>
#include "draco/io/point_cloud_io.h"
#include "draco/io/mesh_io.h"
#include <sstream>
// #include "cxxgen1.h"
// #include "draco"


namespace draco_extra
{

    // struct BlobMetadata;

    // struct MeshWithStatus
    // {

    //     BlobMetadata metadata {};

    //     std::unique_ptr<draco::Mesh> mesh;
    //     draco::Status status;

    //     // Constructor to initialize the struct
    //     MeshWithStatus(std::unique_ptr<draco::Mesh> m, draco::Status s)
    //         : mesh(std::move(m)), status(std::move(s)) {}
    // };

    // inline BlobMetadata unpack_blob_metadata()
    // {
    //     BlobMetadata a{};

    //     // a.metadata = pc.value()->GetBlobMetadata();

    //     return a;
    // }

    inline draco::Status unpack_status_or_mesh_status(const draco::StatusOr<std::unique_ptr<draco::Mesh>> &pc)
    {
        return pc.status();
    }

    inline std::unique_ptr<draco::Mesh> unpack_status_or_mesh_value(draco::StatusOr<std::unique_ptr<draco::Mesh>> &pc)
    {
        return std::move(pc).value();
    }

    inline draco::Status unpack_status_or_pointcloud_status(const draco::StatusOr<std::unique_ptr<draco::PointCloud>> &pc)
    {
        return pc.status();
    }

    inline std::unique_ptr<draco::PointCloud> unpack_status_or_pointcloud_value(draco::StatusOr<std::unique_ptr<draco::PointCloud>> &pc)
    {
        return std::move(pc).value();
    }

    // inline std::unique_ptr<MeshWithStatus> unpack_status_mesh(draco::StatusOr<std::unique_ptr<draco::Mesh>> &pc)
    // {


    //     BlobMetadata a{};

    //     return std::make_unique<MeshWithStatus>(
    //         std::move(pc).value(),
    //         pc.status()
    //     );


    //     // if (!pc.ok())
    //     // {
    //     //     return nullptr;
    //     // }

    //     // return std::move(pc).value();
    // }
    // std::unique_ptr<MeshWithStatus> unpack_status_mesh(draco::StatusOr<std::unique_ptr<draco::Mesh>> &pc);

    // instantiate the template for the types we need

} // namespace name
