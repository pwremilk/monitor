//! `crypto`: random values, digests and an HMAC, for a script that has to sign
//! or identify what it sends.
//!
//! The API is Node's, because that is what the reference implementation's
//! scripts are written against: `createHash('sha256').update(x).digest('hex')`
//! and `createHmac`, beside `randomUUID` and `getRandomValues`.
//!
//! A message is accumulated and hashed once, at `digest()`. That is what the
//! reference implementation does, and it keeps `update` chainable without a
//! boxed digest state per call: the buffer is bounded by the engine's memory
//! limit, and a notification is measured in kilobytes.

use std::cell::RefCell;

use rand::RngCore;
use rquickjs::{Ctx, TypedArray, Value};
// Both crates re-export the same `digest` trait, so one import covers the
// SHA-1 and the SHA-2 family alike.
use sha1::Digest;

use super::binary::bytes_of;
use super::{native, refuse};

/// The most `randomBytes` will fill in one call.
///
/// `getRandomValues` has its own quota of 65536 bytes, which the bootstrap
/// enforces; this is the ceiling the primitive itself will not go past, so a
/// mistake in the bootstrap cannot ask the hub for a gigabyte of randomness.
const MOST_RANDOM: f64 = 1024.0 * 1024.0;

/// The digests a script may ask for.
#[derive(Clone, Copy)]
enum Algorithm {
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

impl Algorithm {
    /// The name Node would accept, mapped onto the one algorithm it means.
    ///
    /// Node's table is exact -- `sha256` works and `sha-256` does not -- and the
    /// reference implementation normalises only the slash in `sha512/256`. Both
    /// spellings are taken here, in either case, and nothing else: a digest
    /// silently computed with a different algorithm than the one asked for would
    /// be worse than a refusal.
    fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().replace('/', "-").as_str() {
            "sha1" => Some(Self::Sha1),
            "sha224" => Some(Self::Sha224),
            "sha256" => Some(Self::Sha256),
            "sha384" => Some(Self::Sha384),
            "sha512" => Some(Self::Sha512),
            _ => None,
        }
    }

    fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Self::Sha1 => sha1::Sha1::digest(data).to_vec(),
            Self::Sha224 => sha2::Sha224::digest(data).to_vec(),
            Self::Sha256 => sha2::Sha256::digest(data).to_vec(),
            Self::Sha384 => sha2::Sha384::digest(data).to_vec(),
            Self::Sha512 => sha2::Sha512::digest(data).to_vec(),
        }
    }

    /// The block size HMAC pads its key to, per FIPS 198-1: 64 bytes for the
    /// SHA-1 and SHA-256 family, 128 for the SHA-384/512 one.
    fn block(self) -> usize {
        match self {
            Self::Sha1 | Self::Sha224 | Self::Sha256 => 64,
            Self::Sha384 | Self::Sha512 => 128,
        }
    }
}

/// The hashes one evaluation has open, keyed by the id a script's object holds.
///
/// Host data rather than a Rust object on the call path: the natives are plain
/// function pointers, and this is the only thing they can borrow. It is dropped
/// with the engine, so one event's half-finished digest never reaches the next.
#[derive(Default)]
pub(super) struct Hashes {
    /// Indexed by `id - 1`; a slot is `None` once its digest has been taken, so
    /// the next `createHash` can reuse it.
    slots: RefCell<Vec<Option<Hasher>>>,
}

/// One hash or HMAC in progress.
struct Hasher {
    algorithm: Algorithm,
    /// `Some` for an HMAC, whose key is padded and folded in at the end.
    key: Option<Vec<u8>>,
    message: Vec<u8>,
}

impl Hashes {
    fn insert(&self, hasher: Hasher) -> u32 {
        let mut slots = self.slots.borrow_mut();
        let index = match slots.iter().position(Option::is_none) {
            Some(index) => {
                slots[index] = Some(hasher);
                index
            }
            None => {
                slots.push(Some(hasher));
                slots.len() - 1
            }
        };
        // Ids start at one so that zero can never name a slot.
        index as u32 + 1
    }

    fn update(&self, id: u32, data: &[u8]) -> bool {
        let mut slots = self.slots.borrow_mut();
        let Some(Some(hasher)) = slots.get_mut(id.wrapping_sub(1) as usize) else { return false };
        hasher.message.extend_from_slice(data);
        true
    }

    fn take(&self, id: u32) -> Option<Hasher> {
        self.slots.borrow_mut().get_mut(id.wrapping_sub(1) as usize)?.take()
    }
}

impl Hasher {
    fn finish(&self) -> Vec<u8> {
        match &self.key {
            None => self.algorithm.digest(&self.message),
            Some(key) => hmac(self.algorithm, key, &self.message),
        }
    }
}

/// HMAC, per FIPS 198-1.
///
/// Written out rather than pulled in: it is two XOR-padded copies of the key
/// around the digest the crate already provides, and the alternative is another
/// package in the lock for the one construction a webhook signature needs.
fn hmac(algorithm: Algorithm, key: &[u8], message: &[u8]) -> Vec<u8> {
    let block = algorithm.block();
    // A key longer than the block is hashed down to one first, per the standard.
    let mut padded = if key.len() > block { algorithm.digest(key) } else { key.to_vec() };
    padded.resize(block, 0);
    let inner: Vec<u8> = padded.iter().map(|b| b ^ 0x36).chain(message.iter().copied()).collect();
    let inner = algorithm.digest(&inner);
    let outer: Vec<u8> = padded.iter().map(|b| b ^ 0x5c).chain(inner).collect();
    algorithm.digest(&outer)
}

// SAFETY: the table has no `'js` lifetime of its own -- ids, byte buffers and
// algorithm names -- so there is nothing in it for `Changed` to rewrite.
unsafe impl<'js> rquickjs::JsLifetime<'js> for Hashes {
    type Changed<'to> = Hashes;
}

/// Installs the crypto primitives under the names the bootstrap picks up.
pub(super) fn install<'js>(ctx: &Ctx<'js>) -> anyhow::Result<()> {
    native(ctx, "__randomUUID", random_uuid)?;
    native(ctx, "__randomBytes", random_bytes)?;
    native(ctx, "__hashNew", hash_new)?;
    native(ctx, "__hashUpdate", hash_update)?;
    native(ctx, "__hashDigest", hash_digest)?;
    native(ctx, "__digestBytes", digest_bytes)
}

/// `crypto.randomUUID()`: a version 4 UUID, shaped the way RFC 4122 asks for.
fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    // The version and variant bits, so the string is a UUID an endpoint will
    // accept rather than sixteen random bytes wearing one's punctuation.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex = hex::encode(bytes);
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// The bytes behind `crypto.getRandomValues`, which fills the array itself.
fn random_bytes<'js>(ctx: Ctx<'js>, length: f64) -> rquickjs::Result<TypedArray<'js, u8>> {
    if !length.is_finite() || !(0.0..=MOST_RANDOM).contains(&length) {
        return Err(refuse(&ctx, "the number of random bytes must be between 0 and 1048576"));
    }
    let mut bytes = vec![0u8; length as usize];
    rand::rng().fill_bytes(&mut bytes);
    TypedArray::new(ctx, bytes)
}

/// `createHash(algorithm)` and `createHmac(algorithm, key)`: one opened hash,
/// named by the id the script's object carries.
fn hash_new<'js>(ctx: Ctx<'js>, algorithm: String, key: Value<'js>) -> rquickjs::Result<u32> {
    let algorithm =
        Algorithm::parse(&algorithm).ok_or_else(|| refuse(&ctx, "Digest method not supported"))?;
    let key = if key.is_undefined() || key.is_null() { None } else { Some(bytes_of(&ctx, &key)?) };
    let Some(hashes) = ctx.userdata::<Hashes>() else {
        return Err(refuse(&ctx, "this context has no digest table"));
    };
    Ok(hashes.insert(Hasher { algorithm, key, message: Vec::new() }))
}

/// `hash.update(data)`: appends to the message.
fn hash_update<'js>(ctx: Ctx<'js>, id: u32, data: Value<'js>) -> rquickjs::Result<()> {
    let bytes = bytes_of(&ctx, &data)?;
    let Some(hashes) = ctx.userdata::<Hashes>() else {
        return Err(refuse(&ctx, "this context has no digest table"));
    };
    if !hashes.update(id, &bytes) {
        return Err(refuse(&ctx, "the digest has already been taken"));
    }
    Ok(())
}

/// `hash.digest()`: the digest of what was accumulated, as hex.
///
/// Hex because it is the one spelling that needs no second representation: the
/// bootstrap turns it into the `Buffer` or the text encoding the script asked
/// for, and every other encoding is then a path that is already tested.
fn hash_digest<'js>(ctx: Ctx<'js>, id: u32) -> rquickjs::Result<String> {
    let Some(hashes) = ctx.userdata::<Hashes>() else {
        return Err(refuse(&ctx, "this context has no digest table"));
    };
    // Taken rather than read: a second `digest()` on the same object is a
    // mistake Node refuses, and taking the state frees the buffer with it.
    let Some(hasher) = hashes.take(id) else {
        return Err(refuse(&ctx, "the digest has already been taken"));
    };
    Ok(hex::encode(hasher.finish()))
}

/// `crypto.subtle.digest(algorithm, data)`: the whole digest in one call.
fn digest_bytes<'js>(
    ctx: Ctx<'js>,
    algorithm: String,
    data: Value<'js>,
) -> rquickjs::Result<TypedArray<'js, u8>> {
    let algorithm =
        Algorithm::parse(&algorithm).ok_or_else(|| refuse(&ctx, "Digest method not supported"))?;
    let bytes = bytes_of(&ctx, &data)?;
    TypedArray::new(ctx, algorithm.digest(&bytes))
}
