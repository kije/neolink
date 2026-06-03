//! Handles I/O input contact messages (cmd 677)
//!
//! NVRs and hubs expose physical alarm-in terminals — door sensors, gate
//! contacts, etc. The camera pushes [`MSG_ID_IO_INPUT`] (cmd 677) whenever
//! the state of any input changes.
//!
//! The XML field names follow the `starkillerOG/reolink_aio` reference
//! and may need adjustment once tested against real firmware.

use tokio::sync::mpsc::{channel, Receiver};

use super::{BcCamera, Result};
use crate::bc::{model::*, xml::*};

impl BcCamera {
    /// Subscribe to I/O input state changes (cmd 677 push).
    ///
    /// Each push from the NVR / hub is forwarded as the parsed
    /// [`IoInputList`] payload. The list may contain one or more
    /// [`IoItem`] entries, one per input terminal that changed.
    pub async fn io_input_stream(&self) -> Result<Receiver<IoInputList>> {
        let (tx, rx) = channel(8);
        let connection = self.get_connection();
        connection
            .handle_msg(MSG_ID_IO_INPUT, move |bc| {
                let tx = tx.clone();
                Box::pin(async move {
                    if let Bc {
                        meta:
                            BcMeta {
                                msg_id: MSG_ID_IO_INPUT,
                                ..
                            },
                        body:
                            BcBody::ModernMsg(ModernMsg {
                                payload:
                                    Some(BcPayloads::BcXml(BcXml {
                                        io_input_list: Some(list),
                                        ..
                                    })),
                                ..
                            }),
                    } = bc
                    {
                        let _ = tx.send(list.clone()).await;
                    }
                    None
                })
            })
            .await?;
        Ok(rx)
    }

    /// Subscribe to I/O input state changes and yield individual
    /// `(index, state)` events.
    ///
    /// Convenience wrapper around [`Self::io_input_stream`] that flattens
    /// the [`IoInputList`] into per-input events. The boolean state is
    /// `true` when the input is active / closed (`result == 1`).
    pub async fn io_input_state_stream(&self) -> Result<Receiver<(u8, bool)>> {
        let mut list_rx = self.io_input_stream().await?;
        let (tx, rx) = channel(16);
        tokio::spawn(async move {
            while let Some(list) = list_rx.recv().await {
                for item in list.items {
                    if tx.send((item.index, item.result != 0)).await.is_err() {
                        return;
                    }
                }
            }
        });
        Ok(rx)
    }
}
