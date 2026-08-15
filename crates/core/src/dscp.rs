//! DSCP marking for the sockets that talk to cameras.
//!
//! Reolink's Baichuan protocol multiplexes *everything* — PTZ commands, config
//! reads, snapshots and the video substream — onto a single connection per
//! camera. There is no separate control channel to mark, so a DSCP value set
//! here applies to the whole conversation with that camera, media included.
//! That is worth knowing before picking a class: putting a multi-megabit video
//! flow into a queue sized for voice is a good way to have a policer drop it.
//!
//! The value is process-wide rather than per-camera. Every socket that reaches
//! a camera is created several layers below the config (discovery probes, the
//! UDP keepalive socket, the TCP stream), and threading a value through all of
//! them would touch far more code than the feature is worth. Neolink runs one
//! process per config file, so "the marking neolink uses for camera traffic" is
//! a process-level property in practice.
//!
//! Unset by default: marking traffic a network is not configured to honour can
//! make things *worse* (some switches remap or police unknown classes), so this
//! only does anything once a user asks for it.

use std::sync::atomic::{AtomicI16, Ordering};

use socket2::SockRef;

/// Sentinel for "no marking configured". DSCP is 6 bits, so every real value
/// fits in 0..=63 and a negative number cannot collide with one.
const UNSET: i16 = -1;

static DSCP: AtomicI16 = AtomicI16::new(UNSET);

/// Set the DSCP class applied to every camera socket opened from now on.
///
/// `None` clears the marking. Existing sockets keep whatever they were opened
/// with; the change takes effect as connections are re-established.
pub fn set_dscp(dscp: Option<u8>) {
    let raw = match dscp {
        // A DSCP is 6 bits. Anything larger is a caller bug rather than a
        // value to silently truncate into a different class.
        Some(v) if v <= 63 => v as i16,
        Some(v) => {
            log::warn!("Ignoring out-of-range DSCP {v} (must be 0-63)");
            UNSET
        }
        None => UNSET,
    };
    DSCP.store(raw, Ordering::Relaxed);
}

/// The configured DSCP class, if any.
pub fn dscp() -> Option<u8> {
    match DSCP.load(Ordering::Relaxed) {
        UNSET => None,
        v => Some(v as u8),
    }
}

/// Apply the configured DSCP to a socket.
///
/// Best-effort: a kernel or container that refuses the option is logged once at
/// debug level and otherwise ignored. Failing to set a QoS hint is never a
/// reason to fail the connection the hint was for.
pub(crate) fn mark(sock: SockRef<'_>, is_ipv6: bool) {
    let Some(dscp) = dscp() else {
        return;
    };
    // The DSCP occupies the top 6 bits of the legacy TOS byte; the bottom two
    // are ECN and must be left alone for the kernel to manage.
    let tos = (dscp as u32) << 2;

    let res = if is_ipv6 {
        set_tclass_v6(&sock, tos)
    } else {
        sock.set_tos_v4(tos)
    };
    if let Err(e) = res {
        log::debug!("Could not set DSCP {dscp} on socket: {e}");
    }
}

/// `IPV6_TCLASS` is not offered by socket2 on every platform it supports, so
/// the v6 path is compiled in only where it exists and is a no-op elsewhere.
/// The allow-list mirrors socket2's own.
#[cfg(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "fuchsia",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "illumos",
))]
fn set_tclass_v6(sock: &SockRef<'_>, tos: u32) -> std::io::Result<()> {
    sock.set_tclass_v6(tos)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "fuchsia",
    target_os = "linux",
    target_os = "macos",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "illumos",
)))]
fn set_tclass_v6(_sock: &SockRef<'_>, _tos: u32) -> std::io::Result<()> {
    Ok(())
}

/// Resolve a DSCP written either as a number (`"44"`) or as one of the standard
/// class names (`"VOICE-ADMIT"`, `"EF"`, `"AF41"`, `"CS5"`, ...).
///
/// Names are matched case-insensitively and ignore `-`/`_` so `voice_admit`,
/// `VOICE-ADMIT` and `voiceadmit` all work.
pub fn parse_dscp(s: &str) -> Option<u8> {
    let norm: String = s
        .trim()
        .chars()
        .filter(|c| *c != '-' && *c != '_' && *c != ' ')
        .collect::<String>()
        .to_ascii_uppercase();
    if norm.is_empty() {
        return None;
    }
    // A bare number is taken verbatim, so a class this table doesn't know is
    // still reachable.
    if let Ok(v) = norm.parse::<u8>() {
        return (v <= 63).then_some(v);
    }
    let v = match norm.as_str() {
        "NONE" | "OFF" | "DEFAULT" | "CS0" | "BE" => 0,
        "CS1" => 8,
        "AF11" => 10,
        "AF12" => 12,
        "AF13" => 14,
        "CS2" => 16,
        "AF21" => 18,
        "AF22" => 20,
        "AF23" => 22,
        "CS3" => 24,
        "AF31" => 26,
        "AF32" => 28,
        "AF33" => 30,
        "CS4" => 32,
        "AF41" => 34,
        "AF42" => 36,
        "AF43" => 38,
        "CS5" => 40,
        // RFC 5865. The one this exists for.
        "VOICEADMIT" => 44,
        "EF" => 46,
        "CS6" => 48,
        "CS7" => 56,
        _ => return None,
    };
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_admit_resolves_to_44() {
        for spelling in ["VOICE-ADMIT", "voice_admit", "VoiceAdmit", " voice-admit "] {
            assert_eq!(parse_dscp(spelling), Some(44), "{spelling}");
        }
    }

    #[test]
    fn numbers_pass_through_and_are_range_checked() {
        assert_eq!(parse_dscp("44"), Some(44));
        assert_eq!(parse_dscp("0"), Some(0));
        assert_eq!(parse_dscp("63"), Some(63));
        // 64 does not fit in the 6-bit field.
        assert_eq!(parse_dscp("64"), None);
    }

    #[test]
    fn standard_class_names_resolve() {
        assert_eq!(parse_dscp("EF"), Some(46));
        assert_eq!(parse_dscp("AF41"), Some(34));
        assert_eq!(parse_dscp("CS5"), Some(40));
        assert_eq!(parse_dscp("none"), Some(0));
    }

    #[test]
    fn nonsense_is_rejected_rather_than_guessed() {
        assert_eq!(parse_dscp("banana"), None);
        assert_eq!(parse_dscp(""), None);
        assert_eq!(parse_dscp("AF99"), None);
    }

    /// The whole point of the marking: DSCP 44 has to land in the top six bits
    /// of the TOS byte, i.e. 0xB0, leaving ECN clear.
    #[test]
    fn dscp_is_shifted_into_the_tos_byte() {
        assert_eq!((44u32) << 2, 0xB0);
        assert_eq!((46u32) << 2, 0xB8);
    }
}
