//! Bytes and text encodings: what `Buffer.from(text, 'base64')`, `.toString(…)`
//! and `atob`/`btoa` stand on.
//!
//! Four primitives, each taking or returning a plain `Uint8Array`, and a
//! bootstrap in [`super::globals`] that turns them into the `Buffer` a Node
//! script expects. The split is deliberate: text encodings are table lookups and
//! arithmetic, which Rust writes once and correctly, while the shape of
//! `Buffer` -- a `Uint8Array` subclass with a static `from`, `alloc` and
//! `concat` -- is a JavaScript idea and is written as JavaScript.
//!
//! Case is not significant here and aliases are the bootstrap's business: by the
//! time a primitive is called, the encoding is one of the seven names below, and
//! anything else is refused rather than guessed at.

use base64::engine::general_purpose::{GeneralPurpose, STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use rquickjs::{Ctx, TypedArray, Value};

use super::{native, refuse};

/// Installs the byte primitives under the names the bootstrap picks up.
pub(super) fn install<'js>(ctx: &Ctx<'js>) -> anyhow::Result<()> {
    native(ctx, "__bytesFromString", bytes_from_string)?;
    native(ctx, "__bytesToString", bytes_to_string)?;
    native(ctx, "__b64Encode", b64_encode)?;
    native(ctx, "__b64Decode", b64_decode)
}

/// The encoding names every primitive accepts, after the bootstrap has mapped
/// the aliases Node also accepts onto them.
const UTF8: &str = "utf8";
const UTF16LE: &str = "utf16le";

/// `Buffer.from(text, encoding)`: text to bytes.
fn bytes_from_string<'js>(
    ctx: Ctx<'js>,
    text: String,
    encoding: String,
) -> rquickjs::Result<TypedArray<'js, u8>> {
    let bytes = match encoding.as_str() {
        UTF8 => text.into_bytes(),
        "hex" => hex::decode(&text)
            .map_err(|e| refuse(&ctx, &format!("the hexadecimal string is not valid: {e}")))?,
        "base64" => decode(&ctx, &text, STANDARD, STANDARD_NO_PAD)?,
        "base64url" => decode(&ctx, &text, URL_SAFE, URL_SAFE_NO_PAD)?,
        // Node's latin1 and ascii keep the low bits and drop the rest rather
        // than refusing the text, which is what makes them usable for binary
        // data that arrived as a string.
        "latin1" => text.chars().map(|c| (c as u32 & 0xff) as u8).collect(),
        "ascii" => text.chars().map(|c| (c as u32 & 0x7f) as u8).collect(),
        UTF16LE => text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        other => return Err(refuse(&ctx, &format!("Unknown encoding: {other}"))),
    };
    TypedArray::new(ctx, bytes)
}

/// `buffer.toString(encoding)`: bytes to text.
fn bytes_to_string<'js>(ctx: Ctx<'js>, value: Value<'js>, encoding: String) -> rquickjs::Result<String> {
    let bytes = bytes_of(&ctx, &value)?;
    Ok(match encoding.as_str() {
        // Lossy on purpose: Node replaces what is not valid UTF-8 and so does
        // this, rather than failing a notification over a stray byte.
        UTF8 => String::from_utf8_lossy(&bytes).into_owned(),
        "hex" => hex::encode(&bytes),
        "base64" => STANDARD.encode(&bytes),
        // Node's base64url is the unpadded spelling, in the same way a URL
        // would carry it.
        "base64url" => URL_SAFE_NO_PAD.encode(&bytes),
        "latin1" => bytes.iter().map(|b| char::from(*b)).collect(),
        "ascii" => bytes.iter().map(|b| char::from(b & 0x7f)).collect(),
        // An odd trailing byte is dropped rather than refused, which is what
        // Node does with a string that was cut in half.
        UTF16LE => String::from_utf16_lossy(
            &bytes.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair)).collect::<Vec<_>>(),
        ),
        other => return Err(refuse(&ctx, &format!("Unknown encoding: {other}"))),
    })
}

/// `btoa(text)`: the bytes of the bootstrap's binary string, as base64.
fn b64_encode<'js>(ctx: Ctx<'js>, value: Value<'js>) -> rquickjs::Result<String> {
    Ok(STANDARD.encode(bytes_of(&ctx, &value)?))
}

/// `atob(text)`: base64 to the bytes of a binary string.
fn b64_decode<'js>(ctx: Ctx<'js>, text: String) -> rquickjs::Result<TypedArray<'js, u8>> {
    // Both spellings, because `atob` in a browser accepts either and the padded
    // form is the one a program is most likely to have copied from somewhere.
    let bytes = decode(&ctx, &text, STANDARD, STANDARD_NO_PAD)
        .or_else(|_| decode(&ctx, &text, URL_SAFE, URL_SAFE_NO_PAD))?;
    TypedArray::new(ctx, bytes)
}

/// Base64 in one of its two spellings, padded or bare.
///
/// Two engines rather than one because padding is optional in what arrives --
/// a URL-safe token usually has none -- and requiring it would refuse strings
/// Node accepts.
fn decode(
    ctx: &Ctx<'_>,
    text: &str,
    padded: GeneralPurpose,
    bare: GeneralPurpose,
) -> rquickjs::Result<Vec<u8>> {
    padded
        .decode(text)
        .or_else(|_| bare.decode(text))
        .map_err(|e| refuse(ctx, &format!("the base64 string is not valid: {e}")))
}

/// The bytes behind a `Buffer` or `Uint8Array` the bootstrap handed over.
///
/// A copy rather than a borrow. Everything reaching these primitives has been
/// normalized to a plain `Uint8Array` by the bootstrap, so there is no view to
/// keep alive, and a copy keeps the borrow from reaching back into the engine.
pub(super) fn bytes_of(ctx: &Ctx<'_>, value: &Value<'_>) -> rquickjs::Result<Vec<u8>> {
    value
        .as_object()
        .and_then(|object| object.as_typed_array::<u8>())
        .and_then(|array| array.as_bytes())
        .map(<[u8]>::to_vec)
        .ok_or_else(|| refuse(ctx, "the value must be a Buffer or a Uint8Array"))
}
