//! Embedded cover-art extraction for audio blobs.
//!
//! An audio request (`.mp3`, `.flac`, `.m4a`, …) carries no pixels of its
//! own; the derivative served for it is its embedded album art, run through
//! the normal decode → resize → encode pipeline. [`is_audio`] gates on magic
//! bytes so both routes — and extension-less Blossom hashes — are covered by
//! one sniff at the single place every original passes through.
//!
//! Only ID3v2 (`APIC`), FLAC (`PICTURE`), and MP4 (`covr`) can embed art; a
//! bare MPEG frame stream cannot, so it is deliberately not detected and
//! fails later as an undecodable image instead.

use std::io::Cursor;

use lofty::picture::PictureType;
use lofty::prelude::*;
use lofty::probe::Probe;

use crate::error::SvcError;

/// Whether `bytes` is an audio container this service extracts cover art from.
pub(crate) fn is_audio(bytes: &[u8]) -> bool {
    if bytes.len() < 12 {
        return false;
    }
    let m4a = &bytes[4..8] == b"ftyp";
    let wav = &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE";
    for magic in [b"ID3".as_slice(), b"fLaC", b"OggS", b"MAC "] {
        if bytes.starts_with(magic) {
            return true;
        }
    }
    m4a || wav
}

/// Extract the embedded cover art image bytes from an audio file.
///
/// Prefers the front-cover picture; falls back to the first stored picture.
pub(crate) fn extract_cover_art(bytes: &[u8]) -> Result<Vec<u8>, SvcError> {
    let tagged = Probe::new(Cursor::new(bytes))
        .guess_file_type()
        .map_err(|_| SvcError::BadRequest("unrecognized audio format"))?
        .read()
        .map_err(|_| SvcError::BadRequest("malformed audio file"))?;
    let pictures = tagged
        .primary_tag()
        .or_else(|| tagged.first_tag())
        .map(|tag| tag.pictures())
        .unwrap_or(&[]);
    let best = pictures
        .iter()
        .find(|pic| pic.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first());
    match best {
        Some(pic) if !pic.data().is_empty() => Ok(pic.data().to_vec()),
        _ => Err(SvcError::BadRequest("no embedded cover art in audio file")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JPEG: &[u8] = b"\xff\xd8\xff\xe0fakejpeg";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfakepng";

    /// Build a minimal ID3v2.3 tag holding APIC frames.
    fn id3v2_with_apics(pics: &[(&[u8], u8)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (data, pic_type) in pics {
            let mut frame = Vec::new();
            frame.push(0u8); // text encoding: latin-1
            frame.extend_from_slice(b"image/jpeg\0");
            frame.push(*pic_type);
            frame.push(0); // empty description terminator
            frame.extend_from_slice(data);
            body.extend_from_slice(b"APIC");
            body.extend_from_slice(&(frame.len() as u32).to_be_bytes());
            body.extend_from_slice(&[0, 0]); // frame flags
            body.extend_from_slice(&frame);
        }
        let size = body.len() as u32;
        let syncsafe = [
            ((size >> 21) & 0x7f) as u8,
            ((size >> 14) & 0x7f) as u8,
            ((size >> 7) & 0x7f) as u8,
            (size & 0x7f) as u8,
        ];
        let mut out = Vec::new();
        out.extend_from_slice(b"ID3");
        out.extend_from_slice(&[3, 0, 0]); // version 2.3, no flags
        out.extend_from_slice(&syncsafe);
        out.extend_from_slice(&body);
        out
    }

    /// A minimal but structurally valid MPEG-1 Layer III frame (417 bytes),
    /// appended after the ID3 tag: lofty's parser requires a frame sync.
    fn mpeg_frame() -> Vec<u8> {
        let mut frame = vec![0xff, 0xfb, 0x90, 0x00];
        frame.resize(417, 0);
        frame
    }

    fn mp3_with(pics: &[(&[u8], u8)]) -> Vec<u8> {
        let mut out = id3v2_with_apics(pics);
        let frame = mpeg_frame();
        // lofty validates an MPEG frame by comparing it with the next one
        out.extend_from_slice(&frame);
        out.extend_from_slice(&frame);
        out
    }

    #[test]
    fn is_audio_detects_container_magics() {
        let mut m4a = Vec::new();
        m4a.extend_from_slice(&[0, 0, 0, 32]);
        m4a.extend_from_slice(b"ftypM4A ");
        m4a.extend_from_slice(&[0; 8]);
        for bytes in [
            mp3_with(&[]),
            b"fLaC\x00\x00\x00\x22streaminfo\x00\x00".to_vec(),
            b"OggS\x00\x02\x00\x00vorbis\x00\x00".to_vec(),
            b"RIFF\x24\x00\x00\x00WAVEfmt \x00\x00".to_vec(),
            m4a,
        ] {
            assert!(is_audio(&bytes), "{}", String::from_utf8_lossy(&bytes[..4]));
        }
        assert!(!is_audio(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"));
        assert!(!is_audio(b"\xff\xd8\xff\xe0JFIF"));
        assert!(!is_audio(b"short"));
    }

    #[test]
    fn extract_cover_art_reads_id3v2_apic() {
        let mp3 = mp3_with(&[(JPEG, 0x03)]);
        assert_eq!(extract_cover_art(&mp3).unwrap(), JPEG);
    }

    #[test]
    fn extract_cover_art_prefers_front_cover() {
        let mp3 = mp3_with(&[(PNG, 0x00), (JPEG, 0x03)]);
        assert_eq!(extract_cover_art(&mp3).unwrap(), JPEG);
    }

    #[test]
    fn extract_cover_art_without_picture_is_bad_request() {
        let mp3 = mp3_with(&[]);
        assert!(matches!(
            extract_cover_art(&mp3),
            Err(SvcError::BadRequest(_))
        ));
    }
}
