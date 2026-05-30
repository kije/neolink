use clap::Parser;
use std::path::PathBuf;
use std::str::FromStr;

/// Upload a firmware image to the camera and trigger a flash.
///
/// # WARNING - SCAFFOLDING ONLY
///
/// The exact Baichuan wire format for firmware upgrade is NOT known. This
/// subcommand will refuse to actually transmit anything to the camera until
/// the cmd_ids and XML payload have been captured from a real Reolink-app
/// upgrade session. See `docs/baichuan-lifecycle.md`.
///
/// Even once implemented, getting this wrong can brick a camera, which is
/// why `--yes-i-am-sure` is required.
#[derive(Parser, Debug)]
pub struct Opt {
    /// Name of the camera (must match a `[[cameras]]` entry in the config).
    pub camera: String,

    /// Path to the firmware `.pak` file to upload.
    #[arg(value_parser = PathBuf::from_str)]
    pub firmware: PathBuf,

    /// Required acknowledgement that you understand this command is
    /// destructive and will (once implemented) flash the camera.
    #[arg(long = "yes-i-am-sure")]
    pub yes_i_am_sure: bool,

    /// Run pre-flight checks only (validate the file, compute its hash)
    /// without attempting to talk to the camera.
    ///
    /// While the wire format is unconfirmed this is effectively the only
    /// supported mode of operation; running without `--dry-run` will
    /// currently fail with `NotImplemented`.
    #[arg(long)]
    pub dry_run: bool,
}
