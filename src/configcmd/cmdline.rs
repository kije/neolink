use clap::{Parser, Subcommand, ValueEnum};

/// The config command exposes the camera's stream / image / overlay /
/// privacy-mask / day-night configuration surface (Baichuan commands
/// 25/26/44/45/52/53/56/57/78/296/297).
///
/// All settings use a read-modify-write strategy: every `set` first fetches
/// the live XML from the camera and only mutates the requested field. The
/// camera will reject re-serialized XML that drops fields it expected.
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera. Must be a name in the config
    pub camera: String,
    #[command(subcommand)]
    pub cmd: ConfigCmd,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Encoder (codec/resolution/bitrate/framerate/GOP) -- cmd 56/57
    Encoder {
        #[command(subcommand)]
        action: EncoderAction,
    },
    /// ISP image controls (brightness/contrast/saturation/sharpness/hue)
    /// -- cmd 25/26/78
    Image {
        #[command(subcommand)]
        action: ImageAction,
    },
    /// On-screen-display overlays (datetime + channel name) -- cmd 44/45
    Osd {
        #[command(subcommand)]
        action: OsdAction,
    },
    /// Privacy mask regions -- cmd 52/53
    PrivacyMask {
        #[command(subcommand)]
        action: PrivacyMaskAction,
    },
    /// Day/Night IR-cut threshold -- cmd 296/297
    DayNight {
        #[command(subcommand)]
        action: DayNightAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum EncoderAction {
    /// Print the current encoder XML
    Get,
    /// Update one or more encoder fields. Fields you don't pass are left
    /// unchanged.
    Set {
        /// Which stream to mutate (main, sub, third). Defaults to main.
        #[arg(long, default_value_t = EncStreamSel::Main, value_enum)]
        stream: EncStreamSel,
        /// Encoder bitrate (kbps)
        #[arg(long)]
        bitrate: Option<u32>,
        /// Frame rate (fps)
        #[arg(long)]
        framerate: Option<u32>,
        /// Stream width (px)
        #[arg(long)]
        width: Option<u32>,
        /// Stream height (px)
        #[arg(long)]
        height: Option<u32>,
        /// Video encoder type (e.g. `h264`, `h265`)
        #[arg(long)]
        codec: Option<String>,
        /// Rate-control / encoder type (`cbr`, `vbr`)
        #[arg(long, name = "encoder-type")]
        encoder_type: Option<String>,
        /// Encoder profile (`baseline`, `main`, `high`)
        #[arg(long)]
        profile: Option<String>,
        /// GOP length (frames)
        #[arg(long)]
        gop: Option<u32>,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum EncStreamSel {
    /// The main stream
    Main,
    /// The sub stream
    Sub,
    /// The third stream (if available)
    Third,
}

#[derive(Subcommand, Debug)]
pub enum ImageAction {
    /// Print the current VideoInput XML
    Get,
    /// Update one or more image fields. Fields you don't pass are left
    /// unchanged. Most cameras accept values in the 0..=255 range.
    Set {
        /// Brightness
        #[arg(long)]
        brightness: Option<u32>,
        /// Contrast
        #[arg(long)]
        contrast: Option<u32>,
        /// Saturation
        #[arg(long)]
        saturation: Option<u32>,
        /// Hue
        #[arg(long)]
        hue: Option<u32>,
        /// Sharpness
        #[arg(long)]
        sharpness: Option<u32>,
    },
    /// Print the full ISP XML (newer cameras only)
    Isp,
}

#[derive(Subcommand, Debug)]
pub enum OsdAction {
    /// Print the current OSD XML
    Get,
    /// Update the datetime overlay
    Datetime {
        /// Enable (`on`) or disable (`off`) the datetime overlay
        #[arg(long, value_enum)]
        state: Option<OnOff>,
        /// Position on the frame (e.g. `upperLeft`, `lowerRight`)
        #[arg(long)]
        position: Option<String>,
    },
    /// Update the channel-name overlay
    Name {
        /// Enable (`on`) or disable (`off`) the channel-name overlay
        #[arg(long, value_enum)]
        state: Option<OnOff>,
        /// Channel name text
        #[arg(long)]
        name: Option<String>,
        /// Position on the frame (e.g. `upperLeft`, `lowerRight`)
        #[arg(long)]
        position: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PrivacyMaskAction {
    /// Print the current privacy-mask XML
    Get,
    /// Enable or disable the privacy mask globally.
    Enable {
        /// `on` / `off`
        #[arg(value_enum)]
        state: OnOff,
    },
}

#[derive(Subcommand, Debug)]
pub enum DayNightAction {
    /// Print the current DayNightSwitch XML
    Get,
    /// Update the day/night switch settings.
    Set {
        /// Switch mode (`auto`, `day`, `night`, `blackAndWhite`)
        #[arg(long)]
        mode: Option<String>,
        /// Threshold (0..=100) used in `auto` mode
        #[arg(long)]
        threshold: Option<u32>,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum OnOff {
    /// Enable
    On,
    /// Disable
    Off,
}

impl OnOff {
    pub(crate) fn as_u32(self) -> u32 {
        match self {
            OnOff::On => 1,
            OnOff::Off => 0,
        }
    }
}
