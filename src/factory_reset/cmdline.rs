use clap::Parser;

/// Restore the camera to factory defaults.
///
/// # WARNING - SCAFFOLDING ONLY
///
/// The exact Baichuan wire format for factory reset is NOT known. This
/// subcommand will refuse to actually transmit anything to the camera
/// until the cmd_id and XML payload have been captured from a real
/// Reolink-app reset session. See `docs/baichuan-lifecycle.md`.
///
/// Even once implemented, a factory reset is destructive (wipes
/// credentials, paired notifications, schedules, etc.), which is why
/// `--yes-i-am-sure` is required.
#[derive(Parser, Debug)]
pub struct Opt {
    /// Name of the camera (must match a `[[cameras]]` entry in the config).
    pub camera: String,

    /// Required acknowledgement that you understand this command is
    /// destructive and will (once implemented) wipe the camera's settings.
    #[arg(long = "yes-i-am-sure")]
    pub yes_i_am_sure: bool,

    /// If set, ask the camera to preserve its network configuration
    /// (IP, Wi-Fi credentials) across the reset.
    ///
    /// TODO: confirm via capture whether the underlying Baichuan message
    /// actually supports this. For now this flag only changes a field in
    /// the (placeholder) request XML.
    #[arg(long)]
    pub keep_network: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Wrap {
        #[command(flatten)]
        opt: Opt,
    }

    #[test]
    fn requires_camera() {
        assert!(Wrap::try_parse_from(["test"]).is_err());
    }

    #[test]
    fn yes_and_keep_network_default_off() {
        let parsed = Wrap::try_parse_from(["test", "cam"]).unwrap();
        assert!(!parsed.opt.yes_i_am_sure);
        assert!(!parsed.opt.keep_network);
        assert_eq!(parsed.opt.camera, "cam");
    }

    #[test]
    fn flags_parse() {
        let parsed =
            Wrap::try_parse_from(["test", "cam", "--yes-i-am-sure", "--keep-network"]).unwrap();
        assert!(parsed.opt.yes_i_am_sure);
        assert!(parsed.opt.keep_network);
    }
}
