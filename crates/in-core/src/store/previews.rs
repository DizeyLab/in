//! Preview derivatives: the playable stand-in for a video whose stored
//! bytes a browser cannot demux as they sit.
//!
//! The stored file is never rewritten — `?dl=1` and every download still
//! serve the original bytes byte for byte. What this module adds is a
//! second blob at `previews/<id>`, built the first time a media route asks
//! to serve a file whose tracks are not what its container's magic claims:
//! the AAC-in-webm Matroska the sniffer used to have to call `video/webm`
//! (Firefox demuxes the VP9 and refuses the audio), HEVC-in-mp4, and kin.
//! After that one build, every media serve draws from the derivative.
//!
//! The build is one `ffmpeg` child under the same house rules
//! [`crate::thumbs`] runs — `-v error`, a runaway killed, output that must
//! declare itself the container it was asked for — staged to a temp file in
//! the previews tree and placed with [`Blobs::adopt`], so a reader of
//! `previews/<id>` sees nothing or the whole derivative, never half, and
//! two racing builders cannot tear it. Every failure serves the original:
//! a derivative that cannot be made is a miss, not an error.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt as _;
use ulid::Ulid;

use super::blobs::Blobs;
use super::sniff::{self, Track, TrackKind};
use super::MediaDerivative;
use super::turso_store::FILES_DIR;

/// The key family derivatives live in: `previews/<id>`, beside `files/` and
/// `thumbs/` — deleted alongside them in every purge, and swept for orphans
/// at boot under the same watermark rule as every other half of the tree.
pub(super) const PREVIEWS_DIR: &str = "previews";

/// Head bytes inspected for a Matroska `Tracks` element. Muxers write the
/// tracks before the clusters, so the element sits near the head of every
/// real file; a Matroska that does not show it in this window is one the
/// verdict declines to name, and the original is served.
const DETECT_WINDOW: u64 = 1024 * 1024;

/// The largest `moov` read whole for track inspection. A `moov` bigger than
/// this carries no codecs this needs — and a bound is what keeps a hostile
/// box walk from allocating to a size it names itself.
const MOOV_CAP: u64 = 16 * 1024 * 1024;

/// How long one derivative build may run before it is killed. Unlike the
/// thumbnailer's one frame, this is a whole transcode of a whole file —
/// minutes, not milliseconds — but a build past ten minutes is wedged, and
/// the request behind it is better served the original.
const BUILD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// What to do with a file's stored bytes at media-serve time.
enum Plan {
    /// The stored tracks play as they sit: serve the original.
    Native,
    /// Build this derivative first, then serve it.
    Build(Build),
    /// No recipe this module knows: serve the original, today's behavior.
    Unsupported,
}

/// One derivative worth building, and the `ffmpeg` recipe for it.
enum Build {
    /// VP8/VP9/AV1 video stream-copied, the audio transcoded to Opus,
    /// muxed as the webm the tracks were almost legal for.
    Webm { video: bool, audio: bool },
    /// H.264 or H.265 video with AAC (or no) audio, remuxed into a
    /// faststart mp4 — no re-encode, one container the browsers demux.
    RemuxMp4 { audio: bool },
}

impl Build {
    /// The mime the result will carry.
    fn mime(&self) -> &'static str {
        match self {
            Build::Webm { .. } => "video/webm",
            Build::RemuxMp4 { .. } => "video/mp4",
        }
    }

    /// The `ffmpeg` arguments: explicit stream maps, so nothing a track
    /// parser never blessed — a subtitle, an attachment — rides along.
    fn args(&self, input: &Path, output: &Path) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "-v".into(),
            "error".into(),
            "-y".into(),
            "-i".into(),
            input.display().to_string(),
        ];
        match self {
            Build::Webm { video, audio } => {
                if *video {
                    args.extend(["-map", "0:v:0", "-c:v", "copy"].map(str::to_string));
                }
                if *audio {
                    args.extend(["-map", "0:a:0", "-c:a", "libopus"].map(str::to_string));
                }
                args.extend(["-f", "webm"].map(str::to_string));
            }
            Build::RemuxMp4 { audio } => {
                args.extend(["-map", "0:v:0"].map(str::to_string));
                if *audio {
                    args.extend(["-map", "0:a:0"].map(str::to_string));
                }
                args.extend(
                    ["-c", "copy", "-movflags", "+faststart", "-f", "mp4"].map(str::to_string),
                );
            }
        }
        args.push(output.display().to_string());
        args
    }
}

/// The derivative to serve for file `id` — built first, when the stored
/// tracks need one — or `None`, the answer for every file that is not a
/// video container, already plays, has no recipe, or whose build failed.
/// A first request may wait one whole transcode; the permit makes every
/// later one a cache read.
pub(super) async fn ensure(
    storage: &Path,
    blobs: &dyn Blobs,
    id: &str,
    mime: &str,
) -> Option<MediaDerivative> {
    if !is_video_container(mime) {
        return None;
    }
    let key = format!("{PREVIEWS_DIR}/{id}");
    if let Some(ready) = cached(blobs, &key).await {
        return Some(ready);
    }
    let permit = build_permit(id).await;
    let _guard = permit.lock().await;
    if let Some(ready) = cached(blobs, &key).await {
        release_permit(id);
        return Some(ready);
    }
    let built = match detect(blobs, id).await {
        Plan::Native | Plan::Unsupported => None,
        Plan::Build(build) => build_derivative(storage, blobs, id, &key, build).await,
    };
    release_permit(id);
    built
}

/// The video containers a derivative may stand in for — the spellings the
/// sniffer and the moviemakers actually produce.
fn is_video_container(mime: &str) -> bool {
    matches!(
        mime,
        "video/mp4" | "video/webm" | "video/quicktime" | "video/x-matroska"
    )
}

/// The derivative already sitting at `key`, its mime read from its own head
/// — a restart forgets the recipe, not the derivative. A blob whose head
/// names no container is not a derivative: `None`, and the caller rebuilds.
async fn cached(blobs: &dyn Blobs, key: &str) -> Option<MediaDerivative> {
    let span = blobs.span(key, 0, u64::MAX).await.ok()??;
    let head = read_stream(span.stream, 12).await;
    let mime = container_mime(&head)?;
    Some(MediaDerivative {
        mime,
        size: span.len,
    })
}

/// The container a head of bytes declares: EBML magic or an `ftyp` box.
fn container_mime(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Some("video/webm")
    } else if head.len() >= 12 && &head[4..8] == b"ftyp" {
        Some("video/mp4")
    } else {
        None
    }
}

/// What the stored bytes are, decided without staging: the tracks are read
/// out of windows of the blob, and only a build actually wanted drags the
/// whole original to disk.
async fn detect(blobs: &dyn Blobs, id: &str) -> Plan {
    let key = format!("{FILES_DIR}/{id}");
    let head = match read_span(blobs, &key, 0, DETECT_WINDOW).await {
        Some(found) => found,
        None => return Plan::Unsupported,
    };
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return match sniff::ebml_tracks(&head) {
            None => Plan::Unsupported,
            Some(tracks) if sniff::webm_legal(&tracks) => Plan::Native,
            Some(tracks) => matroska_plan(&tracks),
        };
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        let total = match read_span_len(blobs, &key).await {
            Some(len) => len,
            None => return Plan::Unsupported,
        };
        return mp4_plan(blobs, &key, total).await;
    }
    Plan::Unsupported
}

/// The build for Matroska tracks that are not webm-legal: VP8/VP9/AV1 video
/// stream-copies into a real webm with whatever audio there is transcoded to
/// Opus; H.264 or H.265 with AAC (or silence) remuxes into a faststart mp4.
/// Anything else has no recipe worth a transcode — the original is what
/// plays-or-doesn't today.
fn matroska_plan(tracks: &[Track]) -> Plan {
    let video = first_codec_of(tracks, TrackKind::Video);
    let audio = first_codec_of(tracks, TrackKind::Audio);
    let webm_video = matches!(video, Some("v_vp8" | "v_vp9" | "v_av1"));
    let aac = matches!(audio, Some("a_aac"));
    if webm_video {
        return Plan::Build(Build::Webm {
            video: video.is_some(),
            audio: audio.is_some(),
        });
    }
    if matches!(video, Some("v_mpeg4/iso/avc" | "v_mpegh/iso/hevc"))
        && (audio.is_none() || aac)
    {
        return Plan::Build(Build::RemuxMp4 {
            audio: audio.is_some(),
        });
    }
    Plan::Unsupported
}

/// The mp4 plan: an mp4 that is not native is helped only when its tracks
/// are the remuxable kind — H.264/H.265 video with AAC (or no) audio —
/// where the fix is the faststart moov, not the codecs.
async fn mp4_plan(blobs: &dyn Blobs, key: &str, total: u64) -> Plan {
    let Some((moov_start, moov_len)) = moov_location(blobs, key, total).await else {
        return Plan::Unsupported;
    };
    if moov_len > MOOV_CAP {
        return Plan::Unsupported;
    }
    let Some(moov) = read_span(blobs, key, moov_start, moov_len).await else {
        return Plan::Unsupported;
    };
    let tracks = sniff::mp4_tracks(&moov);
    if sniff::mp4_native(&tracks) {
        return Plan::Native;
    }
    let video = first_codec_of(&tracks, TrackKind::Video);
    let audio = first_codec_of(&tracks, TrackKind::Audio);
    if matches!(video, Some(codec) if codec.starts_with("avc") || codec == "hev1" || codec == "hvc1")
        && (audio.is_none() || audio == Some("mp4a"))
    {
        return Plan::Build(Build::RemuxMp4 {
            audio: audio.is_some(),
        });
    }
    Plan::Unsupported
}

/// Where the `moov` sits: `(body offset, body length)`, walked one header
/// at a time — each read 16 bytes off the blob, each `mdat` skipped by its
/// declared size — so a faststart-less file, moov behind megabytes of
/// media, costs a handful of range reads and never the file. A `size` of 0
/// (to end of file, the streaming-written shape) resolves against the
/// blob's own length; a walk that runs off the file finds nothing.
async fn moov_location(blobs: &dyn Blobs, key: &str, total: u64) -> Option<(u64, u64)> {
    let mut at = 0u64;
    for _ in 0..64 {
        let header = read_span(blobs, key, at, 16).await?;
        if header.len() < 8 {
            return None;
        }
        let size =
            u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let typ = [header[4], header[5], header[6], header[7]];
        let (header_len, box_len) = match size {
            1 => {
                if header.len() < 16 {
                    return None;
                }
                (
                    16u64,
                    u64::from_be_bytes([
                        header[8], header[9], header[10], header[11], header[12], header[13],
                        header[14], header[15],
                    ]),
                )
            }
            0 => (8u64, total - at),
            size => (8u64, size),
        };
        if box_len < header_len {
            return None;
        }
        if typ == *b"moov" {
            return Some((at + header_len, box_len - header_len));
        }
        at += box_len;
    }
    None
}
/// The codec of a kind's first track, for the verdicts that weigh one
/// stream of each class.
fn first_codec_of<'a>(tracks: &'a [Track], kind: TrackKind) -> Option<&'a str> {
    tracks
        .iter()
        .find(|track| track.kind == kind)
        .map(|track| track.codec.as_str())
}

/// Builds and places the derivative: stage the original, run one `ffmpeg`
/// child on it, adopt the result into place. The staging lives in the
/// previews tree so the adopt's rename is same-filesystem atomic. Any miss
/// answers `None` — the original is what the caller serves — and leaves
/// nothing behind but a log line.
async fn build_derivative(
    storage: &Path,
    blobs: &dyn Blobs,
    id: &str,
    key: &str,
    build: Build,
) -> Option<MediaDerivative> {
    let dir = storage.join(PREVIEWS_DIR);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let staged = dir.join(format!("{id}.{}.in.tmp", Ulid::new()));
    let output = dir.join(format!("{id}.{}.out.tmp", Ulid::new()));
    let staged_ok = stage_original(blobs, id, &staged).await.is_some();
    let built = if staged_ok {
        let input = staged.clone();
        let target = output.clone();
        tokio::task::spawn_blocking(move || run_ffmpeg(&build, &input, &target))
            .await
            .ok()
    } else {
        None
    };
    // The length is read before the adopt: the adopt renames the staging
    // away, and the honest serving length is the file that was placed.
    let len = file_len(&output);
    let derivative = match built {
        Some(Ok(mime)) if len > 0 => match blobs.adopt(key, &output, len).await {
            Ok(()) => Some(MediaDerivative { mime, size: len }),
            Err(_) => None,
        },
        _ => None,
    };
    let _ = std::fs::remove_file(&staged);
    let _ = std::fs::remove_file(&output);
    derivative
}

/// A file's length, `0` when it has gone missing — an adopt refuses a
/// missing staging anyway, and the caller serves the original.
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// Copies the original blob to `dest`, frame by frame — never the whole
/// file in one buffer. `None` when the blob is not there to copy.
async fn stage_original(blobs: &dyn Blobs, id: &str, dest: &Path) -> Option<()> {
    let span = blobs
        .span(&format!("{FILES_DIR}/{id}"), 0, u64::MAX)
        .await
        .ok()??;
    let mut file = tokio::fs::File::create(dest).await.ok()?;
    let mut stream = span.stream;
    while let Some(chunk) = stream.next().await {
        file.write_all(&chunk.ok()?).await.ok()?;
    }
    file.flush().await.ok()?;
    Some(())
}

/// Runs one derivative build: the recipe's `ffmpeg` arguments, the
/// container bytes into `output`. The same house rules as the thumbnailer —
/// `-v error`, a runaway killed, output that must declare the container it
/// was asked for — with stderr captured for the one failure line. `Err`
/// carries nothing: the caller's contract is "serve the original".
fn run_ffmpeg(build: &Build, input: &Path, output: &Path) -> Result<&'static str, ()> {
    let mut child = Command::new("ffmpeg")
        .args(build.args(input, output))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| ())?;
    // stderr drains on its own thread: the pipe buffer is small, and a
    // parent that only `try_wait`s while the child fills it would deadlock
    // against it.
    let stderr = child.stderr.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut message = String::new();
        if let Some(mut err) = stderr {
            let _ = err.read_to_string(&mut message);
        }
        let _ = tx.send(message);
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) => {
                if start.elapsed() >= BUILD_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break false,
        }
    };
    // Whatever left the pipe must be the container asked for, and nonempty:
    // ffmpeg exits 0 over some failures it has already printed.
    let mut head = [0u8; 12];
    let head_len = std::fs::File::open(output)
        .and_then(|mut file| file.read(&mut head))
        .unwrap_or(0);
    let mime = container_mime(&head[..head_len]);
    match (status, mime) {
        (true, Some(mime)) if mime == build.mime() => Ok(mime),
        _ => {
            let message = rx.recv().unwrap_or_default();
            let reason = message.lines().next().unwrap_or("no output");
            eprintln!("preview build for {} failed: {reason}", input.display());
            Err(())
        }
    }
}

/// The derivative to serve, straight off the key: same clamping contract as
/// the file stream, so a range serve reads only its span.
pub(super) async fn stream(
    blobs: &dyn Blobs,
    id: &str,
    start: u64,
    len: u64,
) -> Option<super::FileSpan> {
    blobs
        .span(&format!("{PREVIEWS_DIR}/{id}"), start, len)
        .await
        .ok()?
}

/// `len` bytes of a blob from `start`, buffered whole — the windows track
/// detection reads, never the file a transcode consumes.
async fn read_span(blobs: &dyn Blobs, key: &str, start: u64, len: u64) -> Option<Vec<u8>> {
    let span = blobs.span(key, start, len).await.ok()??;
    let bytes = read_stream(span.stream, span.len).await;
    if bytes.len() as u64 == span.len {
        Some(bytes)
    } else {
        None
    }
}

/// A blob's honest length: the span clamp against what the backend holds.
async fn read_span_len(blobs: &dyn Blobs, key: &str) -> Option<u64> {
    blobs.span(key, 0, u64::MAX).await.ok()?.map(|span| span.len)
}

/// Drains a span's stream into one buffer, capped at `max` bytes.
async fn read_stream(
    mut stream: super::ByteStream,
    max: u64,
) -> Vec<u8> {
    let mut out = Vec::new();
    while out.len() < max as usize {
        match stream.next().await {
            Some(Ok(chunk)) => out.extend_from_slice(&chunk),
            _ => break,
        }
    }
    out
}

/// One builder at a time per file: a permit per id in flight. The map
/// holds one entry per id whose build is somewhere running — the entry
/// leaves when its last holder is done, so the map never grows past the
/// files being built right now.
static PERMITS: LazyLock<Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

/// The holder of a build: take one, guard it for the length of `ensure`.
async fn build_permit(id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    PERMITS
        .lock()
        .expect("permit map")
        .entry(id.to_string())
        .or_default()
        .clone()
}

/// Forgets a permit whose last holder just finished — the map's own
/// strong count is the only reader it can trust.
fn release_permit(id: &str) {
    let mut permits = PERMITS.lock().expect("permit map");
    if let Some(permit) = permits.get(id) {
        if std::sync::Arc::strong_count(permit) == 1 {
            permits.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipes_ask_for_the_containers_they_name() {
        let webm = Build::Webm {
            video: true,
            audio: true,
        };
        assert_eq!(webm.mime(), "video/webm");
        let args = webm.args(Path::new("in.mkv"), Path::new("out.tmp"));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert!(args.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        assert!(args.windows(2).any(|pair| pair == ["-c:a", "libopus"]));
        let remux = Build::RemuxMp4 { audio: true };
        let remux_args = remux.args(Path::new("in.mp4"), Path::new("out.tmp"));
        let args: Vec<&str> = remux_args.iter().map(String::as_str).collect();
        assert!(args.windows(2).any(|pair| pair == ["-c", "copy"]));
        assert!(args.windows(2).any(|pair| pair == ["-movflags", "+faststart"]));
    }
    #[tokio::test]
    async fn moov_location_walks_past_mdat_by_its_declared_size() {
        // ftyp, then an mdat far bigger than any head window, then moov —
        // the faststart-less shape the walk exists for.
        let dir = std::env::temp_dir().join(format!("in-preview-{}", Ulid::new()));
        std::fs::create_dir_all(dir.join("files")).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&8u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        let mdat_body = 10u64 * 1024 * 1024;
        bytes.extend_from_slice(&((mdat_body + 8) as u32).to_be_bytes());
        bytes.extend_from_slice(b"mdat");
        bytes.extend(std::iter::repeat_n(0u8, mdat_body as usize));
        bytes.extend_from_slice(&(24u32).to_be_bytes());
        bytes.extend_from_slice(b"moov");
        bytes.extend([0u8; 16]);
        std::fs::write(dir.join("files").join("x"), &bytes).unwrap();
        let total = bytes.len() as u64;
        let blobs = super::super::blobs::LocalBlobs::new(&dir);
        assert_eq!(
            moov_location(&blobs, "files/x", total).await,
            Some((total - 16, 16))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
