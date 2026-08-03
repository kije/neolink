//! End-to-end checks against real encoders and demuxers.
//!
//! The tests in [`super::tests`] verify the muxer against a reader
//! written from the same standard by the same hand. These ones close that
//! loop with tools that have never heard of neolink: `ffmpeg` encodes
//! genuine H264/H265/AAC elementary streams, we mux them, and `ffprobe`
//! and `gst-discoverer` are asked what they see. If either one disagrees
//! with what we think we wrote, the muxer is wrong.
//!
//! All of these skip when the tool they need is not installed, in the
//! same spirit as the pipeline tests in `src/rtsp/factory.rs`.

use super::*;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Skip the test unless every named binary is on `PATH`.
fn require(tools: &[&str]) -> bool {
    for tool in tools {
        let found = Command::new(tool)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !found {
            eprintln!("skipping: `{tool}` is not installed");
            return false;
        }
    }
    true
}

/// A scratch directory of our own, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("neolink-mpegts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("could not create a scratch directory");
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-y"])
        .args(args)
        .output()
        .expect("could not run ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Encode a short clip as a bare Annex-B elementary stream.
///
/// `bframes=0` because the Baichuan protocol never carries any, so PTS
/// always equals DTS and the muxer never has to write a DTS. Feeding the
/// muxer B-frames here would test a case that cannot occur.
fn encode_video(scratch: &Scratch, codec: &str, name: &str) -> Vec<u8> {
    let path = scratch.path(name);
    let target = path.to_str().unwrap();
    match codec {
        "libx264" => ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=640x480:rate=25:duration=2",
            "-c:v",
            "libx264",
            "-bf",
            "0",
            "-g",
            "25",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "h264",
            target,
        ]),
        "libx265" => ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=640x480:rate=25:duration=2",
            "-c:v",
            "libx265",
            "-x265-params",
            "bframes=0:log-level=none",
            "-g",
            "25",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "hevc",
            target,
        ]),
        other => panic!("unknown codec {}", other),
    }
    std::fs::read(&path).expect("could not read the encoded video")
}

/// Encode a short tone as ADTS-framed AAC, the framing cameras send.
fn encode_audio(scratch: &Scratch) -> Vec<u8> {
    let path = scratch.path("a.aac");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=2:sample_rate=16000",
        "-c:a",
        "aac",
        "-ac",
        "1",
        "-f",
        "adts",
        path.to_str().unwrap(),
    ]);
    std::fs::read(&path).expect("could not read the encoded audio")
}

/// Offsets of every Annex-B start code in `data`.
fn start_codes(data: &[u8]) -> Vec<usize> {
    let mut found = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            // Prefer the 4-byte form so the leading zero stays with the
            // start code rather than trailing the previous NAL.
            let start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            found.push(start);
            i += 3;
        } else {
            i += 1;
        }
    }
    found
}

/// Split an Annex-B byte stream into access units.
///
/// This is only needed because ffmpeg hands us a flat file; the camera
/// delivers one access unit per `BcMedia` frame already, which is why the
/// muxer takes them pre-split. The rule here — start a new access unit at
/// a VCL NAL when the current one already has one — holds for the
/// single-slice-per-frame streams ffmpeg produces.
///
/// Returns `(access_unit, is_keyframe)` pairs.
fn split_access_units(data: &[u8], h265: bool) -> Vec<(Vec<u8>, bool)> {
    let offsets = start_codes(data);
    let mut units: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut has_slice = false;
    let mut is_key = false;

    for (n, &offset) in offsets.iter().enumerate() {
        let end = offsets.get(n + 1).copied().unwrap_or(data.len());
        let nal = &data[offset..end];

        // Step over the start code to reach the NAL header.
        let header = if nal.starts_with(&[0, 0, 0, 1]) {
            nal[4]
        } else {
            nal[3]
        };

        let (vcl, keyframe) = if h265 {
            let kind = (header >> 1) & 0x3F;
            // 0..=31 are VCL; 16..=23 are the IRAP (keyframe) types.
            (kind <= 31, (16..=23).contains(&kind))
        } else {
            let kind = header & 0x1F;
            (kind == 1 || kind == 5, kind == 5)
        };

        if vcl && has_slice {
            units.push((std::mem::take(&mut current), is_key));
            has_slice = false;
            is_key = false;
        }

        current.extend_from_slice(nal);
        if vcl {
            has_slice = true;
            is_key |= keyframe;
        }
    }

    if !current.is_empty() {
        units.push((current, is_key));
    }
    units
}

/// Split an ADTS stream into frames, using the length in each header.
fn split_adts(data: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let mut i = 0;
    while i + 7 <= data.len() {
        assert_eq!(data[i], 0xFF, "lost ADTS sync");
        assert_eq!(data[i + 1] & 0xF0, 0xF0, "lost ADTS sync");
        let length = ((data[i + 3] as usize & 0x03) << 11)
            | ((data[i + 4] as usize) << 3)
            | (data[i + 5] as usize >> 5);
        if length == 0 || i + length > data.len() {
            break;
        }
        frames.push(data[i..i + length].to_vec());
        i += length;
    }
    frames
}

/// Ask ffprobe to describe a file, returning its JSON.
fn ffprobe(path: &Path) -> serde_json::Value {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("could not run ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("ffprobe did not return JSON")
}

/// Mux one video elementary stream, plus optionally AAC, into TS bytes.
fn mux(video: &[u8], h265: bool, audio: Option<&[u8]>) -> Vec<u8> {
    let video_kind = if h265 {
        StreamType::H265
    } else {
        StreamType::H264
    };
    let mut kinds = vec![video_kind];
    if audio.is_some() {
        kinds.push(StreamType::Aac);
    }

    let mut muxer = TsMuxer::new(&kinds);
    let video_track = muxer.track(video_kind).unwrap();
    let audio_track = muxer.track(StreamType::Aac);

    let units = split_access_units(video, h265);
    assert!(units.len() > 25, "expected ~50 frames, got {}", units.len());

    // 25 fps video against 1024-sample AAC frames at 16 kHz; interleave
    // them on their own clocks the way the camera delivers them.
    let audio_frames = audio.map(split_adts).unwrap_or_default();
    let mut audio_next = 0usize;
    let audio_step = 1_024 * 1_000_000 / 16_000;

    let mut out = Vec::new();
    for (i, (unit, keyframe)) in units.iter().enumerate() {
        let pts = i as u64 * 40_000;
        muxer.write_frame(video_track, pts, *keyframe, unit, &mut out);

        if let Some(track) = audio_track {
            while (audio_next as u64 * audio_step) < pts + 40_000 {
                let Some(frame) = audio_frames.get(audio_next) else {
                    break;
                };
                muxer.write_frame(
                    track,
                    audio_next as u64 * audio_step,
                    false,
                    frame,
                    &mut out,
                );
                audio_next += 1;
            }
        }
    }
    out
}

#[test]
fn ffprobe_reads_back_an_h264_and_aac_program() {
    if !require(&["ffmpeg", "ffprobe"]) {
        return;
    }
    let scratch = Scratch::new("h264-aac");
    let video = encode_video(&scratch, "libx264", "v.h264");
    let audio = encode_audio(&scratch);

    let ts = mux(&video, false, Some(&audio));
    let path = scratch.path("out.ts");
    std::fs::write(&path, &ts).unwrap();

    let probe = ffprobe(&path);
    assert_eq!(probe["format"]["format_name"], "mpegts");

    let streams = probe["streams"].as_array().expect("no streams");
    assert_eq!(streams.len(), 2, "expected one video and one audio stream");

    let video_stream = &streams[0];
    assert_eq!(video_stream["codec_name"], "h264");
    assert_eq!(video_stream["width"], 640);
    assert_eq!(video_stream["height"], 480);

    let audio_stream = &streams[1];
    assert_eq!(audio_stream["codec_name"], "aac");
    assert_eq!(audio_stream["sample_rate"], "16000");
    assert_eq!(audio_stream["channels"], 1);

    // Two seconds in, two seconds out. A muxer that mangled PTS would
    // show up here as a wildly wrong duration.
    let duration: f64 = probe["format"]["duration"]
        .as_str()
        .expect("no duration")
        .parse()
        .unwrap();
    assert!(
        (1.8..2.3).contains(&duration),
        "expected roughly 2s, got {}s",
        duration
    );
}

#[test]
fn ffprobe_reads_back_an_h265_program() {
    if !require(&["ffmpeg", "ffprobe"]) {
        return;
    }
    let scratch = Scratch::new("h265");
    let video = encode_video(&scratch, "libx265", "v.h265");

    let ts = mux(&video, true, None);
    let path = scratch.path("out.ts");
    std::fs::write(&path, &ts).unwrap();

    let probe = ffprobe(&path);
    let streams = probe["streams"].as_array().expect("no streams");
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0]["codec_name"], "hevc");
    assert_eq!(streams[0]["width"], 640);
    assert_eq!(streams[0]["height"], 480);
}

#[test]
fn ffmpeg_decodes_every_frame_without_error() {
    if !require(&["ffmpeg", "ffprobe"]) {
        return;
    }
    let scratch = Scratch::new("decode");
    let video = encode_video(&scratch, "libx264", "v.h264");
    let audio = encode_audio(&scratch);

    let ts = mux(&video, false, Some(&audio));
    let path = scratch.path("out.ts");
    std::fs::write(&path, &ts).unwrap();

    // Decoding to null exercises the whole chain and turns any corrupt
    // packet into stderr output, which `-v error` would otherwise hide.
    let out = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            path.to_str().unwrap(),
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("could not run ffmpeg");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stderr.trim().is_empty(),
        "ffmpeg reported problems decoding our stream:\n{}",
        stderr
    );

    // And the frames all arrived.
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_packets",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "json",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("could not run ffprobe");
    let probe: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("ffprobe did not return JSON");
    let count = probe["streams"][0]["nb_read_packets"]
        .as_str()
        .expect("ffprobe did not count packets");
    assert_eq!(
        count, "50",
        "expected all 50 video frames to survive muxing"
    );
}

#[test]
fn gstreamer_agrees_with_ffmpeg_about_the_program() {
    if !require(&["ffmpeg"]) {
        return;
    }
    // A second, independent demuxer. gst-discoverer-1.0 has no
    // `-version` flag in the shape `require` expects, so probe it here.
    let available = Command::new("gst-discoverer-1.0")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !available {
        eprintln!("skipping: `gst-discoverer-1.0` is not installed");
        return;
    }

    let scratch = Scratch::new("gst");
    let video = encode_video(&scratch, "libx264", "v.h264");
    let audio = encode_audio(&scratch);
    let ts = mux(&video, false, Some(&audio));
    let path = scratch.path("out.ts");
    std::fs::write(&path, &ts).unwrap();

    let out = Command::new("gst-discoverer-1.0")
        .arg(path.to_str().unwrap())
        .output()
        .expect("could not run gst-discoverer-1.0");
    let report = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "gst-discoverer failed:\n{}", report);
    assert!(
        report.contains("H.264") || report.contains("h264"),
        "gstreamer did not find the video track:\n{}",
        report
    );
    assert!(
        report.contains("MPEG-4 AAC") || report.contains("audio/mpeg"),
        "gstreamer did not find the audio track:\n{}",
        report
    );
}
