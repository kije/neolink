use clap::{Parser, Subcommand};

/// The siren command controls the camera/hub siren.
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera. Must be a name in the config
    pub camera: String,
    /// Action to perform
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Subcommand, Debug)]
pub enum Action {
    /// Fire the siren now. Optional duration in seconds (0 = firmware default).
    On {
        /// Duration in seconds. `0` uses the firmware's default.
        #[arg(long, short = 'd', default_value_t = 0)]
        duration: u32,
    },
    /// Stop a currently-firing siren.
    Off,
    /// Configure the scheduled-siren window.
    Schedule {
        /// How many times the siren may fire per schedule window.
        #[arg(long, short = 't', default_value_t = 1)]
        times: u32,
        /// Duration of each siren in seconds.
        #[arg(long, short = 'd', default_value_t = 10)]
        duration: u32,
        /// Enable (default) or disable the schedule with `--disable`.
        #[arg(long)]
        disable: bool,
    },
    /// Watch the siren-status push (cmd 547) until Ctrl-C.
    Watch,
}
