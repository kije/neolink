use anyhow::{anyhow, Result};
use clap::Parser;

fn onoff_parse(src: &str) -> Result<bool> {
    match src {
        "true" | "on" | "yes" => Ok(true),
        "false" | "off" | "no" => Ok(false),
        _ => Err(anyhow!(
            "Could not understand {}, check your input, should be true/false, on/off or yes/no",
            src
        )),
    }
}

/// Control the Baichuan privacy-mode shutter on a supported camera.
///
/// Privacy mode is a hard lens-shutter / blackout state. While active the
/// camera's HTTP and ONVIF APIs are unresponsive; only the Baichuan TCP
/// socket stays live. Omit the on|off argument to query the current state.
#[derive(Parser, Debug)]
pub struct Opt {
    /// The name of the camera. Must be a name in the config.
    pub camera: String,
    /// Whether to turn privacy mode ON or OFF. Omit to read the current state.
    #[arg(value_parser = onoff_parse, action = clap::ArgAction::Set, name = "on|off")]
    pub on: Option<bool>,
}
