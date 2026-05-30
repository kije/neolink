//! Device-lifecycle commands: firmware upgrade and factory reset.
//!
//! # WARNING - SCAFFOLDING ONLY
//!
//! The exact Baichuan wire format for these commands is NOT known. The
//! constants in [`crate::bc::model`] (`MSG_ID_UPGRADE_BEGIN`,
//! `MSG_ID_UPGRADE_DATA`, `MSG_ID_UPGRADE_COMMIT`, `MSG_ID_FACTORY_RESET`)
//! and the XML placeholders in [`crate::bc::xml`] (`UpgradeReq`,
//! `UpgradeData`, `FactoryReset`) are deliberate placeholders. The
//! `dissector/baichuan.lua` plugin gives hints (cmd 67 = FW upgrade,
//! cmd 99 = factory default) but these have not been verified by capture.
//!
//! Because a wrong cmd_id on a destructive command can brick a camera,
//! both functions in this module perform pre-flight validation (file
//! existence, integrity hash, ability check) and then refuse to actually
//! send anything to the camera, returning [`Error::NotImplemented`].
//!
//! See `docs/baichuan-lifecycle.md` for the discovery checklist that must
//! be completed before this module is wired up to real transmissions.
use super::{BcCamera, Error, Result};
use log::*;
use std::path::Path;

/// Recommended chunk size for streaming firmware bytes to the camera.
///
/// TODO: confirm via capture. The Baichuan TCP framing has a 16-bit
/// `msg_num` field and per-frame overhead; 8 KiB is a typical default
/// for similar protocols and leaves plenty of headroom under any
/// reasonable per-frame cap.
pub const UPGRADE_CHUNK_SIZE: usize = 8 * 1024;

/// Pre-flight metadata gathered before attempting a firmware upgrade.
///
/// Returned by [`BcCamera::upgrade_firmware`] (via the error path while
/// the wire format is unknown) and useful for the discovery work — it
/// gives the implementer a concrete object to compare against a real
/// capture without needing to also redo the file-handling code.
#[derive(Debug, Clone)]
pub struct FirmwarePreflight {
    /// Absolute size of the firmware file in bytes.
    pub size: u64,
    /// Hex-encoded MD5 of the file contents.
    ///
    /// MD5 is used here because it is already in `neolink_core`'s
    /// dependency set; the real wire format may expect SHA-256 (the
    /// dissector and reference implementations are inconsistent), in
    /// which case the hashing call here should be replaced once the
    /// real format is confirmed.
    pub md5_hex: String,
    /// File name, if the path had one.
    pub file_name: Option<String>,
}

impl BcCamera {
    /// Upload a new firmware image to the camera.
    ///
    /// # WARNING - SCAFFOLDING ONLY
    ///
    /// This currently performs pre-flight validation and then returns
    /// [`Error::NotImplemented`]. See the module docs for why.
    ///
    /// The pre-flight checks are still useful as a sanity gate even
    /// before the real wire format is implemented:
    ///
    /// 1. The path exists and is a regular file.
    /// 2. The file is non-empty and not absurdly large.
    /// 3. The camera advertises an "upgrade" ability (best-effort —
    ///    cameras do not always expose this and the check is currently
    ///    informational only).
    pub async fn upgrade_firmware(&self, path: &Path) -> Result<()> {
        let _preflight = self.upgrade_firmware_preflight(path).await?;

        // -- INTENTIONALLY UNIMPLEMENTED --
        //
        // The real implementation would look approximately like:
        //
        //     let connection = self.get_connection();
        //     let msg_num = self.new_message_num();
        //     let mut sub = connection.subscribe(MSG_ID_UPGRADE_BEGIN, msg_num).await?;
        //     // ... send UpgradeReq XML, stream chunks at MSG_CLASS_FILE_DOWNLOAD, commit ...
        //
        // but with all four MSG_ID_UPGRADE_* constants currently set to 0
        // (placeholder) sending anything would be either a no-op or — worse —
        // could collide with an unrelated command on the camera's side and
        // brick the device. Refuse loudly until the values are known.
        error!(
            "upgrade_firmware: refusing to transmit — wire format is not yet \
             confirmed. See docs/baichuan-lifecycle.md for the discovery \
             checklist."
        );
        Err(Error::NotImplemented {
            what: "firmware upgrade (cmd_ids not yet captured)",
        })
    }

    /// Compute the pre-flight metadata for a firmware upgrade without
    /// transmitting anything.
    ///
    /// Useful for testing the file-handling path and for the discovery
    /// work — the implementer can compare the computed MD5/size against
    /// what shows up in the Wireshark capture's `<ConfigFileInfo>` XML.
    pub async fn upgrade_firmware_preflight(&self, path: &Path) -> Result<FirmwarePreflight> {
        // Ability check is informational while the command is not wired up;
        // log a warning rather than fail, because we cannot know the real
        // ability name from the surveyed reference code. TODO: confirm.
        if let Err(e) = self.has_ability_rw("upgrade").await {
            warn!(
                "upgrade_firmware: camera does not advertise 'upgrade' \
                 ability ({:?}). The ability name has not been confirmed; \
                 proceeding with the pre-flight anyway.",
                e
            );
        }

        let metadata = tokio::fs::metadata(path).await.map_err(|e| {
            error!("upgrade_firmware: cannot stat {:?}: {}", path, e);
            Error::from(e)
        })?;
        if !metadata.is_file() {
            return Err(Error::Other("firmware path is not a regular file"));
        }
        let size = metadata.len();
        if size == 0 {
            return Err(Error::Other("firmware file is empty"));
        }
        // Sanity cap at 256 MiB. Real Reolink .pak files are typically
        // single-digit MiB; anything an order of magnitude larger is
        // overwhelmingly likely to be a mistake.
        const MAX_FIRMWARE_BYTES: u64 = 256 * 1024 * 1024;
        if size > MAX_FIRMWARE_BYTES {
            return Err(Error::OtherString(format!(
                "firmware file is suspiciously large ({} bytes); \
                 refusing as a safety check",
                size
            )));
        }

        let bytes = tokio::fs::read(path).await.map_err(Error::from)?;
        let digest = md5::compute(&bytes);
        let md5_hex = format!("{:x}", digest);
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());

        Ok(FirmwarePreflight {
            size,
            md5_hex,
            file_name,
        })
    }

    /// Factory-reset the camera.
    ///
    /// # WARNING - SCAFFOLDING ONLY
    ///
    /// This currently checks the abilities list (best-effort) and then
    /// returns [`Error::NotImplemented`]. See the module docs for why.
    ///
    /// # Parameters
    ///
    /// * `keep_network` — if `true`, ask the camera to preserve its
    ///   network configuration across the reset. This is the option the
    ///   Reolink mobile app exposes; whether the underlying Baichuan
    ///   message has a corresponding field has not been confirmed.
    pub async fn factory_reset(&self, keep_network: bool) -> Result<()> {
        // Ability check is informational while the command is not wired up;
        // log a warning rather than fail. TODO: confirm ability name.
        if let Err(e) = self.has_ability_rw("restore").await {
            warn!(
                "factory_reset: camera does not advertise 'restore' \
                 ability ({:?}). The ability name has not been confirmed; \
                 proceeding anyway.",
                e
            );
        }

        warn!(
            "factory_reset: refusing to transmit (keep_network={}) — wire \
             format is not yet confirmed. See docs/baichuan-lifecycle.md \
             for the discovery checklist.",
            keep_network
        );
        Err(Error::NotImplemented {
            what: "factory reset (cmd_id not yet captured)",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `FirmwarePreflight` is `Debug + Clone` so it can be threaded through
    /// log macros and test fixtures. Compile-time check via construction.
    #[test]
    fn firmware_preflight_is_debug_clone() {
        let p = FirmwarePreflight {
            size: 1,
            md5_hex: "d41d8cd98f00b204e9800998ecf8427e".to_string(),
            file_name: Some("test.pak".to_string()),
        };
        let cloned = p.clone();
        let s = format!("{:?}", cloned);
        assert!(s.contains("d41d8cd98f00b204e9800998ecf8427e"));
        assert_eq!(p.size, cloned.size);
    }

    #[test]
    fn chunk_size_is_reasonable() {
        // Must be > 0 and small enough to fit in a single Baichuan frame
        // with comfortable headroom. The Baichuan TCP framing puts no hard
        // upper bound but practical reolink captures show frames well under
        // 64 KiB.
        assert!(UPGRADE_CHUNK_SIZE > 0);
        assert!(UPGRADE_CHUNK_SIZE <= 64 * 1024);
    }
}
