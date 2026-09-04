//! Thumbnails: one small webp per image or video file.
//!
//! Images decode in-process (`bytes -> bytes`, with no I/O and nothing
//! store-shaped in it); video needs a decoder the `image` crate does not have,
//! so those frames come from `ffmpeg` on `PATH` instead (see
//! [`thumbnail_for_video_file`]). The upload path calls into here once per
//! finished file, and the store writes whatever comes back to
//! `<storage>/thumbs/<id>`. Anything that cannot be made into a thumbnail
//! answers `None`, and the row wears `failed` — or `none` when no attempt was
//! even possible — so the attempt is never repeated: a thumbnail that cannot
//! be made is a fact, not an error.

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The longest edge a thumbnail keeps, in pixels. Big enough to be legible
/// on a file card, small enough that a library of them stays cheap.
pub const THUMB_MAX_DIM: u32 = 512;

/// How long one `ffmpeg` extraction may run before it is killed. A single
/// frame out of a local file is milliseconds; ten seconds is already a wedged
/// binary, not a slow one.
pub const FFMPEG_TIMEOUT: Duration = Duration::from_secs(10);

/// Makes the thumbnail for `bytes`: decoded, resized so the longest edge is
/// [`THUMB_MAX_DIM`], encoded as webp. `None` for bytes that are not an
/// image the decoder knows, or that fail to encode — the caller records the
/// miss and moves on.
pub fn thumbnail_for_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(bytes).ok()?;
    let thumb = image.thumbnail(THUMB_MAX_DIM, THUMB_MAX_DIM);
    let mut out = Vec::new();
    thumb
        .write_to(
            &mut std::io::Cursor::new(&mut out),
            image::ImageFormat::WebP,
        )
        .ok()?;
    if out.is_empty() { None } else { Some(out) }
}

/// Whether a thumbnail is even attempted for this mime: images, decoded
/// in-process, and the video containers the sniffer names, framed by
/// `ffmpeg`. Anything else wears `none` without a decoder ever looking at it.
pub fn thumbnailed(mime: &str) -> bool {
    mime.starts_with("image/") || is_video_mime(mime)
}

/// Whether `mime` is a video container this crate frames via `ffmpeg`:
/// exactly the video mimes the sniffer can name.
pub fn is_video_mime(mime: &str) -> bool {
    matches!(mime, "video/mp4" | "video/webm" | "video/quicktime")
}

/// Whether `ffmpeg` is on `PATH`. The verdict comes from one `ffmpeg
/// -version` run and is cached for the process: a missing binary must degrade
/// every video to its icon, not fork a failing child per upload.
pub fn ffmpeg_available() -> bool {
    static HAVE_FFMPEG: OnceLock<bool> = OnceLock::new();
    *HAVE_FFMPEG.get_or_init(|| {
        Command::new("ffmpeg")
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// Makes the thumbnail for the video at `path`: the frame at one second,
/// scaled so the longest edge is [`THUMB_MAX_DIM`], encoded as webp. Clips
/// shorter than a second fall back to their first frame. `None` without
/// `ffmpeg` on `PATH`, or when the file is not a video `ffmpeg` can read —
/// the caller records the miss and moves on, never the upload.
pub fn thumbnail_for_video_file(path: &Path) -> Option<Vec<u8>> {
    if !ffmpeg_available() {
        return None;
    }
    run_ffmpeg_frame(path, true).or_else(|| run_ffmpeg_frame(path, false))
}

/// Makes the thumbnail for video `bytes`, spilled to a temp file because
/// `ffmpeg` seeks its input and a pipe is not seekable. Same `None` contract
/// as [`thumbnail_for_video_file`].
pub fn thumbnail_for_video_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    if !ffmpeg_available() || bytes.is_empty() {
        return None;
    }
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "in-thumb-{}-{}.bin",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, bytes).ok()?;
    let out = thumbnail_for_video_file(&tmp);
    let _ = std::fs::remove_file(&tmp);
    out
}

/// Runs one `ffmpeg` frame extraction: `seek_one_second` takes the frame at
/// one second, otherwise the first frame. The webp comes back over the
/// stdout pipe; anything else — a nonzero exit, an empty pipe, bytes that are
/// not a webp — is `None`. A run past [`FFMPEG_TIMEOUT`] is killed.
fn run_ffmpeg_frame(path: &Path, seek_one_second: bool) -> Option<Vec<u8>> {
    let scale = format!(
        "scale={0}:{0}:force_original_aspect_ratio=decrease",
        THUMB_MAX_DIM
    );
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error"]);
    if seek_one_second {
        // Before `-i`: fast input seeking, good enough for a thumbnail.
        cmd.args(["-ss", "1"]);
    }
    cmd.arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-vf", &scale])
        // `-c:v libwebp` matters: without it this ffmpeg answers `-f webp`
        // with its animated encoder, which refuses a single frame.
        .args(["-c:v", "libwebp", "-f", "webp", "-"]);
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // The webp is read on its own thread: the pipe buffer is small, and a
    // parent that only `try_wait`s while the child fills the buffer would
    // deadlock against it.
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = stdout {
            let _ = out.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let buf = rx.recv().ok()?;
                if !status.success() || buf.len() < 12 {
                    return None;
                }
                // Whatever left the pipe must be the webp asked for.
                if &buf[0..4] == b"RIFF" && &buf[8..12] == b"WEBP" {
                    return Some(buf);
                }
                return None;
            }
            Ok(None) => {
                if start.elapsed() >= FFMPEG_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1x1 PNG, hand-built: the smallest thing the decoder must accept.
    fn tiny_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
        let mut out = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut out),
            image::ImageFormat::Png,
        )
        .unwrap();
        out
    }

    #[test]
    fn an_image_makes_a_webp_thumbnail() {
        let thumb = thumbnail_for_bytes(&tiny_png()).unwrap();
        // A webp RIFF header, not the PNG that went in.
        assert_eq!(&thumb[0..4], b"RIFF");
        assert_eq!(&thumb[8..12], b"WEBP");
        let back = image::load_from_memory(&thumb).unwrap();
        assert!(back.width() <= THUMB_MAX_DIM && back.height() <= THUMB_MAX_DIM);
    }

    #[test]
    fn non_images_make_no_thumbnail() {
        assert!(thumbnail_for_bytes(b"just some text").is_none());
        assert!(thumbnail_for_bytes(&[]).is_none());
    }

    #[test]
    fn images_and_covered_video_are_thumbnailed() {
        assert!(thumbnailed("image/png"));
        assert!(thumbnailed("image/webp"));
        assert!(thumbnailed("video/mp4"));
        assert!(thumbnailed("video/webm"));
        assert!(thumbnailed("video/quicktime"));
        assert!(!thumbnailed("application/pdf"));
        assert!(!thumbnailed("text/plain"));
        assert!(!thumbnailed("audio/mpeg"));
        assert!(!thumbnailed("application/octet-stream"));
    }

    /// Renders a tiny mp4 with `ffmpeg`'s own test pattern, for the tests
    /// that need a real video. `None` when the render itself fails.
    fn make_test_mp4(duration_secs: &str) -> Option<Vec<u8>> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "in-test-video-{}-{}.mp4",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let rendered = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=duration={duration_secs}:size=64x64:rate=10"),
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "mpeg4",
                "-y",
            ])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        let bytes = if rendered.success() {
            std::fs::read(&path).ok()
        } else {
            None
        };
        let _ = std::fs::remove_file(&path);
        bytes
    }

    #[test]
    fn a_video_frame_makes_a_webp_thumbnail() {
        if !ffmpeg_available() {
            // No decoder on PATH, no test: the upload path degrades the same
            // way and asserts nothing here.
            return;
        }
        let mp4 = make_test_mp4("2").expect("ffmpeg renders the fixture");
        let thumb = thumbnail_for_video_bytes(&mp4).expect("a frame is framed");
        assert_eq!(&thumb[0..4], b"RIFF");
        assert_eq!(&thumb[8..12], b"WEBP");
        let back = image::load_from_memory(&thumb).unwrap();
        assert!(back.width() <= THUMB_MAX_DIM && back.height() <= THUMB_MAX_DIM);
    }

    #[test]
    fn a_sub_second_clip_falls_back_to_its_first_frame() {
        if !ffmpeg_available() {
            return;
        }
        let mp4 = make_test_mp4("0.3").expect("ffmpeg renders the fixture");
        let thumb = thumbnail_for_video_bytes(&mp4).expect("short clips frame too");
        assert_eq!(&thumb[0..4], b"RIFF");
        assert_eq!(&thumb[8..12], b"WEBP");
    }

    #[test]
    fn garbage_video_makes_no_thumbnail() {
        // Bytes `ffmpeg` cannot read — and a path that is not a file at all —
        // degrade to `None`, on every machine: with `ffmpeg` the extraction
        // fails, without it no attempt runs. Either way nothing panics and
        // nothing hangs.
        let mut garbage = b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00".to_vec();
        garbage.extend_from_slice(&[0xA5u8; 4096]);
        assert!(thumbnail_for_video_bytes(&garbage).is_none());
        assert!(thumbnail_for_video_bytes(&[]).is_none());
        assert!(
            thumbnail_for_video_file(Path::new("/nonexistent/in-thumb-probe")).is_none()
        );
    }
}
