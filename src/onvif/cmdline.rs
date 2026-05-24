use clap::Parser;

/// The `onvif` command starts the ONVIF Profile S bridge.
///
/// All cameras with `enabled = true` and `onvif.enabled = true` in the config
/// are exposed as virtual ONVIF Profile S devices. The global `[onvif]` block
/// controls the listening port and WS-Discovery behaviour.
#[derive(Parser, Debug, Default)]
pub struct Opt {}
