// use crate::prelude::ffi;

// // Class for storing point specific data about each attribute. In general,
// // multiple points stored in a point cloud can share the same attribute value
// // and this class provides the necessary mapping between point ids and attribute
// // value ids.
// pub struct NonOwningPointAttribute {
//     pub(crate) ptr: *const ffi::draco::PointAttribute,
// }

// impl NonOwningPointAttribute {
//     // # Safety
//     // This function is unsafe because it dereferences the raw pointer.
//     pub unsafe fn size(&self) -> usize {
//         (*self.ptr).size()
//     }
// }
