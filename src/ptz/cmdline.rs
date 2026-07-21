use clap::Parser;

#[derive(clap::ValueEnum, Clone, Debug)]
pub enum CmdDirection {
    Left,
    Right,
    Up,
    Down,
    Stop,
}

#[derive(clap::ValueEnum, Clone, Debug)]
pub enum OnOff {
    On,
    Off,
}

/// The ptz command will control the positioning of the camera
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera to change the lights of. Must be a name in the config
    pub camera: String,

    #[command(subcommand)]
    pub cmd: PtzCommand,
}

#[derive(Parser, Debug)]
pub enum PtzCommand {
    /// Move to a stored preset
    Preset { preset_id: Option<u8> },
    /// Assign the current position to a preset with a given name
    Assign { preset_id: u8, name: String },
    /// Performs a movement in the given direction
    Control {
        /// The amount to move
        amount: u32,
        /// The direction command
        #[clap(value_enum)]
        command: CmdDirection,
        /// The speed to move at
        speed: Option<u32>,
    },
    Zoom {
        /// The amount to zoom to
        amount: f32,
    },
    /// Drive a PTZ patrol / cruise tour
    Patrol {
        #[command(subcommand)]
        action: PatrolCommand,
    },
    /// Inspect or drive the PTZ guard / return-to-home position
    Guard {
        #[command(subcommand)]
        action: GuardCommand,
    },
    /// Click-to-zoom: have the camera centre on a given pixel
    Click {
        /// X coordinate of the click in screen pixels
        x: u32,
        /// Y coordinate of the click in screen pixels
        y: u32,
        /// Total screen width in pixels (defaults to 1920)
        #[clap(long, default_value_t = 1920)]
        screen_width: u32,
        /// Total screen height in pixels (defaults to 1080)
        #[clap(long, default_value_t = 1080)]
        screen_height: u32,
        /// Width of the click bounding box (defaults to 1)
        #[clap(long, default_value_t = 1)]
        width: u32,
        /// Height of the click bounding box (defaults to 1)
        #[clap(long, default_value_t = 1)]
        height: u32,
        /// Movement speed (camera-specific, typical range 1-64)
        #[clap(long, default_value_t = 32)]
        speed: u32,
    },
    /// Toggle the camera's auto-focus
    Autofocus {
        /// `on` to enable auto-focus, `off` to switch to manual focus
        #[clap(value_enum)]
        state: OnOff,
    },
    /// Print the current pan/tilt position
    Position,
}

#[derive(Parser, Debug)]
pub enum PatrolCommand {
    /// List the patrols configured on the camera
    List,
    /// Start running a configured patrol
    Start {
        /// The id of the patrol to start
        id: u32,
    },
    /// Stop the currently running patrol
    Stop {
        /// The id of the patrol to stop
        id: u32,
    },
}

#[derive(Parser, Debug)]
pub enum GuardCommand {
    /// Print the current guard configuration
    Get,
    /// Save the current position as the guard / return-to-home position
    Set {
        /// Inactivity seconds after which the camera auto-returns
        #[clap(long, default_value_t = 60)]
        timeout: u32,
    },
    /// Move the camera to the guard / return-to-home position
    Goto,
    /// Delete the guard / return-to-home position
    Delete,
}
