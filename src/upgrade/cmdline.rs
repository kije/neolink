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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Wrapper so clap accepts the bare subcommand args in tests.
    #[derive(Parser, Debug)]
    struct Wrap {
        #[command(flatten)]
        opt: Opt,
    }

    #[test]
    fn requires_camera_and_firmware_path() {
        // Missing positional args must fail to parse.
        assert!(Wrap::try_parse_from(["test"]).is_err());
        assert!(Wrap::try_parse_from(["test", "cam"]).is_err());
    }

    #[test]
    fn yes_i_am_sure_defaults_off() {
        let parsed = Wrap::try_parse_from(["test", "cam", "fw.pak"]).unwrap();
        assert!(!parsed.opt.yes_i_am_sure);
        assert!(!parsed.opt.dry_run);
        assert_eq!(parsed.opt.camera, "cam");
        assert_eq!(parsed.opt.firmware.to_string_lossy(), "fw.pak");
    }

    #[test]
    fn yes_flag_and_dry_run_parse() {
        let parsed =
            Wrap::try_parse_from(["test", "cam", "fw.pak", "--yes-i-am-sure", "--dry-run"])
                .unwrap();
        assert!(parsed.opt.yes_i_am_sure);
        assert!(parsed.opt.dry_run);
    }
}
