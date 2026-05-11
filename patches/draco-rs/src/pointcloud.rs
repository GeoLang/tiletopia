use crate::{
    decode::{Decoder, DecoderBuffer},
    encode::{Encoder, EncoderBuffer},
    prelude::*,
};
use autocxx::prelude::*;

pub type PointCloudBuilder = WrappedDracoObject<ffi::draco::PointCloudBuilder>;

impl PointCloudBuilder {
    pub fn new(num: u32) -> Self {
        let mut builder = ffi::draco::PointCloudBuilder::new().within_unique_ptr();
        builder.pin_mut().Start(num);
        Self(builder)
    }

    pub fn add_attribute(
        &mut self,
        attribute_type: ffi::draco::GeometryAttribute_Type,
        num_components: i8,
        data_type: ffi::draco::DataType,
    ) -> AttrId {
        self.0
            .pin_mut()
            .AddAttribute(attribute_type, num_components, data_type)
            .into()
    }

    /// Adds the data from the provided slice as the attribute value for a specific point.
    ///
    /// # Safety
    ///
    /// The `point` slice must point to valid memory with a lifetime at least as long as the `PointCloudBuilder` instance.
    /// The size and type of the data in the slice must be compatible with the attribute `attr_id`.
    pub fn add_point<T>(
        &mut self,
        attr_id: AttrId,
        point_index: impl Into<ffi::draco::PointIndex>,
        point: &[T],
    ) {
        unsafe { self.add_point_with_ptr(attr_id, point_index, point.as_ptr() as *const c_void) }
    }

    /// Adds the data pointed to by the raw pointer as the attribute value for a specific point.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it takes a raw pointer `ptr` to the point data.
    /// It is the caller's responsibility to ensure that:
    /// - `ptr` is valid and points to memory that is properly aligned for the attribute type.
    /// - The memory pointed to by `ptr` has a lifetime at least as long as the `PointCloudBuilder` instance.
    /// - The size of the data pointed to by `ptr` is sufficient for the attribute type associated with `attr_id`.
    pub unsafe fn add_point_with_ptr(
        &mut self,
        attr_id: AttrId,
        point_index: impl Into<ffi::draco::PointIndex>,
        ptr: *const c_void,
    ) {
        self.0
            .pin_mut()
            .SetAttributeValueForPoint(attr_id.into(), point_index.into(), ptr);
    }

    pub fn build(mut self, deduplicate_points: bool) -> PointCloud {
        PointCloud {
            0: self.0.pin_mut().Finalize(deduplicate_points),
        }
    }
}

pub type PointCloud = WrappedDracoObject<ffi::draco::PointCloud>;

impl Default for PointCloud {
    fn default() -> Self {
        Self::new()
    }
}

impl PointCloud {
    pub fn new() -> Self {
        let pc = ffi::draco::PointCloud::new().within_unique_ptr();
        Self(pc)
    }

    // This function returns the attribute id of the attribute with the given name
    // It stores the value in-place
    pub fn get_point<T>(
        &mut self,
        attr_id: AttrId,
        point_index: impl Into<ffi::draco::PointIndex>,
        point_container: &mut [T],
    ) where
        T: Default + Copy,
    {
        let pa_ptr = self.0.pin_mut().GetAttributeByUniqueId(attr_id.as_u32());
        unsafe {
            (*pa_ptr).GetMappedValue(
                point_index.into(),
                point_container.as_mut_ptr() as *mut c_void,
            );
        };
    }

    // This function allocates a new array of type T and fills it with the point data
    pub fn get_point_alloc<T, const N: usize>(
        &mut self,
        attr_id: AttrId,
        point_index: impl Into<ffi::draco::PointIndex>,
    ) -> [T; N]
    where
        T: Default + Copy,
    {
        let mut point = [T::default(); N];
        self.get_point(attr_id, point_index, &mut point);
        point
    }

    // Returns the number of named attributes of a given type.
    pub fn num_named_attributes(&self, attr_type: ffi::draco::GeometryAttribute_Type) -> i32 {
        self.0.NumNamedAttributes(attr_type)
    }

    // Returns the id of the i-th named attribute of a given type.
    pub fn get_named_attribute_id(
        &self,
        attr_type: ffi::draco::GeometryAttribute_Type,
        i: i32,
    ) -> Option<AttrId> {
        let id = self.0.GetNamedAttributeId1(attr_type, i.into());
        if id < 0 {
            None
        } else {
            Some(AttrId(id))
        }
    }

    // // Returns the i-th named attribute of a given type.
    // pub fn get_named_attribute(
    //     &self,
    //     attr_type: ffi::draco::GeometryAttribute_Type,
    //     i: i32,
    // ) -> Option<NonOwningPointAttribute> {
    //     let attr = self.0.GetNamedAttribute1(attr_type, i.into());
    //     if attr.is_null() {
    //         None
    //     } else {
    //         Some(NonOwningPointAttribute { ptr: attr })
    //     }
    // }

    //   // Returns the named attribute of a given unique id.
    //     pub fn get_named_attribute_by_unique_id(
    //         &self,
    //         attr_type: ffi::draco::GeometryAttribute_Type,
    //         id: u32,
    //     ) -> Option<NonOwningPointAttribute> {
    //         let attr = self.0.GetNamedAttributeByUniqueId(attr_type, id);
    //         if attr.is_null() {
    //             None
    //         } else {
    //             Some(NonOwningPointAttribute { ptr: attr })
    //         }
    //     }

    pub fn num_points(&self) -> u32 {
        self.0.num_points()
    }

    pub fn len(&self) -> u32 {
        self.0.num_points()
    }

    pub fn is_empty(&self) -> bool {
        self.0.num_points() == 0
    }

    /// Encode the point cloud to an encoder buffer
    pub fn to_buffer(&self, encoder: &mut Encoder) -> DracoStatusType<EncoderBuffer> {
        let mut buffer = EncoderBuffer::new();

        let status = unsafe {
            encoder
                .0
                .pin_mut()
                .EncodePointCloudToBuffer(self.0.as_ref().unwrap(), buffer.as_mut_ptr())
                .within_unique_ptr()
        };

        if status.ok() {
            Ok(buffer)
        } else {
            Err(status.into())
        }
    }

    /// Decode a point cloud from a decoder buffer
    ///
    /// # Safety
    ///
    /// The decoder buffer must contains valid memory
    pub fn from_buffer(decoder: &mut Decoder, buffer: &mut DecoderBuffer) -> DracoStatusType<Self> {
        let mut status_or = unsafe {
            decoder
                .decoder
                .pin_mut()
                .DecodePointCloudFromBuffer(buffer.0.as_mut_ptr())
        };
        if status_or.ok() {
            Ok(Self(status_or.pin_mut().value()))
        } else {
            Err(status_or.status().within_unique_ptr().into())
        }
    }
}
