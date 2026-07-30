use clap::{Parser, ValueEnum};
use neolink_core::bc_protocol::SmartAiKind;

/// The ai command inspects and configures the camera's AI detection
///
/// Second generation Reolink cameras (Duo 3, TrackMix, CX series, ...) expose
/// their AI detectors only over the Baichuan protocol. This command reads the
/// smart-AI zones, gets/sets the per-AI-type alarm settings and baby-cry
/// detection, and can watch detections live.
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera. Must be a name in the config
    pub camera: String,
    #[command(subcommand)]
    pub cmd: AiCommand,
}

/// Which part of the AI surface to operate on
#[derive(Parser, Debug)]
pub enum AiCommand {
    /// Dump the configured smart-AI detection zones as XML
    Zones {
        /// Which detector to dump. Dumps all five when omitted.
        #[clap(value_enum)]
        kind: Option<SmartKind>,
    },
    /// Get or set the alarm config (sensitivity / stay time) of one AI type
    Alarm {
        /// The AI type, e.g. people, vehicle, dog_cat
        ai_type: String,
        /// New sensitivity, 0-100. Reads the current config when omitted.
        #[arg(short, long)]
        sensitivity: Option<u32>,
        /// New stay time in seconds
        #[arg(long)]
        stay_time: Option<u32>,
    },
    /// Get or set the baby-cry detection sensitivity
    Cry {
        /// New sensitivity. Reads the current value when omitted.
        level: Option<u32>,
    },
    /// Dump the whole AiCfg block (auto-tracking plus cry detection) as XML
    Cfg,
    /// Watch AI detections as they happen
    Watch {
        /// Also listen for the YOLO push events (cmd 600/696) that only the
        /// newest models send
        #[arg(long)]
        yolo: bool,
    },
}

/// The smart-AI detectors, as command line values
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SmartKind {
    /// Line-crossing zones
    Crossline,
    /// Intrusion zones
    Intrusion,
    /// Loitering zones
    #[value(alias = "linger")]
    Loitering,
    /// Forgotten-object zones
    #[value(alias = "forgotten")]
    Legacy,
    /// Taken-object zones
    #[value(alias = "taken")]
    Loss,
}

impl From<SmartKind> for SmartAiKind {
    fn from(value: SmartKind) -> Self {
        match value {
            SmartKind::Crossline => SmartAiKind::Crossline,
            SmartKind::Intrusion => SmartAiKind::Intrusion,
            SmartKind::Loitering => SmartAiKind::Loitering,
            SmartKind::Legacy => SmartAiKind::Legacy,
            SmartKind::Loss => SmartAiKind::Loss,
        }
    }
}
