//! Byte helpers a game needs to keep a picture or a blob small and to carry
//! it as text.
//!
//! ```lua
//! local packed = data.deflate(bytes [, { level = 6, format = "zlib"|"raw"|"gzip" }])
//! local bytes, err = data.inflate(packed [, { format = "zlib", maxSize = 64 MiB }])
//! local text = data.base64Encode(bytes [, { url = true }])
//! local bytes, err = data.base64Decode(text)
//! ```
//!
//! Every input is a Lua string holding raw bytes. `inflate` and
//! `base64Decode` read what a player or a server handed over, so they answer
//! `nil, err` for bad input rather than raising, and `inflate` stops at
//! `maxSize` so a small hostile blob cannot expand to fill memory.

use std::io::{Read as _, Write as _};

use base64::Engine as _;
use mlua::{Lua, Table, Value};

/// What `data.inflate` will expand to unless told otherwise.
pub const DEFAULT_MAX_INFLATE: usize = 64 * 1024 * 1024;

fn format_of(call: &str, opts: Option<&Table>) -> mlua::Result<&'static str> {
    let Some(o) = opts else { return Ok("zlib") };
    match o.get::<Option<String>>("format")?.as_deref() {
        None | Some("zlib") => Ok("zlib"),
        Some("raw") => Ok("raw"),
        Some("gzip") => Ok("gzip"),
        Some(other) => Err(mlua::Error::RuntimeError(format!(
            "{call}: options.format must be \"zlib\", \"raw\" or \"gzip\" (got \"{other}\")"
        ))),
    }
}

fn check_keys(call: &str, opts: Option<&Table>, known: &[&str]) -> mlua::Result<()> {
    let Some(o) = opts else { return Ok(()) };
    for pair in o.pairs::<Value, Value>() {
        let (k, _) = pair?;
        let name = match &k {
            Value::String(s) => s.to_string_lossy().to_string(),
            other => format!("[{}]", other.type_name()),
        };
        if !known.contains(&name.as_str()) {
            return Err(mlua::Error::RuntimeError(format!(
                "{call}: unknown option \"{name}\" — it reads {}",
                known.join(", ")
            )));
        }
    }
    Ok(())
}

/// Compress `bytes`. Pure, so it is exposed for tests and for Rust callers.
pub fn deflate(bytes: &[u8], level: u32, format: &str) -> Result<Vec<u8>, String> {
    let lvl = flate2::Compression::new(level.min(9));
    let io = |e: std::io::Error| e.to_string();
    match format {
        "zlib" => {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), lvl);
            e.write_all(bytes).map_err(io)?;
            e.finish().map_err(io)
        }
        "raw" => {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), lvl);
            e.write_all(bytes).map_err(io)?;
            e.finish().map_err(io)
        }
        "gzip" => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), lvl);
            e.write_all(bytes).map_err(io)?;
            e.finish().map_err(io)
        }
        other => Err(format!("unknown format \"{other}\"")),
    }
}

/// Expand `bytes`, refusing past `max` bytes of output.
pub fn inflate(bytes: &[u8], format: &str, max: usize) -> Result<Vec<u8>, String> {
    let reader: Box<dyn std::io::Read + '_> = match format {
        "zlib" => Box::new(flate2::read::ZlibDecoder::new(bytes)),
        "raw" => Box::new(flate2::read::DeflateDecoder::new(bytes)),
        "gzip" => Box::new(flate2::read::GzDecoder::new(bytes)),
        other => return Err(format!("unknown format \"{other}\"")),
    };
    let mut out = Vec::new();
    // One byte past the limit is enough to know it was crossed.
    let read = reader.take(max as u64 + 1).read_to_end(&mut out);
    match read {
        Err(e) => Err(format!("not valid {format} data ({e})")),
        Ok(_) if out.len() > max => Err(format!(
            "the data expands past {max} bytes — pass a larger options.maxSize if that is expected"
        )),
        Ok(_) => Ok(out),
    }
}

/// Encode as base64; `url` picks the URL-safe alphabet without padding.
pub fn base64_encode(bytes: &[u8], url: bool) -> String {
    if url {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    } else {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}

/// Decode either alphabet, padded or not, ignoring whitespace (a line-wrapped
/// block from a file or a server is still base64).
pub fn base64_decode(text: &[u8]) -> Result<Vec<u8>, String> {
    use base64::engine::{GeneralPurpose, GeneralPurposeConfig, DecodePaddingMode};
    let clean: Vec<u8> = text.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
    let cfg = GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent);
    let standard = GeneralPurpose::new(&base64::alphabet::STANDARD, cfg);
    let url = GeneralPurpose::new(&base64::alphabet::URL_SAFE, cfg);
    standard
        .decode(&clean)
        .or_else(|_| url.decode(&clean))
        .map_err(|e| format!("not base64 ({e})"))
}

fn fail(lua: &Lua, why: String) -> mlua::Result<(Value, Value)> {
    Ok((Value::Nil, Value::String(lua.create_string(why)?)))
}

pub(crate) fn install(lua: &Lua) -> mlua::Result<()> {
    let t = lua.create_table()?;

    t.set(
        "deflate",
        lua.create_function(|lua, (bytes, opts): (mlua::String, Option<Table>)| {
            const CALL: &str = "data.deflate";
            check_keys(CALL, opts.as_ref(), &["level", "format"])?;
            let format = format_of(CALL, opts.as_ref())?;
            let level = match opts.as_ref().map(|o| o.get::<Option<f64>>("level")).transpose()?.flatten() {
                None => 6,
                Some(l) if l.is_finite() && (0.0..=9.0).contains(&l) => l.round() as u32,
                Some(l) => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "{CALL}: options.level must be 0 (store) to 9 (smallest) (got {l})"
                    )));
                }
            };
            let out = deflate(&bytes.as_bytes(), level, format).map_err(mlua::Error::RuntimeError)?;
            lua.create_string(&out)
        })?,
    )?;

    t.set(
        "inflate",
        lua.create_function(|lua, (bytes, opts): (mlua::String, Option<Table>)| {
            const CALL: &str = "data.inflate";
            check_keys(CALL, opts.as_ref(), &["format", "maxSize"])?;
            let format = format_of(CALL, opts.as_ref())?;
            let max = match opts.as_ref().map(|o| o.get::<Option<f64>>("maxSize")).transpose()?.flatten() {
                None => DEFAULT_MAX_INFLATE,
                Some(m) if m.is_finite() && m >= 0.0 => m as usize,
                Some(m) => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "{CALL}: options.maxSize must be a byte count (got {m})"
                    )));
                }
            };
            match inflate(&bytes.as_bytes(), format, max) {
                Ok(out) => Ok((Value::String(lua.create_string(&out)?), Value::Nil)),
                Err(why) => fail(lua, why),
            }
        })?,
    )?;

    t.set(
        "base64Encode",
        lua.create_function(|lua, (bytes, opts): (mlua::String, Option<Table>)| {
            check_keys("data.base64Encode", opts.as_ref(), &["url"])?;
            let url = opts.as_ref().map(|o| o.get::<Option<bool>>("url")).transpose()?.flatten().unwrap_or(false);
            lua.create_string(base64_encode(&bytes.as_bytes(), url))
        })?,
    )?;

    t.set(
        "base64Decode",
        lua.create_function(|lua, text: mlua::String| match base64_decode(&text.as_bytes()) {
            Ok(out) => Ok((Value::String(lua.create_string(&out)?), Value::Nil)),
            Err(why) => fail(lua, why),
        })?,
    )?;

    lua.globals().set("data", t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_format_round_trips_and_shrinks_repetitive_bytes() {
        let bytes: Vec<u8> = (0..20_000u32).map(|i| (i % 7) as u8).collect();
        for f in ["zlib", "raw", "gzip"] {
            let packed = deflate(&bytes, 6, f).unwrap();
            assert!(packed.len() < bytes.len() / 10, "{f}: {} bytes", packed.len());
            assert_eq!(inflate(&packed, f, DEFAULT_MAX_INFLATE).unwrap(), bytes, "{f}");
        }
    }

    #[test]
    fn inflate_stops_at_the_size_limit_and_refuses_garbage() {
        let packed = deflate(&vec![0u8; 100_000], 9, "zlib").unwrap();
        assert!(inflate(&packed, "zlib", 99_999).unwrap_err().contains("expands past"));
        assert_eq!(inflate(&packed, "zlib", 100_000).unwrap().len(), 100_000);
        assert!(inflate(b"not deflate at all", "zlib", 1000).is_err());
    }

    #[test]
    fn base64_reads_both_alphabets_with_or_without_padding() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let std = base64_encode(&bytes, false);
        let url = base64_encode(&bytes, true);
        assert!(std.contains('+') && std.ends_with('='));
        assert!(url.contains('-') && !url.ends_with('='));
        assert_eq!(base64_decode(std.as_bytes()).unwrap(), bytes);
        assert_eq!(base64_decode(url.as_bytes()).unwrap(), bytes);
        let wrapped: String = std.as_bytes().chunks(76).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("\n");
        assert_eq!(base64_decode(wrapped.as_bytes()).unwrap(), bytes);
        assert!(base64_decode(b"!!!").is_err());
    }
}
