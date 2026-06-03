use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// The playback command exposes the Baichuan recording / replay surface:
/// list SD-card recordings, search by alarm type, fetch a cover preview
/// JPEG, or download a clip.
#[derive(Parser, Debug)]
pub struct Opt {
    #[command(subcommand)]
    pub cmd: Action,
}

#[derive(Subcommand, Debug)]
pub enum Action {
    /// List recordings on the camera's SD card within a date range.
    List(ListArgs),
    /// Fetch a JPEG cover-preview thumbnail at a unix timestamp.
    Cover(CoverArgs),
    /// Download a recording's raw bytes to a `.bcmedia` file.
    ///
    /// The output is the raw BcMedia stream (same shape as a live
    /// preview); pipe it through an MP4 muxer if you want a playable
    /// container. This avoids transcoding.
    Download(DownloadArgs),
}

#[derive(Parser, Debug)]
pub struct ListArgs {
    /// The name of the camera. Must be a name in the config.
    pub camera: String,
    /// Start date `YYYY-MM-DD`.
    #[arg(long)]
    pub from: String,
    /// End date `YYYY-MM-DD` (inclusive).
    #[arg(long)]
    pub to: String,
    /// Comma-separated allow-list of record types. Defaults to the
    /// camera's full allow-list. Examples: `md`, `md,people,vehicle`.
    #[arg(long)]
    pub record_types: Option<String>,
    /// Comma-separated alarm types. When set, the alarm-video search
    /// (cmd 272) is used instead of the generic file-info-list (cmd 14).
    /// Examples: `md`, `people,vehicle`.
    #[arg(long)]
    pub alarm_types: Option<String>,
    /// Channel id (default 0; non-zero on NVR setups).
    #[arg(long, default_value_t = 0)]
    pub channel: u8,
    /// Stream to query. Default `mainStream`.
    #[arg(long, default_value = "mainStream")]
    pub stream: String,
}

#[derive(Parser, Debug)]
pub struct CoverArgs {
    /// The name of the camera. Must be a name in the config.
    pub camera: String,
    /// Output file (e.g. `preview.jpg`).
    #[arg(short, long)]
    pub file: PathBuf,
    /// Unix timestamp (seconds) to capture the cover at.
    #[arg(long)]
    pub time: u64,
    /// Channel id (default 0).
    #[arg(long, default_value_t = 0)]
    pub channel: u8,
    /// Stream to query. Default `mainStream`.
    #[arg(long, default_value = "mainStream")]
    pub stream: String,
}

#[derive(Parser, Debug)]
pub struct DownloadArgs {
    /// The name of the camera. Must be a name in the config.
    pub camera: String,
    /// File name of the clip as reported by `playback list`.
    #[arg(long)]
    pub name: String,
    /// Output file. The raw BcMedia bytes are written verbatim.
    #[arg(short, long)]
    pub file: PathBuf,
    /// Channel id (default 0).
    #[arg(long, default_value_t = 0)]
    pub channel: u8,
    /// Stream to query. Default `mainStream`.
    #[arg(long, default_value = "mainStream")]
    pub stream: String,
}
