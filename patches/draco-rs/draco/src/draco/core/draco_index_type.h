// Copyright 2016 The Draco Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// This files provides a basic framework for strongly typed indices that are
// used within the Draco library. The motivation of using strongly typed indices
// is to prevent bugs caused by mixing up incompatible indices, such as indexing
// mesh faces with point indices and vice versa.
//
// Usage:
//      Define strongly typed index using macro:
//
//        DEFINE_NEW_DRACO_INDEX_TYPE(value_type, name)
//
//      where |value_type| is the data type of the index value (such as int32_t)
//      and |name| is a unique typename of the new index.
//
//      E.g., we can define new index types as:
//
//        DEFINE_NEW_DRACO_INDEX_TYPE(int, PointIndex)
//        DEFINE_NEW_DRACO_INDEX_TYPE(int, FaceIndex)
//
//      The new types can then be used in the similar way as the regular weakly
//      typed indices (such as int32, int64, ...), but they cannot be
//      accidentally misassigned. E.g.:
//
//        PointIndex point_index(10);
//        FaceIndex face_index;
//        face_index = point_index;  // Compile error!
//
//      One can still cast one type to another explicitly by accessing the index
//      value directly using the .value() method:
//
//        face_index = FaceIndex(point_index.value());  // Compiles OK.
//
//      Strongly typed indices support most of the common binary and unary
//      operators and support for additional operators can be added if
//      necessary.

#ifndef DRACO_CORE_DRACO_INDEX_TYPE_H_
#define DRACO_CORE_DRACO_INDEX_TYPE_H_

#include <inttypes.h>
#include <ostream>

#include "draco/draco_features.h"

namespace draco {

// autocxx has trouble with understanding the IndexType::ValueType,
// and since ALL usage of this macro is with uint32_t, we can just
// expose this directly for autocxx.
using IndexValueType = uint32_t;

// #define DEFINE_NEW_DRACO_INDEX_TYPE(value_type, name) \
//   struct name##_tag_type_ {};                         \
//   typedef IndexType<name##_tag_type_> name;

#define DEFINE_NEW_DRACO_INDEX_TYPE(value_type, name)                      \
  /* first generate the class inside draco namespace */                    \
  class name##IndexType {                                                  \
   public:                                                                 \
    typedef uint32_t ValueType;                                            \
                                                                           \
    constexpr name##IndexType() : val(ValueType()) {}                      \
    constexpr explicit name##IndexType(ValueType value) : val(value) {}    \
    constexpr name##IndexType with(ValueType value) {                      \
      return name##IndexType(value);                                       \
    }                                                                      \
                                                                           \
    constexpr ValueType value() const { return val; }                      \
                                                                           \
    constexpr bool operator==(const name##IndexType &i) const {            \
      return val == i.val;                                                 \
    }                                                                      \
    constexpr bool operator==(const ValueType &val_in) const {             \
      return val == val_in;                                                \
    }                                                                      \
    constexpr bool operator!=(const name##IndexType &i) const {            \
      return val != i.val;                                                 \
    }                                                                      \
    constexpr bool operator!=(const ValueType &val_in) const {             \
      return val != val_in;                                                \
    }                                                                      \
    constexpr bool operator<(const name##IndexType &i) const {             \
      return val < i.val;                                                  \
    }                                                                      \
    constexpr bool operator<(const ValueType &val_in) const {              \
      return val < val_in;                                                 \
    }                                                                      \
    constexpr bool operator>(const name##IndexType &i) const {             \
      return val > i.val;                                                  \
    }                                                                      \
    constexpr bool operator>(const ValueType &val_in) const {              \
      return val > val_in;                                                 \
    }                                                                      \
    constexpr bool operator>=(const name##IndexType &i) const {            \
      return val >= i.val;                                                 \
    }                                                                      \
    constexpr bool operator>=(const ValueType &val_in) const {             \
      return val >= val_in;                                                \
    }                                                                      \
                                                                           \
    inline name##IndexType &operator++() {                                 \
      ++val;                                                               \
      return *this;                                                        \
    }                                                                      \
    inline name##IndexType operator++(int) {                               \
      const name##IndexType ret(val);                                      \
      ++val;                                                               \
      return ret;                                                          \
    }                                                                      \
                                                                           \
    inline name##IndexType &operator--() {                                 \
      --val;                                                               \
      return *this;                                                        \
    }                                                                      \
    inline name##IndexType operator--(int) {                               \
      const name##IndexType ret(val);                                      \
      --val;                                                               \
      return ret;                                                          \
    }                                                                      \
                                                                           \
    constexpr name##IndexType operator+(const name##IndexType &i) const {  \
      return name##IndexType(val + i.val);                                 \
    }                                                                      \
    constexpr name##IndexType operator+(const ValueType &val_in) const {   \
      return name##IndexType(val + val_in);                                \
    }                                                                      \
    constexpr name##IndexType operator-(const name##IndexType &i) const {  \
      return name##IndexType(val - i.val);                                 \
    }                                                                      \
    constexpr name##IndexType operator-(const ValueType &val_in) const {   \
      return name##IndexType(val - val_in);                                \
    }                                                                      \
                                                                           \
    inline name##IndexType &operator+=(const name##IndexType &i) {         \
      val += i.val;                                                        \
      return *this;                                                        \
    }                                                                      \
    inline name##IndexType operator+=(const ValueType &val_in) {           \
      val += val_in;                                                       \
      return *this;                                                        \
    }                                                                      \
    inline name##IndexType &operator-=(const name##IndexType &i) {         \
      val -= i.val;                                                        \
      return *this;                                                        \
    }                                                                      \
    inline name##IndexType operator-=(const ValueType &val_in) {           \
      val -= val_in;                                                       \
      return *this;                                                        \
    }                                                                      \
    inline name##IndexType &operator=(const name##IndexType &i) {          \
      val = i.val;                                                         \
      return *this;                                                        \
    }                                                                      \
    inline name##IndexType &operator=(const ValueType &val_in) {           \
      val = val_in;                                                        \
      return *this;                                                        \
    }                                                                      \
    /* NO private field to support POD in rust bindings */                 \
    /*  private: */                                                        \
    ValueType val;                                                         \
  };                                                                       \
  typedef name##IndexType name;                                            \
                                                                           \
// /* also generate a << operator for logging purposes */                           \
// std::ostream &operator<<(std::ostream &os, name##IndexType index) {              \
//   return os << index.value();                                                    \
// }

/* and generate the hash function, inside std namespace */
#define DEFINE_NEW_DRACO_INDEX_TYPE_HASH_OPERATOR(name) \
template <>                                             \
struct hash<draco::name> {                              \
  size_t operator()(const draco::name &i) const {       \
    return static_cast<size_t>(i.value());              \
  }                                                     \
};

} // namespace draco

#endif  // DRACO_CORE_DRACO_INDEX_TYPE_H_
