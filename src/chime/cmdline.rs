use clap::Parser;

/// The chime command controls the doorbell + wireless chime accessories
/// over the Baichuan protocol.
///
/// Supported sub-commands:
///
/// - `list` — list paired wireless chimes
/// - `ring` — trigger a wireless chime by ID with a tone
/// - `silent` — get/set the silent-window for a chime
/// - `quick-reply` — play a pre-recorded audio clip from the doorbell
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera (must be a name in the config)
    pub camera: String,

    #[command(subcommand)]
    pub cmd: ChimeCommand,
}

#[derive(Parser, Debug)]
pub enum ChimeCommand {
    /// List the wireless chimes paired with the doorbell.
    List,
    /// Trigger a chime to play a tone (cmd 485 sub-op 4 ringWithMusic).
    Ring {
        /// Wireless chime device id (as reported by `list`)
        device_id: u32,
        /// Tone name (citybird, originaltune, pianokey, loop, attraction,
        /// hophop, goodday, operetta, moonlight, waybackhome) OR a raw
        /// numeric tone id.
        tone: String,
    },
    /// Get or set the silent-window configuration for a chime (cmd 609/610).
    Silent {
        /// Wireless chime device id
        device_id: u32,
        /// Weekday bitmask: bit 0 = Sunday … bit 6 = Saturday (e.g. 63 = Mon-Sat).
        /// Omit to print the current window.
        weekday_mask: Option<u32>,
        /// Start time, HH:MM
        start_time: Option<String>,
        /// End time, HH:MM
        end_time: Option<String>,
    },
    /// Play a pre-recorded quick-reply audio clip stored on the camera (cmd 349).
    QuickReply {
        /// File id of the clip (as configured on the camera)
        file_id: u32,
        /// Playback duration in seconds (default: 0 = camera-determined)
        #[arg(long, default_value_t = 0)]
        duration: u32,
        /// Number of times to play (default: 1)
        #[arg(long, default_value_t = 1)]
        times: u32,
    },
}
