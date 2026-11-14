//! Video export of a recorded page via ffmpeg.
//!
//! Recordings are variable frame rate (the browser only emits frames on repaint),
//! so frames are resampled to a constant rate by holding the latest frame at each
//! output tick. Wall-clock timing is preserved, including idle stretches.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::frames::FrameIndex;

pub struct ExportOptions {
    pub fps: u32,
    /// Hold the last frame this long so the end of the run is visible.
    pub tail_ms: f64,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            fps: 25,
            tail_ms: 500.0,
        }
    }
}

pub fn export_mp4(frames: &Path, out: &Path, opts: &ExportOptions) -> Result<()> {
    let mut index = FrameIndex::open(frames)?;
    if index.entries.is_empty() {
        bail!("no frames recorded for this page");
    }

    // Normalise every frame to the first frame's size: viewport changes mid-run
    // would otherwise break the encoder. H.264 needs even dimensions.
    let first = index.read(0)?;
    let (w, h) = image::ImageReader::with_format(std::io::Cursor::new(&first), image::ImageFormat::Jpeg)
        .into_dimensions()
        .context("reading first frame size")?;
    let (w, h) = (w + w % 2, h + h % 2);

    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let filter =
        format!("scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,format=yuv420p");
    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "image2pipe",
            "-c:v",
            "mjpeg",
            "-framerate",
            &opts.fps.to_string(),
            "-i",
            "-",
        ])
        .args(["-vf", &filter, "-c:v", "libx264", "-preset", "veryfast", "-crf", "23"])
        .args(["-movflags", "+faststart"])
        .arg(out)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning ffmpeg (is it installed and on PATH?)")?;

    let step = 1000.0 / opts.fps as f64;
    let start = index.entries[0].at;
    let end = index.entries.last().unwrap().at + opts.tail_ms;
    let mut stdin = child.stdin.take().unwrap();
    let mut current = usize::MAX;
    let mut jpeg = Vec::new();
    let mut t = start;
    let write_result: Result<()> = (|| {
        while t <= end {
            let i = index.index_at(t).unwrap_or(0);
            if i != current {
                jpeg = index.read(i)?;
                current = i;
            }
            stdin.write_all(&jpeg)?;
            t += step;
        }
        Ok(())
    })();
    drop(stdin);

    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("ffmpeg failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    write_result
}
