//! Payload layer: everything between an MCPK file body and a raw marshal stream.
//!
//! Historical builds wrap the marshalled module in a rotor pass, then a zlib
//! stream, then a byte-reversal obfuscation. Newer/older builds differ, so the
//! decoder is written as a list of candidate transforms that are tried in
//! order and validated by the marshal parser instead of by hard-coded ids.

use crate::rotor::Rotor;
use anyhow::{Context, Result};
use std::io::Read;

/// Number of leading bytes flipped with [`XOR_BYTE`] before the whole buffer
/// is reversed (see `reverseData` in the legacy tool).
pub const HEAD_XOR_LEN: usize = 130;
pub const XOR_BYTE: u8 = 0x9c;

/// Tail marker every payload produced by the known builds carries.
pub const TRAILER_MARKER: [u8; 2] = [0xFF, 0xFE];

/// The obfuscation applied on top of the zlib stream, in the order the
/// encryption path applied them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    /// rotor -> zlib -> head xor + reverse (standard since at least 2020).
    RotorZlibReverse,
    /// "MCPK" xor on the first four bytes -> zlib (redirect.mcs and friends).
    MagicXorZlib,
    /// Plain zlib, no obfuscation.
    ZlibOnly,
    /// rotor -> zlib without the reversal step.
    RotorZlib,
}

impl LayerKind {
    pub const ALL: [LayerKind; 4] = [
        LayerKind::RotorZlibReverse,
        LayerKind::MagicXorZlib,
        LayerKind::ZlibOnly,
        LayerKind::RotorZlib,
    ];

    pub fn label(self) -> &'static str {
        match self {
            LayerKind::RotorZlibReverse => "rotor+zlib+reverse",
            LayerKind::MagicXorZlib => "magic-xor+zlib",
            LayerKind::ZlibOnly => "zlib",
            LayerKind::RotorZlib => "rotor+zlib",
        }
    }
}

/// Undo (or apply) the head-xor + full reversal step. The transform is its own
/// inverse, which is why one function serves both directions.
pub fn reverse_data(data: &mut [u8]) {
    let head = HEAD_XOR_LEN.min(data.len());
    for b in &mut data[..head] {
        *b ^= XOR_BYTE;
    }
    data.reverse();
}

fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 4);
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .context("zlib inflate failed")?;
    Ok(out)
}

/// Apply one candidate transform. Returns `None` when the stream does not
/// inflate, i.e. the candidate does not apply to this file.
pub fn apply_layer(kind: LayerKind, raw: &[u8], rotor: &mut Rotor) -> Option<Vec<u8>> {
    let mut buf = raw.to_vec();
    match kind {
        LayerKind::RotorZlibReverse => {
            rotor.d_region(&mut buf);
            let mut out = inflate(&buf).ok()?;
            reverse_data(&mut out);
            Some(out)
        }
        LayerKind::MagicXorZlib => {
            if buf.len() < 4 {
                return None;
            }
            for (b, m) in buf[..4].iter_mut().zip(b"MCPK") {
                *b ^= *m;
            }
            inflate(&buf).ok()
        }
        LayerKind::ZlibOnly => inflate(&buf).ok(),
        LayerKind::RotorZlib => {
            rotor.d_region(&mut buf);
            inflate(&buf).ok()
        }
    }
}

/// What followed the marshal stream inside the decoded payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trailer {
    /// Bytes left over after the marshal object.
    pub len: usize,
    /// Whether the payload ended with [`TRAILER_MARKER`]; when it did, the last
    /// two overrun bytes are the marker and `len - 2` bytes are filler.
    pub marker: bool,
}

impl Trailer {
    pub fn detect(payload: &[u8], consumed: usize) -> Trailer {
        let len = payload.len().saturating_sub(consumed);
        let marker = payload.len() >= 2 && payload[payload.len() - 2..] == TRAILER_MARKER;
        Trailer { len, marker }
    }

    pub fn filler_len(&self) -> usize {
        if self.marker {
            self.len.saturating_sub(2)
        } else {
            self.len
        }
    }
}

/// `StringID` / `_Z8StringIDPKc`: hashes a name into the 32-bit ids the
/// container and the pyc file reference use. Only needed to look names up in
/// either direction — decoding never depends on it.
pub fn string_id(input: &str) -> u32 {
    let bytes = input.as_bytes();
    let words = bytes.len().div_ceil(4);
    let mut m = vec![0u32; words + 2];
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let mut w = [0u8; 4];
        w[..chunk.len()].copy_from_slice(chunk);
        m[i] = u32::from_le_bytes(w);
    }
    m[words] = 0x9BE7_4448;
    m[words + 1] = 0x66F4_2C48;

    let mut v: u32 = 0xF4FA_8928;
    let mut edi: u32 = 0x7758_B42B;
    let mut esi: u32 = 0x37A8_470E;

    for &word in m.iter() {
        let mut ebx: u32 = 0x267B_0B11;
        v = v.rotate_left(1);
        ebx ^= v;
        esi ^= word;
        edi ^= word;

        let mut edx = ebx.wrapping_add(edi);
        edx |= 0x0204_0801;
        edx &= 0xBFEF_7FDF;
        let num = (edx as u64) * (esi as u64);
        let mut eax = num as u32;
        let hi = (num >> 32) as u32;
        if hi != 0 {
            eax = eax.wrapping_add(1);
        }
        let num = eax as u64 + hi as u64;
        eax = num as u32;
        if (num >> 32) != 0 {
            eax = eax.wrapping_add(1);
        }

        let mut edx = ebx.wrapping_add(esi);
        edx |= 0x0080_4021;
        edx &= 0x7DFE_FBFF;
        esi = eax;
        let num = (edi as u64) * (edx as u64);
        let mut eax = num as u32;
        let hi = (num >> 32) as u32;
        let num = hi as u64 + hi as u64;
        let edx = num as u32;
        if (num >> 32) != 0 {
            eax = eax.wrapping_add(1);
        }
        let num = eax as u64 + edx as u64;
        let mut eax = num as u32;
        if (num >> 32) != 0 {
            eax = eax.wrapping_add(2);
        }
        let _ = &mut eax;
        edi = eax;
    }
    esi ^ edi
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_roundtrip() {
        // Encoding is "reverse, then xor the first HEAD_XOR_LEN bytes"; decoding
        // is the mirror image, so the pair must roundtrip (the function itself
        // is not an involution).
        let orig: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let mut enc = orig.clone();
        enc.reverse();
        for b in enc.iter_mut().take(HEAD_XOR_LEN) {
            *b ^= XOR_BYTE;
        }
        let mut dec = enc.clone();
        reverse_data(&mut dec);
        assert_eq!(dec, orig);
        assert_ne!(enc, orig);
    }

    #[test]
    fn string_id_matches_reference() {
        // Known values from the shipped containers.
        assert_eq!(string_id("redirect.mcs"), 0x8d7b_fdc9);
    }

    #[test]
    fn trailer_detection() {
        let mut payload = vec![1u8, 2, 3];
        payload.extend_from_slice(&vec![0u8; 130]);
        payload.extend_from_slice(&TRAILER_MARKER);
        let t = Trailer::detect(&payload, 3);
        assert_eq!(t.len, 132);
        assert!(t.marker);
        assert_eq!(t.filler_len(), 130);
    }
}
