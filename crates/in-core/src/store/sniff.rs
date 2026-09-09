//! What uploaded bytes are, decided from the bytes themselves — never from a
//! part's `content_type()`, which is whatever the browser felt like sending.
//!
//! Pure `bytes -> mime`, with no I/O and nothing browser-shaped in it, living
//! behind the same `server` gate as the store it serves. The upload path
//! sniffs a file once when it arrives, and the store cannot reach the web
//! crate, so this one definition serves every caller — a second sniffer
//! would drift.

pub fn sniff(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"%PDF-") {
        "application/pdf"
    } else if bytes.starts_with(b"PK\x03\x04") {
        // Every OOXML and OpenDocument file is a zip; what kind it is lives in
        // the entry names, which a zip stores uncompressed. `xl/workbook.xml`
        // is the one part an xlsx cannot be without, an ods declares its
        // type in the `mimetype` entry the format requires be stored first,
        // and `ppt/presentation.xml` is the one part a pptx cannot be
        // without.
        zip_mime(bytes)
    } else if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        // The pre-2007 OLE compound file .xls, .doc and .ppt all share. Its
        // directory holds stream names in UTF-16, and only a workbook carries
        // one called `Workbook` — a .doc or .ppt stays an unnamed container.
        if contains(bytes, b"W\0o\0r\0k\0b\0o\0o\0k\0") {
            "application/vnd.ms-excel"
        } else {
            "application/x-ole-storage"
        }
    } else if bytes.starts_with(&[0x1F, 0x8B]) {
        "application/gzip"
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && &bytes[8..12] == b"avif" {
        "image/avif"
    } else if bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(
            &bytes[8..12],
            b"heic" | b"heix" | b"hevc" | b"heif" | b"mif1" | b"msf1"
        )
    {
        // Apple's HEIC container reuses the ISO-BMFF `ftyp` box video shares;
        // the brand at bytes 8..12 is what tells a photo from a video apart.
        "image/heic"
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && &bytes[8..12] == b"qt  " {
        "video/quicktime"
    } else if bytes.len() >= 8 && &bytes[4..8] == b"ftyp" {
        "video/mp4"
    } else if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        matroska_mime(bytes)
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WAVE" {
        "audio/wav"
    } else if bytes.starts_with(b"fLaC") {
        "audio/flac"
    } else if bytes.starts_with(b"OggS") {
        "audio/ogg"
    } else if bytes.starts_with(b"ID3")
        || bytes.starts_with(&[0xFF, 0xFB])
        || bytes.starts_with(&[0xFF, 0xF3])
        || bytes.starts_with(&[0xFF, 0xF2])
    {
        "audio/mpeg"
    } else if std::str::from_utf8(bytes).is_ok() {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

/// What an EBML file is called. `video/webm` only when its tracks are all
/// webm-legal — the container's promise is the tracks, not the magic, and
/// the sniffer spent years calling every Matroska a webm, which Firefox
/// answers with silence when the audio turns out to be AAC. Bytes this
/// parser cannot bound keep their old name: an unparseable Matroska stays
/// `video/webm`, the verdict every EBML file wore before tracks counted.
fn matroska_mime(bytes: &[u8]) -> &'static str {
    match ebml_tracks(bytes) {
        Some(tracks) if webm_legal(&tracks) => "video/webm",
        Some(_) => "video/x-matroska",
        None => "video/webm",
    }
}

/// One track a container declares: its class, and the codec it names — a
/// Matroska `CodecID` such as `V_VP9`, or an ISO-BMFF sample-entry FourCC
/// such as `avc1` — lowercased so every verdict compares one spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Track {
    pub kind: TrackKind,
    pub codec: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackKind {
    Video,
    Audio,
    Other,
}

/// The Matroska tracks, read straight out of the bytes: the EBML header,
/// then the `Segment`'s children down to the `Tracks` element, one entry
/// per `TrackEntry` (`TrackType`, then `CodecID`). `None` when the
/// structure cannot be walked — an element whose size cannot be skipped,
/// a cluster met before the tracks — and the caller keeps its old verdict.
/// A track wearing `ContentEncodings` reads as unknown: a header-compressed
/// or encrypted track is not the codec its `CodecID` names.
pub(crate) fn ebml_tracks(bytes: &[u8]) -> Option<Vec<Track>> {
    let end = bytes.len();
    let (_, _, _, header_end) = ebml_element(bytes, 0, end)?;
    let (id, _, segment, segment_end) = ebml_element(bytes, header_end, end)?;
    if id != 0x1853_8067 {
        return None; // Segment
    }
    let mut at = segment;
    while at < segment_end {
        let (id, _, start, stop) = ebml_element(bytes, at, segment_end)?;
        match id {
            // Tracks
            0x1654_AE6B => return Some(track_entries(bytes, start, stop)),
            // Cluster: muxers write the tracks before the clusters, so one
            // met here means the walk is done looking.
            0x1F43_B675 => return None,
            _ => {}
        }
        at = stop;
    }
    None
}

/// The `TrackEntry`s of a `Tracks` payload: each entry's `TrackType` (1 is
/// video, 2 audio, anything else a kind webm has no word for) and its
/// `CodecID`. An entry with neither is a track no verdict can trust.
fn track_entries(bytes: &[u8], start: usize, end: usize) -> Vec<Track> {
    let mut tracks = Vec::new();
    let mut at = start;
    while at < end {
        let (id, _, entry, entry_end) = match ebml_element(bytes, at, end) {
            Some(found) => found,
            None => break,
        };
        at = entry_end;
        if id != 0xAE {
            continue; // TrackEntry
        }
        let (mut kind, mut codec, mut encoded) = (TrackKind::Other, None, false);
        let mut inner = entry;
        while inner < entry_end {
            let (id, _, value, value_end) = match ebml_element(bytes, inner, entry_end) {
                Some(found) => found,
                None => break,
            };
            inner = value_end;
            match id {
                0x83 => {
                    if let Some(number) = ebml_uint(bytes, value, value_end) {
                        kind = match number {
                            1 => TrackKind::Video,
                            2 => TrackKind::Audio,
                            _ => TrackKind::Other,
                        };
                    }
                }
                0x86 => {
                    codec = Some(String::from_utf8_lossy(&bytes[value..value_end]).into_owned());
                }
                // ContentEncodings: header compression or encryption.
                0x6D80 => encoded = true,
                _ => {}
            }
        }
        if encoded {
            codec = Some(String::new());
        }
        tracks.push(Track {
            kind,
            codec: codec.unwrap_or_default().to_lowercase(),
        });
    }
    tracks
}

/// Whether every track is one a webm muxer may hold: video in the codec set
/// the container was cut down to (VP8, VP9, AV1), audio in its pair
/// (Vorbis, Opus), nothing else — and at least one track, since a file with
/// no tracks gives the verdict nothing to stand on.
pub(crate) fn webm_legal(tracks: &[Track]) -> bool {
    !tracks.is_empty()
        && tracks.iter().all(|track| match track.kind {
            TrackKind::Video => matches!(track.codec.as_str(), "v_vp8" | "v_vp9" | "v_av1"),
            TrackKind::Audio => matches!(track.codec.as_str(), "a_vorbis" | "a_opus"),
            TrackKind::Other => false,
        })
}

/// One EBML element at `at`, bounded by `end`: its id, its payload size,
/// and the payload's bounds. The id keeps its marker bits — the length
/// encoding is the id — and the size drops them. A payload claiming past
/// `end` is clamped: a truncated tail decides nothing here. `None` for an
/// unknown-size element — a live-stream shape a file has no business
/// carrying — and for bytes that end inside a header.
fn ebml_element(bytes: &[u8], at: usize, end: usize) -> Option<(u32, u64, usize, usize)> {
    let head = bytes.get(at..end)?;
    let id_width = head.first()?.leading_zeros() as usize + 1;
    if head.len() < id_width || id_width > 4 {
        return None;
    }
    let mut id = head[0] as u64;
    for &byte in &head[1..id_width] {
        id = (id << 8) | byte as u64;
    }
    let id = id as u32;
    // The size vint follows, same width rule, marker dropped.
    let rest = &head[id_width..];
    let size_width = rest.first()?.leading_zeros() as usize + 1;
    if rest.len() < size_width {
        return None;
    }
    let marker = 0xFFu8.checked_shr(size_width as u32).unwrap_or(0);
    let mut size = (rest[0] & marker) as u64;
    for &byte in &rest[1..size_width] {
        size = (size << 8) | byte as u64;
    }
    // Every value bit set is the unknown size — unbounded, so not a file.
    if size == (1u64 << (7 * size_width)) - 1 {
        return None;
    }
    let start = at + id_width + size_width;
    let stop = start.checked_add(usize::try_from(size).ok()?)?.min(end);
    Some((id, size, start, stop))
}

/// The unsigned integer an element's payload holds — `TrackType` and kin.
fn ebml_uint(bytes: &[u8], start: usize, end: usize) -> Option<u64> {
    let value = bytes.get(start..end)?;
    if value.len() > 8 {
        return None;
    }
    let mut number = 0u64;
    for &byte in value {
        number = (number << 8) | byte as u64;
    }
    Some(number)
}

/// Which office document a zip's bytes declare, decided the way every reader
/// of the format does: by the entry names a zip stores uncompressed. Ends in
/// [`GENERIC_MIME_TYPES`]' zip bucket when the names name nothing.
fn zip_mime(bytes: &[u8]) -> &'static str {
    if contains(bytes, b"xl/workbook.xml") {
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    } else if contains(bytes, b"opendocument.spreadsheet") {
        "application/vnd.oasis.opendocument.spreadsheet"
    } else if contains(bytes, b"ppt/presentation.xml") {
        "application/vnd.openxmlformats-officedocument.presentationml.presentation"
    } else {
        "application/zip"
    }
}

/// Whether `haystack` holds `needle` anywhere in it. Used on zip bytes, where
/// the entry names sit in the clear even when the entries themselves do not.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_the_magic_numbers() {
        assert_eq!(sniff(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A]), "image/png");
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(sniff(b"hello, this is plainly text"), "text/plain");
        assert_eq!(
            sniff(&[0x00, 0x01, 0xFE, 0xFF, 0x02]),
            "application/octet-stream"
        );
    }

    #[test]
    fn sniffs_the_media_types() {
        assert_eq!(
            sniff(b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00"),
            "video/mp4"
        );
        assert_eq!(
            sniff(b"\x00\x00\x00\x18ftypavif\x00\x00\x00\x00"),
            "image/avif"
        );
        assert_eq!(
            sniff(b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00"),
            "image/heic"
        );
        assert_eq!(
            sniff(b"\x00\x00\x00\x18ftypmif1\x00\x00\x00\x00"),
            "image/heic"
        );
        assert_eq!(
            sniff(b"\x00\x00\x00\x14ftypqt  \x00\x00\x00\x00"),
            "video/quicktime"
        );
        assert_eq!(sniff(&[0x1A, 0x45, 0xDF, 0xA3, 0x00, 0x00]), "video/webm");
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "), "image/webp");
        assert_eq!(sniff(b"RIFF\x00\x00\x00\x00WAVEfmt "), "audio/wav");
        assert_eq!(sniff(b"fLaC\x00\x00\x00\x00"), "audio/flac");
        assert_eq!(sniff(b"OggS\x00\x00\x00\x00"), "audio/ogg");
        assert_eq!(sniff(b"ID3\x03\x00\x00\x00"), "audio/mpeg");
        assert_eq!(sniff(&[0xFF, 0xFB, 0x90, 0x00]), "audio/mpeg");
    }

    /// A zip of the named entries, bytes inside irrelevant: the sniff reads
    /// the entry names a zip stores uncompressed.
    fn zip_with(names: &[&str]) -> Vec<u8> {
        use std::io::Write as _;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for name in names {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"<xml/>").unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn a_presentation_sniffs_its_presentation_mime_and_a_plain_zip_stays_a_zip() {
        let pptx = zip_with(&["ppt/presentation.xml", "ppt/slides/slide1.xml"]);
        assert_eq!(
            sniff(&pptx),
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        );
        assert_eq!(sniff(&zip_with(&["notes.txt"])), "application/zip");
    }

    fn matroska(tracks: &[(&str, u64)]) -> Vec<u8> {
        let entries: Vec<u8> = tracks
            .iter()
            .flat_map(|&(codec, kind)| {
                let mut entry = vec![0x83, 0x81, kind as u8, 0x86, 0x80 | codec.len() as u8];
                entry.extend_from_slice(codec.as_bytes());
                ebml(&[0xAE], &entry)
            })
            .collect();
        let mut bytes = ebml(&[0x1A, 0x45, 0xDF, 0xA3], b"");
        bytes.extend(ebml(
            &[0x18, 0x53, 0x80, 0x67],
            &ebml(&[0x16, 0x54, 0xAE, 0x6B], &entries),
        ));
        bytes
    }

    /// An EBML element: id, a one-octet size vint (marker bit on — the
    /// bodies here stay under 128 bytes), body.
    fn ebml(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut bytes = id.to_vec();
        bytes.push(0x80 | body.len() as u8);
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn a_matroska_counts_as_webm_only_when_its_tracks_are() {
        let legal = matroska(&[("V_VP9", 1), ("A_OPUS", 2)]);
        assert_eq!(sniff(&legal), "video/webm");
        let vp9_video_only = matroska(&[("V_VP9", 1)]);
        assert_eq!(sniff(&vp9_video_only), "video/webm");
        // The AAC-in-webm mkv that started this: same magic, but Firefox
        // demuxes the VP9 and refuses the AAC audio, so the name stays
        // honest Matroska.
        let aac = matroska(&[("V_VP9", 1), ("A_AAC", 2)]);
        assert_eq!(sniff(&aac), "video/x-matroska");
        let h264 = matroska(&[("V_MPEG4/ISO/AVC", 1), ("A_AAC", 2)]);
        assert_eq!(sniff(&h264), "video/x-matroska");
        // A subtitle track is a track webm's rule has no word for.
        let subtitle = matroska(&[("V_VP9", 1), ("A_OPUS", 2), ("S_TEXT/UTF8", 0x11)]);
        assert_eq!(sniff(&subtitle), "video/x-matroska");
    }

    #[test]
    fn an_unparseable_matroska_keeps_its_old_name() {
        assert_eq!(sniff(&[0x1A, 0x45, 0xDF, 0xA3, 0x00, 0x00]), "video/webm");
    }
}
