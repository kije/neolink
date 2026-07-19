use clap::Parser;

/// The io command watches the camera's I/O input contacts (cmd 677 push).
///
/// Each push from the NVR / hub is printed as `io <index> <on|off>`.
/// The command runs until Ctrl-C.
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera. Must be a name in the config
    pub camera: String,
}
