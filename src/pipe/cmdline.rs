use clap::{Parser, ValueEnum};
use std::path::PathBuf;

use crate::stream::cmdline::{Audio, Format, Stream};

/// The pipe command serves cameras over FIFOs and sockets in a directory
///
/// It is the answer to neolink and its reader being in different places —
/// neolink on the host, go2rtc or Frigate in a container. `neolink stream`
/// needs the reader to be able to run the neolink binary; this does not.
/// It creates one endpoint per camera in a directory you can bind-mount,
/// and streams into whichever of them a reader opens:
///
/// {n}    neolink pipe --config=/etc/neolink.toml --dir=/run/neolink
///
/// {n}    # docker-compose.yml
/// {n}    volumes:
/// {n}      - /run/neolink:/pipes
///
/// {n}    # go2rtc
/// {n}    streams:
/// {n}      front: exec:cat /pipes/Front.ts
///
/// A camera is only connected while something is reading its endpoint, so
/// an unwatched camera costs nothing.
#[derive(Parser, Debug)]
pub struct Opt {
    /// The directory to create the endpoints in
    ///
    /// Created if it does not exist. Bind-mount this into the container
    /// that will read from it.
    #[arg(short, long)]
    pub dir: PathBuf,

    /// Which cameras to serve. Repeatable; defaults to all of them
    ///
    /// No short form: `-c` is taken by the global `--config`.
    #[arg(long)]
    pub camera: Vec<String>,

    /// Which kind of endpoint to create
    #[arg(long, value_enum, default_value_t = Endpoint::Fifo)]
    pub endpoint: Endpoint,

    /// Which of each camera's streams to pull
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
    #[arg(long, default_value_t = 2.0)]
    pub audio_probe: f32,

    /// How far behind a reader may fall before its backlog is thrown
    /// away, in seconds. Zero never throws anything away
    ///
    /// This is what stops a reader that has crashed or stalled — go2rtc
    /// restarting, a container being redeployed — from coming back to a
    /// queue of everything it missed and running that far behind for
    /// good. Past this much queued stream time the backlog is dropped and
    /// the reader picks up again at the next keyframe.
    #[arg(long, default_value_t = 2.0)]
    pub max_backlog: f32,
}

impl Opt {
    pub(crate) fn settings(&self) -> crate::stream::pump::Settings {
        crate::stream::pump::Settings {
            format: self.format,
            audio: self.audio,
            audio_probe: self.audio_probe,
        }
    }

    pub(crate) fn backlog(&self) -> Option<std::time::Duration> {
        (self.max_backlog > 0.0).then(|| std::time::Duration::from_secs_f32(self.max_backlog))
    }
}

/// Which kind of endpoint to create for each camera.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Endpoint {
    /// A named pipe, `<camera>.ts`
    ///
    /// Reads like a file, so anything can consume it: `exec:cat` in
    /// go2rtc, or an ffmpeg input path in Frigate. One reader at a time —
    /// two processes reading the same FIFO would each get a share of the
    /// bytes and neither a whole stream.
    Fifo,
    /// A unix domain socket, `<camera>.sock`
    ///
    /// Takes as many readers at once as you like, each starting at its
    /// own keyframe, and a reader that dies is noticed immediately rather
    /// than at the next write. Needs a consumer that can open one:
    /// ffmpeg's `unix://` protocol can, so Frigate and go2rtc's `ffmpeg:`
    /// source can, but go2rtc's own pipe reader cannot.
    Socket,
    /// Both of the above, for each camera
    Both,
}

impl Endpoint {
    pub(crate) fn fifo(self) -> bool {
        matches!(self, Endpoint::Fifo | Endpoint::Both)
    }

    pub(crate) fn socket(self) -> bool {
        matches!(self, Endpoint::Socket | Endpoint::Both)
    }
}
