use clap::{Parser, ValueEnum};
use neolink_core::bc_protocol::StreamKind;
use std::path::PathBuf;

/// The stream command writes a camera's stream to stdout, a file or a FIFO
///
/// It exists so another program can read a camera without neolink having to
/// run a server for it. The intended consumer is go2rtc's `exec:` source,
/// configured like this:
///
/// {n}    streams:
/// {n}      front: exec:neolink stream --config=/etc/neolink.toml Front
///
/// go2rtc starts the process when a viewer arrives and stops it when the
/// last one leaves, so the camera is only ever connected on demand and
/// exactly once per stream.
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera to stream. Must be a name in the config
    pub camera: String,

    /// Where to write the stream
    ///
    /// `-` means standard output, which is what a pipe consumer wants. Any
    /// other value is a path; if it names an existing FIFO the open will
    /// block until a reader arrives, which is normal.
    #[arg(short, long, default_value = "-")]
    pub output: Output,

    /// Which of the camera's streams to pull
    #[arg(long, value_enum, default_value_t = Stream::Main)]
    pub stream: Stream,

    /// The container to write
    #[arg(long, value_enum, default_value_t = Format::MpegTs)]
    pub format: Format,

    /// Which audio track to include
    #[arg(long, value_enum, default_value_t = Audio::Auto)]
    pub audio: Audio,

    /// How long to wait for an audio frame before deciding a stream is
    /// video only, in seconds
    ///
    /// The set of tracks has to be fixed before the first byte is written,
    /// because a consumer reads it once and holds onto it. That means
    /// waiting long enough to know whether the camera sends audio at all.
    /// Cameras that do normally send some within a few hundred
    /// milliseconds. Irrelevant when `--audio none` is set.
    #[arg(long, default_value_t = 2.0)]
    pub audio_probe: f32,

    /// How far behind the reader may fall before its backlog is thrown
    /// away, in seconds. Zero never throws anything away
    ///
    /// Live video goes stale: if the reader stalls or dies, queuing what
    /// it missed only means it plays the stall back afterwards, that far
    /// behind for good. Past this much queued stream time, everything
    /// waiting is dropped and the reader is restarted at the next
    /// keyframe, so what it gets is current.
    ///
    /// Set it to zero when writing a file you intend to keep, where
    /// falling behind is fine and losing frames is not.
    #[arg(long, default_value_t = 2.0)]
    pub max_backlog: f32,
}

impl Opt {
    /// The settings that shape the stream rather than its destination.
    pub(crate) fn settings(&self) -> crate::stream::pump::Settings {
        crate::stream::pump::Settings {
            format: self.format,
            audio: self.audio,
            audio_probe: self.audio_probe,
        }
    }

    /// How much stream time a reader may have queued, or `None` to keep
    /// everything however far behind it gets.
    pub(crate) fn backlog(&self) -> Option<std::time::Duration> {
        (self.max_backlog > 0.0).then(|| std::time::Duration::from_secs_f32(self.max_backlog))
    }
}

/// Where the stream is written.
#[derive(Clone, Debug)]
pub(crate) enum Output {
    /// Standard output.
    Stdout,
    /// A file or FIFO.
    Path(PathBuf),
}

impl std::str::FromStr for Output {
    type Err = std::convert::Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "-" => Output::Stdout,
            path => Output::Path(PathBuf::from(path)),
        })
    }
}

/// Which camera stream to pull.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum Stream {
    /// The HD stream
    Main,
    /// The SD stream
    Sub,
    /// A middle stream, on cameras that have one
    Extern,
}

impl From<Stream> for StreamKind {
    fn from(value: Stream) -> Self {
        match value {
            Stream::Main => StreamKind::Main,
            Stream::Sub => StreamKind::Sub,
            Stream::Extern => StreamKind::Extern,
        }
    }
}

/// The container written to the output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Format {
    /// MPEG-TS, carrying video and audio together
    ///
    /// The only format here that can carry both, and the one go2rtc's
    /// pipe reader detects by its sync byte.
    #[value(alias = "mpegts", alias = "ts")]
    MpegTs,
    /// A bare Annex-B video elementary stream, with no audio
    ///
    /// Nothing is wrapped around the camera's frames at all. Useful for
    /// piping into ffmpeg, and as a fallback if a consumer dislikes our
    /// MPEG-TS.
    Annexb,
}

/// Which audio track to publish.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum Audio {
    /// Publish whatever the camera sends, in the cheapest usable form
    ///
    /// AAC is passed through untouched; ADPCM, which nothing downstream
    /// understands, is decoded and re-encoded as A-law.
    Auto,
    /// Publish AAC only, and nothing if the camera does not send AAC
    ///
    /// This is the track go2rtc can mux straight into MP4, HLS and
    /// recordings without re-encoding, but it cannot use it for WebRTC.
    Aac,
    /// Publish G.711 A-law only
    ///
    /// The one audio format go2rtc can pass to a WebRTC viewer without
    /// transcoding. Available from ADPCM cameras; an AAC camera would
    /// need its audio decoded first, which this does not do.
    ///
    /// Needs a go2rtc newer than 1.9.14: the released one does not look
    /// for A-law in an MPEG-TS program and will show the stream as video
    /// only. Nothing breaks on an older version, the track is just not
    /// picked up.
    Pcma,
    /// Publish no audio at all
    ///
    /// Skips the wait described under `--audio-probe`.
    None,
}
