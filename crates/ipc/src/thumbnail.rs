// SPDX-License-Identifier: EUPL-1.2
//! The shape of the PNG thumbnails the content worker makes from untrusted
//! artwork. Callers check every PNG with [`check_png`] before it reaches a
//! browser or a disk cache. The check reads the structure only and never
//! inflates image data, so it stays cheap and holds no decoder.

use crate::{MAX_THUMBNAIL_EDGE as MAX_EDGE, MAX_THUMBNAIL_PNG_BYTES as MAX_PNG_BYTES};

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// `(width, height)` when `png` is a thumbnail as the worker makes them: the
/// PNG signature, one `IHDR` for 8-bit RGBA without interlacing and at most
/// `max_width` × `max_height` (and [`MAX_EDGE`]), one or more `IDAT`, one
/// empty `IEND` and nothing after it, every chunk with a correct CRC. Any
/// other chunk, metadata included, rejects the image.
pub fn check_png(png: &[u8], max_width: u32, max_height: u32) -> Option<(u32, u32)> {
    if png.len() > MAX_PNG_BYTES {
        return None;
    }
    let mut rest = png.strip_prefix(&SIGNATURE)?;
    let mut size = None;
    let mut data = false;
    loop {
        let (length, tail) = rest.split_first_chunk::<4>()?;
        let length = u32::from_be_bytes(*length) as usize;
        if tail.len() < length.checked_add(8)? {
            return None;
        }
        let (kind_and_body, tail) = tail.split_at(4 + length);
        let (crc, tail) = tail.split_first_chunk::<4>()?;
        if crc32(kind_and_body) != u32::from_be_bytes(*crc) {
            return None;
        }
        let (kind, body) = kind_and_body.split_at(4);
        rest = tail;
        match (kind, size) {
            (b"IHDR", None) => size = Some(header(body, max_width, max_height)?),
            (b"IDAT", Some(_)) => data = true,
            (b"IEND", Some(found)) if data && body.is_empty() && rest.is_empty() => {
                return Some(found);
            }
            _ => return None,
        }
    }
}

/// Width and height from an `IHDR` body for 8-bit RGBA, deflate, adaptive
/// filtering and no interlacing.
fn header(body: &[u8], max_width: u32, max_height: u32) -> Option<(u32, u32)> {
    let body: &[u8; 13] = body.try_into().ok()?;
    let width = u32::from_be_bytes(body[0..4].try_into().ok()?);
    let height = u32::from_be_bytes(body[4..8].try_into().ok()?);
    let limit = |edge: u32, max: u32| (1..=max.min(MAX_EDGE)).contains(&edge);
    (limit(width, max_width) && limit(height, max_height) && body[8..] == [8, 6, 0, 0, 0])
        .then_some((width, height))
}

/// CRC-32 as PNG uses it (ISO 3309, reflected, polynomial 0xEDB88320).
fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |crc, &byte| {
        (0..8).fold(crc ^ byte as u32, |crc, _| {
            (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = (body.len() as u32).to_be_bytes().to_vec();
        let mut checked = kind.to_vec();
        checked.extend_from_slice(body);
        out.extend_from_slice(&checked);
        out.extend_from_slice(&crc32(&checked).to_be_bytes());
        out
    }

    fn ihdr(width: u32, height: u32, depth: u8, color: u8) -> Vec<u8> {
        let mut body = width.to_be_bytes().to_vec();
        body.extend_from_slice(&height.to_be_bytes());
        body.extend_from_slice(&[depth, color, 0, 0, 0]);
        chunk(b"IHDR", &body)
    }

    fn png(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        for c in chunks {
            out.extend_from_slice(c);
        }
        out
    }

    #[test]
    fn crc_matches_the_png_specification() {
        // The CRC of an empty IEND chunk, as every PNG ends.
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn accepts_only_bare_rgba_thumbnails_within_bounds() {
        let idat = chunk(b"IDAT", b"\x78\x9c\x63\x60\x00\x00\x00\x02\x00\x01");
        let iend = chunk(b"IEND", b"");
        let good = png(&[ihdr(2, 3, 8, 6), idat.clone(), idat.clone(), iend.clone()]);
        assert_eq!(check_png(&good, 400, 400), Some((2, 3)));
        assert_eq!(check_png(&good, 2, 2), None, "taller than asked");

        let text = chunk(b"tEXt", b"Comment\0from the source");
        let mut bad_crc = good.clone();
        let at = good.len() - 1;
        bad_crc[at] ^= 1;
        let mut trailing = good.clone();
        trailing.push(0);
        for case in [
            png(&[ihdr(2, 3, 8, 6), text, idat.clone(), iend.clone()]),
            png(&[ihdr(2, 3, 8, 2), idat.clone(), iend.clone()]),
            png(&[ihdr(2, 3, 16, 6), idat.clone(), iend.clone()]),
            png(&[ihdr(0, 3, 8, 6), idat.clone(), iend.clone()]),
            png(&[ihdr(401, 1, 8, 6), idat.clone(), iend.clone()]),
            png(&[ihdr(2, 3, 8, 6), iend.clone()]),
            png(&[idat.clone(), ihdr(2, 3, 8, 6), iend.clone()]),
            png(&[
                ihdr(2, 3, 8, 6),
                ihdr(2, 3, 8, 6),
                idat.clone(),
                iend.clone(),
            ]),
            png(&[ihdr(2, 3, 8, 6), idat.clone()]),
            bad_crc,
            trailing,
            good[1..].to_vec(),
            good[..good.len() - 4].to_vec(),
        ] {
            assert_eq!(check_png(&case, 400, 400), None);
        }
        let mut huge = good.clone();
        huge.resize(MAX_PNG_BYTES + 1, 0);
        assert_eq!(check_png(&huge, 400, 400), None);
    }
}
