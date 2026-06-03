use anyhow::{anyhow, Result};
use clap::Parser;

/// Parse the scene argument. Accepts:
///
/// - `off` or `disable` → `None` (disable scene mode)
/// - any `u8` numeric id → `Some(id)`
///
/// Conventional ids: `0`/`off` = disable, `1` = away, `2` = home,
/// `3` = disarm. Custom scenes use higher ids.
pub(crate) fn scene_parse(src: &str) -> Result<Option<u8>> {
    match src {
        "off" | "disable" | "disabled" | "none" => Ok(None),
        n => n.parse::<u8>().map(Some).map_err(|e| {
            anyhow!(
                "Could not parse scene id {:?}: {} (expected 'off' or a numeric id)",
                src,
                e
            )
        }),
    }
}

/// Control the Baichuan scene mode (host-level arming scenarios).
///
/// Conventional ids: `0` / `off` = disable, `1` = away, `2` = home,
/// `3` = disarm. With no argument the command lists the available
/// scene ids configured on the camera.
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera. Must be a name in the config.
    pub camera: String,
    /// Scene to activate. Use `off` to disable scene mode. Omit to list
    /// the available scene ids.
    #[arg(value_parser = scene_parse, action = clap::ArgAction::Set, name = "id|off")]
    pub scene: Option<Option<u8>>,
}
