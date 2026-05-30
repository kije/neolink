use clap::Parser;

/// The wifi command prints the camera's WiFi info as XML.
///
/// Without `--scan`, only the currently associated SSID and signal
/// strength are fetched (cmd 115). With `--scan` the full WiFi scan list
/// is returned (cmd 116).
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera. Must be a name in the config
    pub camera: String,
    /// Perform a full WiFi scan and return the AP list.
    #[arg(long, short = 's')]
    pub scan: bool,
}
