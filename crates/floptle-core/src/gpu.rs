//! The element types a GPU buffer holds, and how WGSL lays them out in
//! storage. Shared by the renderer, which declares and binds the buffers, and
//! the script API, which turns a script's numbers into the bytes a shader
//! reads: the two must agree to the byte, so the rule lives in one place.

/// One element type a buffer can hold, as WGSL names it.
#[derive(Clone, Debug, PartialEq)]
pub enum Elem {
    F32,
    U32,
    I32,
    Vec2,
    Vec3,
    Vec4,
    /// A struct of named fields, each one of the scalar or vector types
    /// above. Laid out the way WGSL lays out storage (a `vec3` field starts
    /// on 16 bytes), so the bytes a script writes are the bytes the shader
    /// reads.
    Struct(Vec<(String, Elem)>),
}

impl Elem {
    /// WGSL's (size, alignment) for this type in a storage buffer.
    pub fn size_align(&self) -> (u32, u32) {
        match self {
            Elem::F32 | Elem::U32 | Elem::I32 => (4, 4),
            Elem::Vec2 => (8, 8),
            Elem::Vec3 => (12, 16),
            Elem::Vec4 => (16, 16),
            Elem::Struct(fields) => {
                let (mut off, mut align) = (0u32, 4u32);
                for (_, f) in fields {
                    let (s, a) = f.size_align();
                    off = off.div_ceil(a) * a + s;
                    align = align.max(a);
                }
                (off.div_ceil(align) * align, align)
            }
        }
    }

    /// The stride between two elements of an `array<T>` of this type.
    pub fn stride(&self) -> u32 {
        let (s, a) = self.size_align();
        s.div_ceil(a) * a
    }

    /// Each field's byte offset, for a struct (empty otherwise).
    pub fn field_offsets(&self) -> Vec<(String, Elem, u32)> {
        let Elem::Struct(fields) = self else { return Vec::new() };
        let mut off = 0u32;
        fields
            .iter()
            .map(|(n, f)| {
                let (s, a) = f.size_align();
                let at = off.div_ceil(a) * a;
                off = at + s;
                (n.clone(), f.clone(), at)
            })
            .collect()
    }

    /// Parse a scalar or vector type name as a script writes it: `f32`, `u32`,
    /// `i32`, `vec2`, `vec3`, `vec4` (`vec3f` and `vec3<f32>` too).
    pub fn parse_scalar(s: &str) -> Option<Elem> {
        Some(match s.trim() {
            "f32" | "float" => Elem::F32,
            "u32" | "uint" => Elem::U32,
            "i32" | "int" => Elem::I32,
            "vec2" | "vec2f" | "vec2<f32>" => Elem::Vec2,
            "vec3" | "vec3f" | "vec3<f32>" => Elem::Vec3,
            "vec4" | "vec4f" | "vec4<f32>" => Elem::Vec4,
            _ => return None,
        })
    }
}
