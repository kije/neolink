use clap::{Parser, Subcommand};

/// The audio command queries or sets the audio configuration.
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera. Must be a name in the config
    pub camera: String,
    /// Audio channel id (defaults to 0).
    #[arg(long, short = 'c', default_value_t = 0)]
    pub channel: u8,
    /// Sub-command (read by default).
    #[command(subcommand)]
    pub action: Option<Action>,
}

#[derive(Subcommand, Debug)]
pub enum Action {
    /// Print the current audio configuration (cmd 264) as XML.
    Cfg,
    /// Set the speaker volume (0-100).
    SetSpeaker {
        /// Volume, typically 0-100.
        volume: u8,
    },
    /// Set the microphone volume (0-100).
    SetMic {
        /// Volume, typically 0-100.
        volume: u8,
    },
    /// Print the noise-reduction configuration (cmd 438) as XML.
    Noise,
    /// Set the noise-reduction level (`0` to disable).
    SetNoise {
        /// Noise reduction intensity.
        level: u8,
    },
}
