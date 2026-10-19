//! Which fields a RON file names that the type reading it does not have.
//!
//! The documents here take unknown fields without complaint — a file written
//! by a newer editor must still open — so a misspelt field (`collde:` for
//! `collide:`) loads, does nothing, and says nothing. A wrong *type* is caught
//! by the parse; a wrong *name* only by asking.
//!
//! Serde hands every field a type does not declare to `deserialize_ignored_any`.
//! [`unknown_fields`] reads the file through a deserializer that wraps ron's
//! and notes the path of every value that goes there, so the answer comes from
//! the same derive that reads the file and cannot drift from it.

use std::cell::RefCell;

use serde::de::{self, DeserializeOwned, DeserializeSeed, Deserializer, Visitor};

/// The dotted path (`tracks[0].collde`) of every field `text` names that `T`
/// ignores. Empty when the file does not parse as a `T` — the parse error is
/// the thing to report then.
pub fn unknown_fields<T: DeserializeOwned>(text: &str) -> Vec<String> {
    let out = RefCell::new(Vec::new());
    let Ok(mut de) = ron::Deserializer::from_str(text) else { return Vec::new() };
    let ok = T::deserialize(Wrap { de: &mut de, path: String::new(), out: &out }).is_ok();
    if ok { out.into_inner() } else { Vec::new() }
}

struct Wrap<'a, D> {
    de: D,
    path: String,
    out: &'a RefCell<Vec<String>>,
}

impl<'a, D> Wrap<'a, D> {
    fn visitor<V>(&self, v: V) -> WrapV<'a, V> {
        WrapV { v, path: self.path.clone(), out: self.out }
    }
}

macro_rules! forward {
    ($($m:ident($($a:ident: $t:ty),*);)*) => {$(
        fn $m<V: Visitor<'de>>(self, $($a: $t,)* v: V) -> Result<V::Value, Self::Error> {
            let w = self.visitor(v);
            self.de.$m($($a,)* w)
        }
    )*};
}

impl<'de, 'a, D: Deserializer<'de>> Deserializer<'de> for Wrap<'a, D> {
    type Error = D::Error;
    forward! {
        deserialize_any(); deserialize_bool(); deserialize_i8(); deserialize_i16();
        deserialize_i32(); deserialize_i64(); deserialize_i128(); deserialize_u8();
        deserialize_u16(); deserialize_u32(); deserialize_u64(); deserialize_u128();
        deserialize_f32(); deserialize_f64(); deserialize_char(); deserialize_str();
        deserialize_string(); deserialize_bytes(); deserialize_byte_buf();
        deserialize_option(); deserialize_unit(); deserialize_unit_struct(n: &'static str);
        deserialize_newtype_struct(n: &'static str); deserialize_seq();
        deserialize_tuple(len: usize); deserialize_tuple_struct(n: &'static str, len: usize);
        deserialize_map(); deserialize_struct(n: &'static str, f: &'static [&'static str]);
        deserialize_enum(n: &'static str, f: &'static [&'static str]); deserialize_identifier();
    }

    /// Where serde sends a field the type does not declare.
    fn deserialize_ignored_any<V: Visitor<'de>>(self, v: V) -> Result<V::Value, Self::Error> {
        self.out.borrow_mut().push(self.path);
        self.de.deserialize_ignored_any(v)
    }

    fn is_human_readable(&self) -> bool {
        self.de.is_human_readable()
    }
}

struct WrapV<'a, V> {
    v: V,
    path: String,
    out: &'a RefCell<Vec<String>>,
}

macro_rules! visit {
    ($($m:ident($t:ty);)*) => {$(
        fn $m<E: de::Error>(self, x: $t) -> Result<Self::Value, E> {
            self.v.$m(x)
        }
    )*};
}

impl<'de, 'a, V: Visitor<'de>> Visitor<'de> for WrapV<'a, V> {
    type Value = V::Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.v.expecting(f)
    }

    visit! {
        visit_bool(bool); visit_i8(i8); visit_i16(i16); visit_i32(i32); visit_i64(i64);
        visit_i128(i128); visit_u8(u8); visit_u16(u16); visit_u32(u32); visit_u64(u64);
        visit_u128(u128); visit_f32(f32); visit_f64(f64); visit_char(char);
        visit_str(&str); visit_borrowed_str(&'de str); visit_string(String);
        visit_bytes(&[u8]); visit_borrowed_bytes(&'de [u8]); visit_byte_buf(Vec<u8>);
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.v.visit_none()
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.v.visit_unit()
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        self.v.visit_some(Wrap { de: d, path: self.path, out: self.out })
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        self.v.visit_newtype_struct(Wrap { de: d, path: self.path, out: self.out })
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        self.v.visit_seq(WrapSeq { seq, path: self.path, out: self.out, i: 0 })
    }

    fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        self.v.visit_map(WrapMap { map, path: self.path, out: self.out, key: String::new() })
    }

    fn visit_enum<A: de::EnumAccess<'de>>(self, e: A) -> Result<Self::Value, A::Error> {
        self.v.visit_enum(WrapEnum { e, path: self.path, out: self.out })
    }
}

/// A seed whose value is read through [`Wrap`] at `path`.
struct WrapSeed<'a, S> {
    seed: S,
    path: String,
    out: &'a RefCell<Vec<String>>,
}

impl<'de, 'a, S: DeserializeSeed<'de>> DeserializeSeed<'de> for WrapSeed<'a, S> {
    type Value = S::Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        self.seed.deserialize(Wrap { de: d, path: self.path, out: self.out })
    }
}

struct WrapSeq<'a, A> {
    seq: A,
    path: String,
    out: &'a RefCell<Vec<String>>,
    i: usize,
}

impl<'de, 'a, A: de::SeqAccess<'de>> de::SeqAccess<'de> for WrapSeq<'a, A> {
    type Error = A::Error;
    fn next_element_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<Option<S::Value>, A::Error> {
        let path = format!("{}[{}]", self.path, self.i);
        self.i += 1;
        self.seq.next_element_seed(WrapSeed { seed, path, out: self.out })
    }
    fn size_hint(&self) -> Option<usize> {
        self.seq.size_hint()
    }
}

struct WrapMap<'a, A> {
    map: A,
    path: String,
    out: &'a RefCell<Vec<String>>,
    /// The key just read, for the path of the value after it.
    key: String,
}

impl<'de, 'a, A: de::MapAccess<'de>> de::MapAccess<'de> for WrapMap<'a, A> {
    type Error = A::Error;
    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, A::Error> {
        let key = RefCell::new(String::new());
        let got = self.map.next_key_seed(KeySeed { seed, key: &key })?;
        self.key = key.into_inner();
        Ok(got)
    }
    fn next_value_seed<S: DeserializeSeed<'de>>(&mut self, seed: S) -> Result<S::Value, A::Error> {
        let path = if self.path.is_empty() { self.key.clone() } else { format!("{}.{}", self.path, self.key) };
        self.map.next_value_seed(WrapSeed { seed, path, out: self.out })
    }
    fn size_hint(&self) -> Option<usize> {
        self.map.size_hint()
    }
}

/// Reads a map key through [`KeyDe`], keeping what it said.
struct KeySeed<'k, S> {
    seed: S,
    key: &'k RefCell<String>,
}

impl<'de, 'k, S: DeserializeSeed<'de>> DeserializeSeed<'de> for KeySeed<'k, S> {
    type Value = S::Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        self.seed.deserialize(KeyDe { de: d, key: self.key })
    }
}

struct KeyDe<'k, D> {
    de: D,
    key: &'k RefCell<String>,
}

macro_rules! forward_key {
    ($($m:ident($($a:ident: $t:ty),*);)*) => {$(
        fn $m<V: Visitor<'de>>(self, $($a: $t,)* v: V) -> Result<V::Value, Self::Error> {
            self.de.$m($($a,)* KeyV { v, key: self.key })
        }
    )*};
}

impl<'de, 'k, D: Deserializer<'de>> Deserializer<'de> for KeyDe<'k, D> {
    type Error = D::Error;
    forward_key! {
        deserialize_any(); deserialize_bool(); deserialize_i8(); deserialize_i16();
        deserialize_i32(); deserialize_i64(); deserialize_i128(); deserialize_u8();
        deserialize_u16(); deserialize_u32(); deserialize_u64(); deserialize_u128();
        deserialize_f32(); deserialize_f64(); deserialize_char(); deserialize_str();
        deserialize_string(); deserialize_bytes(); deserialize_byte_buf();
        deserialize_option(); deserialize_unit(); deserialize_unit_struct(n: &'static str);
        deserialize_newtype_struct(n: &'static str); deserialize_seq();
        deserialize_tuple(len: usize); deserialize_tuple_struct(n: &'static str, len: usize);
        deserialize_map(); deserialize_struct(n: &'static str, f: &'static [&'static str]);
        deserialize_enum(n: &'static str, f: &'static [&'static str]); deserialize_identifier();
        deserialize_ignored_any();
    }
}

/// Forwards everything, noting a key that arrives as a string.
struct KeyV<'k, V> {
    v: V,
    key: &'k RefCell<String>,
}

impl<'de, 'k, V: Visitor<'de>> Visitor<'de> for KeyV<'k, V> {
    type Value = V::Value;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.v.expecting(f)
    }

    fn visit_str<E: de::Error>(self, x: &str) -> Result<Self::Value, E> {
        *self.key.borrow_mut() = x.to_string();
        self.v.visit_str(x)
    }

    fn visit_borrowed_str<E: de::Error>(self, x: &'de str) -> Result<Self::Value, E> {
        *self.key.borrow_mut() = x.to_string();
        self.v.visit_borrowed_str(x)
    }

    fn visit_string<E: de::Error>(self, x: String) -> Result<Self::Value, E> {
        *self.key.borrow_mut() = x.clone();
        self.v.visit_string(x)
    }

    visit! {
        visit_bool(bool); visit_i8(i8); visit_i16(i16); visit_i32(i32); visit_i64(i64);
        visit_i128(i128); visit_u8(u8); visit_u16(u16); visit_u32(u32); visit_u64(u64);
        visit_u128(u128); visit_f32(f32); visit_f64(f64); visit_char(char);
        visit_bytes(&[u8]); visit_borrowed_bytes(&'de [u8]); visit_byte_buf(Vec<u8>);
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.v.visit_none()
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.v.visit_unit()
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        self.v.visit_some(d)
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        self.v.visit_newtype_struct(d)
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        self.v.visit_seq(seq)
    }

    fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        self.v.visit_map(map)
    }

    fn visit_enum<A: de::EnumAccess<'de>>(self, e: A) -> Result<Self::Value, A::Error> {
        self.v.visit_enum(e)
    }
}

struct WrapEnum<'a, A> {
    e: A,
    path: String,
    out: &'a RefCell<Vec<String>>,
}

impl<'de, 'a, A: de::EnumAccess<'de>> de::EnumAccess<'de> for WrapEnum<'a, A> {
    type Error = A::Error;
    type Variant = WrapVariant<'a, A::Variant>;
    fn variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<(S::Value, Self::Variant), A::Error> {
        let (val, var) = self.e.variant_seed(seed)?;
        Ok((val, WrapVariant { var, path: self.path, out: self.out }))
    }
}

struct WrapVariant<'a, A> {
    var: A,
    path: String,
    out: &'a RefCell<Vec<String>>,
}

impl<'de, 'a, A: de::VariantAccess<'de>> de::VariantAccess<'de> for WrapVariant<'a, A> {
    type Error = A::Error;
    fn unit_variant(self) -> Result<(), A::Error> {
        self.var.unit_variant()
    }
    fn newtype_variant_seed<S: DeserializeSeed<'de>>(self, seed: S) -> Result<S::Value, A::Error> {
        self.var.newtype_variant_seed(WrapSeed { seed, path: self.path, out: self.out })
    }
    fn tuple_variant<V: Visitor<'de>>(self, len: usize, v: V) -> Result<V::Value, A::Error> {
        self.var.tuple_variant(len, WrapV { v, path: self.path, out: self.out })
    }
    fn struct_variant<V: Visitor<'de>>(self, fields: &'static [&'static str], v: V) -> Result<V::Value, A::Error> {
        self.var.struct_variant(fields, WrapV { v, path: self.path, out: self.out })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Hit {
        #[serde(default)]
        bounce: f32,
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    enum Shape {
        Point,
        Ring { radius: f32 },
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Track {
        name: String,
        #[serde(default)]
        collide: Option<Hit>,
        shape: Shape,
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Effect {
        tracks: Vec<Track>,
    }

    /// A misspelt field is named, with where it is — beside known fields, in
    /// a nested struct, in an enum's struct variant — and a file with none
    /// names nothing. The file still reads: that is why nothing said so.
    #[test]
    fn a_misspelt_field_is_named_with_its_path() {
        let clean = "(tracks: [(name: \"a\", collide: Some((bounce: 0.5)), shape: Point)])";
        assert!(unknown_fields::<Effect>(clean).is_empty());
        let typos = "(tracks: [(name: \"a\", shape: Point), \
                     (name: \"b\", collde: 1, collide: Some((bonce: 0.5)), shape: Ring(radius: 1, raduis: 2))])";
        assert!(ron::from_str::<Effect>(typos).is_ok(), "an unknown field still loads");
        assert_eq!(
            unknown_fields::<Effect>(typos),
            vec!["tracks[1].collde", "tracks[1].collide.bonce", "tracks[1].shape.raduis"]
        );
        assert!(unknown_fields::<Effect>("(tracks: [(name: 3)])").is_empty(), "a file that does not parse");
    }
}
