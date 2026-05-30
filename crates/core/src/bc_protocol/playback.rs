//! Recording playback / SD-card listing / alarm-video search / cover preview.
//!
//! This module exposes the Baichuan commands documented in
//! `apocaliss92/nodelink-js` and `starkillerOG/reolink_aio`. See
//! [`crate::bc::model`] for the new `MSG_ID_*` constants.
//!
//! The high-level surface is:
//!
//! - [`BcCamera::list_recordings`] — paginated SD-card listing
//! - [`BcCamera::find_alarm_videos`] — paginated alarm-video search
//! - [`BcCamera::day_records`] — which days in a month have recordings
//! - [`BcCamera::cover_preview`] — JPEG thumbnail at a given timestamp
//! - [`BcCamera::replay_stream`] — replay BcMedia stream from a clip
//! - [`BcCamera::download`] — download the raw BcMedia bytes for a clip
//!
//! `list_recordings` / `find_alarm_videos` walk the camera's pagination
//! session to completion and return a `Vec<FileInfo>`, always closing the
//! session before they return (even on error). For very large SD cards the
//! eager pattern keeps the public surface small; the [`MAX_PAGES`] cap
//! guards against a buggy firmware never reporting an empty page.
//!
//! ## Replay-mode partial-AES quirk (not yet handled)
//!
//! `apocaliss92/nodelink-js` notes that on some Reolink firmwares the
//! replay path encrypts only the *header* of P-frames while leaving the
//! payload in clear text. nodelink-js works around this by attempting
//! both candidate decodes and scoring them with `scoreBcMediaLike()`.
//!
//! We currently route replay frames through the existing
//! [`crate::bcmedia`] parser unchanged. On affected firmwares the parser
//! may reject some frames mid-stream. If you hit this, the workaround is
//! to add a fallback parser path that:
//!
//! 1. Parses the BcMedia header from the *decrypted* bytes (status quo).
//! 2. On magic-header mismatch, retries the parse using the *raw* bytes
//!    of the payload while still treating the header as decrypted.
//! 3. Picks whichever produced a plausible BcMedia frame.
//!
//! This is documented here so a future patch can add it once a real
//! camera is available for verification.

use super::{BcCamera, Error, Result};
use crate::{
    bc::{model::*, xml::*},
    bcmedia::model::BcMedia,
};
use bytes::Bytes;
use futures::stream::StreamExt;
use tokio::sync::mpsc::{channel, Receiver};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Default page size used by the paginated listing sessions. The cameras we
/// know of cap this at 255 entries; the value chosen here matches
/// `nodelink-js`.
pub const DEFAULT_LIST_PAGE_SIZE: u32 = 50;

/// The recording categories accepted by [`BcCamera::list_recordings`].
///
/// These are the values used by `reolink_aio`; not every firmware supports
/// every category but the camera silently ignores unknown filters.
pub const ALL_RECORD_TYPES: &str =
    "manual,sched,io,md,people,face,vehicle,dog_cat,visitor,other,package";

/// The alarm types accepted by [`BcCamera::find_alarm_videos`].
pub const ALL_ALARM_TYPES: &str =
    "md,pir,io,people,face,vehicle,dog_cat,visitor,other,package,cry,crossline,intrusion,loitering,legacy,loss";

/// Closed time range (inclusive) used by all paginated listing methods.
#[derive(Debug, Clone, Copy)]
pub struct PlaybackTimeRange {
    /// Start of the range (inclusive).
    pub start: PlaybackTime,
    /// End of the range (inclusive).
    pub end: PlaybackTime,
}

impl PlaybackTimeRange {
    /// Build a range from raw `PlaybackTime` end-points.
    pub fn new(start: PlaybackTime, end: PlaybackTime) -> Self {
        Self { start, end }
    }

    /// Span the entirety of a single day (00:00:00 to 23:59:59).
    pub fn whole_day(year: u16, month: u8, day: u8) -> Self {
        Self {
            start: PlaybackTime::start_of_day(year, month, day),
            end: PlaybackTime::end_of_day(year, month, day),
        }
    }
}

/// A handle on a clip suitable for use with [`BcCamera::replay_stream`] /
/// [`BcCamera::download`].
///
/// In practice the `name` is the SD-card file name as returned in
/// [`FileInfo::file_name`]; callers should treat this struct as opaque.
#[derive(Debug, Clone)]
pub struct RecordingHandle {
    /// File name as stored on the SD card.
    pub name: String,
    /// Stream the clip was recorded into. Re-sent in the replay/download
    /// requests to match the camera's expectations.
    pub stream_type: String,
    /// Channel the clip belongs to.
    pub channel_id: u8,
    /// Optional clip start, used when the camera asks for a seek time on
    /// REPLAY/DOWNLOAD requests.
    pub start: Option<PlaybackTime>,
    /// Optional clip end.
    pub end: Option<PlaybackTime>,
}

impl RecordingHandle {
    /// Build a handle from a [`FileInfo`] entry returned by the camera.
    pub fn from_file_info(channel_id: u8, info: &FileInfo) -> Self {
        Self {
            name: info.file_name.clone(),
            stream_type: info
                .stream_type
                .clone()
                .unwrap_or_else(|| "mainStream".to_string()),
            channel_id,
            start: info.start_time,
            end: info.end_time,
        }
    }
}

/// Maximum number of pages to walk before giving up.
///
/// Cameras return one page of `DEFAULT_LIST_PAGE_SIZE` items at a time, so
/// the default cap (200) is a few thousand recordings — well past anything
/// a single SD card holds. The cap exists so a buggy firmware can never
/// loop us forever.
pub const MAX_PAGES: usize = 200;

fn extract_file_info(msg: &Bc, is_file_info_list: bool) -> Vec<FileInfo> {
    let BcBody::ModernMsg(ModernMsg {
        payload: Some(BcPayloads::BcXml(xml)),
        ..
    }) = &msg.body
    else {
        return Vec::new();
    };
    if is_file_info_list {
        xml.file_info_list
            .as_ref()
            .map(|l| l.file_info.clone())
            .unwrap_or_default()
    } else {
        xml.find_alarm_video
            .as_ref()
            .map(|l| l.file_info.clone())
            .unwrap_or_default()
    }
}

fn extract_handle(msg: &Bc, is_file_info_list: bool) -> u32 {
    let BcBody::ModernMsg(ModernMsg {
        payload: Some(BcPayloads::BcXml(xml)),
        ..
    }) = &msg.body
    else {
        return 0;
    };
    if is_file_info_list {
        xml.file_info_list.as_ref().map(|l| l.handle).unwrap_or(0)
    } else {
        xml.find_alarm_video.as_ref().map(|l| l.handle).unwrap_or(0)
    }
}

/// A handle on a currently-running replay/download stream.
///
/// The data can be pulled with [`Self::next`]. The stream is stopped when
/// the value is dropped; you may also call [`Self::shutdown`] for an
/// explicit, awaitable stop.
pub struct ReplayStream {
    handle: Option<JoinHandle<Result<()>>>,
    rx: Receiver<Result<BcMedia>>,
    cancel: CancellationToken,
}

impl ReplayStream {
    /// Pull the next BcMedia frame. Returns `Err(Error::StreamFinished)`
    /// when the camera signals the end of the clip.
    pub async fn next(&mut self) -> Result<BcMedia> {
        if let Some(handle) = self.handle.as_mut() {
            if handle.is_finished() {
                self.cancel.cancel();
                let res = handle.await;
                if let Ok(Err(e)) = res {
                    return Err(e);
                }
                return Err(Error::StreamFinished);
            }
        } else {
            self.cancel.cancel();
            return Err(Error::StreamFinished);
        }
        match self.rx.recv().await {
            Some(Ok(media)) => Ok(media),
            Some(Err(e)) => Err(e),
            None => {
                self.cancel.cancel();
                Err(Error::StreamFinished)
            }
        }
    }

    /// Gracefully shut the stream down and send the stop command.
    pub async fn shutdown(&mut self) -> Result<()> {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            let _ = handle.await?;
        }
        Ok(())
    }
}

impl Drop for ReplayStream {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            let _gt = tokio::runtime::Handle::current().enter();
            tokio::task::spawn(async move {
                let _ = handle.await;
            });
        }
    }
}

impl BcCamera {
    /// Enumerate SD-card recordings.
    ///
    /// Walks the camera's paginated session (cmds 14/15/16) to completion
    /// and returns the full list of [`FileInfo`] entries. The session is
    /// always closed before returning (even on error).
    ///
    /// `record_types` is a comma-separated allow-list (see
    /// [`ALL_RECORD_TYPES`]); pass [`None`] for the kitchen-sink default.
    /// `stream_type` defaults to `mainStream`.
    pub async fn list_recordings(
        &self,
        channel_id: u8,
        time_range: PlaybackTimeRange,
        record_types: Option<&str>,
        stream_type: Option<&str>,
    ) -> Result<Vec<FileInfo>> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_FILE_INFO_LIST_OPEN, msg_num)
            .await?;

        let open = Bc::new_from_xml(
            BcMeta {
                msg_id: MSG_ID_FILE_INFO_LIST_OPEN,
                channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            BcXml {
                file_info_list: Some(FileInfoList {
                    version: xml_ver(),
                    channel_id,
                    handle: 0,
                    page_size: Some(DEFAULT_LIST_PAGE_SIZE),
                    stream_type: Some(stream_type.unwrap_or("mainStream").to_string()),
                    cmd_version: Some(1),
                    record_type: Some(record_types.unwrap_or(ALL_RECORD_TYPES).to_string()),
                    start_time: Some(time_range.start),
                    end_time: Some(time_range.end),
                    file_info: Vec::new(),
                }),
                ..Default::default()
            },
        );

        sub.send(open).await?;
        let msg = sub.recv().await?;
        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }
        let handle = extract_handle(&msg, true);
        let mut results: Vec<FileInfo> = extract_file_info(&msg, true);

        // Walk the rest of the pages with cmd 15.
        let mut pages = 0usize;
        while !results.is_empty() || pages == 0 {
            pages += 1;
            if pages > MAX_PAGES {
                log::warn!(
                    "list_recordings: aborting after {MAX_PAGES} pages, results may be incomplete"
                );
                break;
            }
            let page = self
                .paginate_get_file_info(channel_id, handle, true)
                .await?;
            if page.is_empty() {
                break;
            }
            results.extend(page);
        }

        // Always close, even if we hit an error above (best-effort).
        let _ = self
            .paginate_close(channel_id, handle, MSG_ID_FILE_INFO_LIST_CLOSE, true)
            .await;
        Ok(results)
    }

    /// Search for alarm-video clips.
    ///
    /// Walks the camera's paginated session (cmds 272/273/274) to completion
    /// and returns the full list of matching [`FileInfo`] entries.
    ///
    /// `alarm_types` is a comma-separated allow-list (see
    /// [`ALL_ALARM_TYPES`]). `stream_type` defaults to `mainStream`.
    pub async fn find_alarm_videos(
        &self,
        channel_id: u8,
        time_range: PlaybackTimeRange,
        alarm_types: Option<&str>,
        stream_type: Option<&str>,
    ) -> Result<Vec<FileInfo>> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_FIND_REC_VIDEO_OPEN, msg_num)
            .await?;

        let open = Bc::new_from_xml(
            BcMeta {
                msg_id: MSG_ID_FIND_REC_VIDEO_OPEN,
                channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            BcXml {
                find_alarm_video: Some(FindAlarmVideo {
                    version: xml_ver(),
                    channel_id,
                    handle: 0,
                    page_size: Some(DEFAULT_LIST_PAGE_SIZE),
                    stream_type: Some(stream_type.unwrap_or("mainStream").to_string()),
                    cmd_version: Some(0),
                    alarm_type: Some(alarm_types.unwrap_or(ALL_ALARM_TYPES).to_string()),
                    start_time: Some(time_range.start),
                    end_time: Some(time_range.end),
                    file_info: Vec::new(),
                }),
                ..Default::default()
            },
        );

        sub.send(open).await?;
        let msg = sub.recv().await?;
        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }
        let handle = extract_handle(&msg, false);
        let mut results: Vec<FileInfo> = extract_file_info(&msg, false);

        let mut pages = 0usize;
        while !results.is_empty() || pages == 0 {
            pages += 1;
            if pages > MAX_PAGES {
                log::warn!(
                    "find_alarm_videos: aborting after {MAX_PAGES} pages, results may be incomplete"
                );
                break;
            }
            let page = self
                .paginate_get_file_info(channel_id, handle, false)
                .await?;
            if page.is_empty() {
                break;
            }
            results.extend(page);
        }

        let _ = self
            .paginate_close(channel_id, handle, MSG_ID_FIND_REC_VIDEO_CLOSE, false)
            .await;
        Ok(results)
    }

    async fn paginate_get_file_info(
        &self,
        channel_id: u8,
        handle: u32,
        is_file_info_list: bool,
    ) -> Result<Vec<FileInfo>> {
        if handle == 0 {
            // The OPEN reply already returned an empty page; there is
            // nothing to paginate.
            return Ok(Vec::new());
        }
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let (get_cmd, payload) = if is_file_info_list {
            (
                MSG_ID_FILE_INFO_LIST_GET,
                BcXml {
                    file_info_list: Some(FileInfoList {
                        version: xml_ver(),
                        channel_id,
                        handle,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
        } else {
            (
                MSG_ID_FIND_REC_VIDEO_GET,
                BcXml {
                    find_alarm_video: Some(FindAlarmVideo {
                        version: xml_ver(),
                        channel_id,
                        handle,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
        };
        let mut sub = connection.subscribe(get_cmd, msg_num).await?;
        sub.send(Bc::new_from_xml(
            BcMeta {
                msg_id: get_cmd,
                channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            payload,
        ))
        .await?;
        let msg = sub.recv().await?;
        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }
        Ok(extract_file_info(&msg, is_file_info_list))
    }

    async fn paginate_close(
        &self,
        channel_id: u8,
        handle: u32,
        close_cmd: u32,
        is_file_info_list: bool,
    ) -> Result<()> {
        if handle == 0 {
            return Ok(());
        }
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let payload = if is_file_info_list {
            BcXml {
                file_info_list: Some(FileInfoList {
                    version: xml_ver(),
                    channel_id,
                    handle,
                    ..Default::default()
                }),
                ..Default::default()
            }
        } else {
            BcXml {
                find_alarm_video: Some(FindAlarmVideo {
                    version: xml_ver(),
                    channel_id,
                    handle,
                    ..Default::default()
                }),
                ..Default::default()
            }
        };
        let mut sub = connection.subscribe(close_cmd, msg_num).await?;
        sub.send(Bc::new_from_xml(
            BcMeta {
                msg_id: close_cmd,
                channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            payload,
        ))
        .await?;
        // Best-effort close.
        let _ = sub.recv().await;
        Ok(())
    }

    /// Query which days in a given `(year, month)` contain at least one
    /// recording. The returned [`DayRecords`] holds a list of populated
    /// days plus (when the firmware emits it) a per-day mask of recording
    /// categories.
    pub async fn day_records(&self, channel_id: u8, year: u16, month: u8) -> Result<DayRecords> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection
            .subscribe(MSG_ID_GET_DAY_RECORDS, msg_num)
            .await?;

        let request = Bc::new_from_xml(
            BcMeta {
                msg_id: MSG_ID_GET_DAY_RECORDS,
                channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            BcXml {
                day_records: Some(DayRecords {
                    version: xml_ver(),
                    channel_id,
                    year,
                    month,
                    stream_type: None,
                    days: Vec::new(),
                }),
                ..Default::default()
            },
        );
        sub.send(request).await?;
        let msg = sub.recv().await?;
        if msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }
        if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    day_records: Some(days),
                    ..
                })),
            ..
        }) = msg.body
        {
            Ok(days)
        } else {
            Err(Error::UnintelligibleReply {
                reply: std::sync::Arc::new(Box::new(msg)),
                why: "Expected DayRecords xml in the reply",
            })
        }
    }

    /// Fetch a JPEG thumbnail (cover) at a specific timestamp.
    ///
    /// `time` is a unix timestamp in seconds. The returned bytes are the
    /// raw JPEG payload, decodable by any standard image library.
    ///
    /// Internally this re-uses the multi-packet binary reassembly pattern
    /// from [`BcCamera::get_snapshot`]: the camera replies with metadata
    /// on cmd 298 then streams the JPEG body on cmd 138
    /// ([`MSG_ID_COVER_RESPONSE`]) with `binaryData = 1`.
    pub async fn cover_preview(
        &self,
        channel_id: u8,
        time: u64,
        stream_type: Option<&str>,
    ) -> Result<Bytes> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let mut sub = connection.subscribe(MSG_ID_COVER_PREVIEW, msg_num).await?;

        let request = Bc::new_from_xml(
            BcMeta {
                msg_id: MSG_ID_COVER_PREVIEW,
                channel_id,
                msg_num,
                stream_type: 0,
                response_code: 0,
                class: 0x6414,
            },
            BcXml {
                cover_preview: Some(CoverPreview {
                    version: xml_ver(),
                    channel_id,
                    stream_type: Some(stream_type.unwrap_or("mainStream").to_string()),
                    time,
                    file_name: None,
                    picture_size: None,
                }),
                ..Default::default()
            },
        );
        sub.send(request).await?;
        let metadata_msg = sub.recv().await?;
        if metadata_msg.meta.response_code != 200 {
            return Err(Error::CameraServiceUnavailable {
                id: metadata_msg.meta.msg_id,
                code: metadata_msg.meta.response_code,
            });
        }
        let expected_size = if let BcBody::ModernMsg(ModernMsg {
            payload:
                Some(BcPayloads::BcXml(BcXml {
                    cover_preview:
                        Some(CoverPreview {
                            picture_size: Some(sz),
                            ..
                        }),
                    ..
                })),
            ..
        }) = &metadata_msg.body
        {
            *sz as usize
        } else {
            0
        };

        // The camera now opens a binary-data stream on MSG_ID_COVER_RESPONSE
        // with its own msg_num. Mirror the snapshot pattern: drop the
        // metadata subscription and listen for ANY incoming message with
        // that id.
        drop(sub);
        let mut sub_bin = connection.subscribe_to_id(MSG_ID_COVER_RESPONSE).await?;

        let mut result: Vec<u8> = Vec::with_capacity(expected_size.max(1024));
        let mut msg = sub_bin.recv().await?;
        while msg.meta.response_code == 200 {
            if let BcBody::ModernMsg(ModernMsg {
                extension:
                    Some(Extension {
                        binary_data: Some(1),
                        ..
                    }),
                payload: Some(BcPayloads::Binary(data)),
            }) = msg.body
            {
                result.extend_from_slice(&data);
            } else {
                return Err(Error::UnintelligibleReply {
                    reply: std::sync::Arc::new(Box::new(msg)),
                    why: "Expected binary data for cover preview",
                });
            }
            msg = sub_bin.recv().await?;
        }
        if msg.meta.response_code == 201 {
            if let BcBody::ModernMsg(ModernMsg {
                extension:
                    Some(Extension {
                        binary_data: Some(1),
                        ..
                    }),
                payload,
            }) = msg.body
            {
                if let Some(BcPayloads::Binary(data)) = payload {
                    result.extend_from_slice(&data);
                }
            } else {
                return Err(Error::UnintelligibleReply {
                    reply: std::sync::Arc::new(Box::new(msg)),
                    why: "Expected binary data on cover-preview EOS",
                });
            }
        } else {
            return Err(Error::CameraServiceUnavailable {
                id: msg.meta.msg_id,
                code: msg.meta.response_code,
            });
        }

        if expected_size > 0 && result.len() != expected_size {
            log::debug!(
                "Cover preview size mismatch: got {} expected {}",
                result.len(),
                expected_size
            );
        }
        Ok(Bytes::from(result))
    }

    /// Begin replaying a previously-listed clip.
    ///
    /// The returned [`ReplayStream`] yields decoded [`BcMedia`] frames in
    /// the same shape as a live preview, so callers can feed them to the
    /// existing gstreamer / MP4 sinks unmodified.
    pub async fn replay_stream(&self, recording: &RecordingHandle) -> Result<ReplayStream> {
        self.start_playback_stream(recording, MSG_ID_FILE_INFO_LIST_REPLAY)
            .await
    }

    /// Download the raw video bytes for a previously-listed clip.
    ///
    /// Same surface as [`Self::replay_stream`]; the only difference is the
    /// initial command id (13 vs 5). Some firmwares treat the two
    /// identically, others throttle the replay path.
    pub async fn download(&self, recording: &RecordingHandle) -> Result<ReplayStream> {
        self.start_playback_stream(recording, MSG_ID_FILE_INFO_LIST_DOWNLOAD)
            .await
    }

    async fn start_playback_stream(
        &self,
        recording: &RecordingHandle,
        open_cmd: u32,
    ) -> Result<ReplayStream> {
        let connection = self.get_connection();
        let msg_num = self.new_message_num();
        let stop_num = self.new_message_num();
        let cancel = CancellationToken::new();
        let cancel_thread = cancel.clone();
        let (tx, rx) = channel::<Result<BcMedia>>(100);
        let channel_id = recording.channel_id;
        let stream_type = recording.stream_type.clone();
        let file_name = recording.name.clone();
        let start = recording.start;
        let end = recording.end;

        let handle = tokio::task::spawn(async move {
            let mut sub_open = connection.subscribe(open_cmd, msg_num).await?;
            let mut payload = FileInfoList {
                version: xml_ver(),
                channel_id,
                handle: 0,
                page_size: None,
                stream_type: Some(stream_type.clone()),
                cmd_version: Some(1),
                record_type: None,
                start_time: start,
                end_time: end,
                file_info: vec![FileInfo {
                    file_name: file_name.clone(),
                    file_size: None,
                    stream_type: Some(stream_type.clone()),
                    record_type: None,
                    start_time: start,
                    end_time: end,
                }],
            };
            // Some firmwares want the seek time on the OPEN; if not set,
            // default to the clip start.
            if payload.start_time.is_none() {
                payload.start_time = Some(PlaybackTime::default());
            }
            let open = Bc::new_from_xml(
                BcMeta {
                    msg_id: open_cmd,
                    channel_id,
                    msg_num,
                    stream_type: 0,
                    response_code: 0,
                    class: BC_CLASS_FILE_DOWNLOAD,
                },
                BcXml {
                    file_info_list: Some(payload),
                    ..Default::default()
                },
            );
            sub_open.send(open).await?;
            // The initial reply should ack the open (response_code 200).
            let ack = sub_open.recv().await?;
            if ack.meta.response_code != 200 {
                return Err(Error::CameraServiceUnavailable {
                    id: ack.meta.msg_id,
                    code: ack.meta.response_code,
                });
            }
            // After the ack the camera streams BcMedia payloads on
            // MSG_ID_FILE_INFO_LIST_DL_VIDEO; route via the existing
            // bcmedia stream helper to reassemble frames identically to a
            // live preview.
            let mut sub_video = connection
                .subscribe(MSG_ID_FILE_INFO_LIST_DL_VIDEO, msg_num)
                .await?;
            {
                let mut media_sub = sub_video.bcmedia_stream(false);
                tokio::select! {
                    _ = cancel_thread.cancelled() => {},
                    _ = async {
                        while let Some(bc_media) = media_sub.next().await {
                            if tx.send(bc_media).await.is_err() {
                                break;
                            }
                        }
                    } => {},
                }
            }

            // Best-effort stop. Send STOP on its own msg_num so the camera
            // doesn't try to associate it with the (now-closed) replay
            // session.
            let mut sub_stop = connection
                .subscribe(MSG_ID_FILE_INFO_LIST_STOP, stop_num)
                .await?;
            let stop = Bc::new_from_xml(
                BcMeta {
                    msg_id: MSG_ID_FILE_INFO_LIST_STOP,
                    channel_id,
                    msg_num: stop_num,
                    stream_type: 0,
                    response_code: 0,
                    class: BC_CLASS_FILE_DOWNLOAD,
                },
                BcXml {
                    file_info_list: Some(FileInfoList {
                        version: xml_ver(),
                        channel_id,
                        handle: 0,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            );
            sub_stop.send(stop).await?;
            tokio::select! {
                _ = sub_stop.recv() => {},
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {},
            }

            Ok(())
        });

        Ok(ReplayStream {
            handle: Some(handle),
            rx,
            cancel,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bc::xml::BcXml;

    #[test]
    fn playback_time_components_roundtrip() {
        let t = PlaybackTime::from_components(2024, 11, 26, 12, 34, 56);
        assert_eq!(t.year, 20241126);
        assert_eq!(t.hour, 123456);

        let s = PlaybackTime::start_of_day(2024, 11, 26);
        assert_eq!(s.year, 20241126);
        assert_eq!(s.hour, 0);

        let e = PlaybackTime::end_of_day(2024, 11, 26);
        assert_eq!(e.year, 20241126);
        assert_eq!(e.hour, 235959);
    }

    #[test]
    fn file_info_list_open_roundtrip() {
        let original = FileInfoList {
            version: xml_ver(),
            channel_id: 0,
            handle: 0,
            page_size: Some(50),
            stream_type: Some("mainStream".to_string()),
            cmd_version: Some(1),
            record_type: Some("md,people".to_string()),
            start_time: Some(PlaybackTime::start_of_day(2024, 11, 26)),
            end_time: Some(PlaybackTime::end_of_day(2024, 11, 26)),
            file_info: Vec::new(),
        };
        let bcxml = BcXml {
            file_info_list: Some(original.clone()),
            ..Default::default()
        };
        let bytes = bcxml.serialize(Vec::<u8>::new()).unwrap();
        let parsed = BcXml::try_parse(bytes.as_slice()).unwrap();
        assert_eq!(parsed.file_info_list.unwrap(), original);
    }

    #[test]
    fn find_alarm_video_roundtrip() {
        let original = FindAlarmVideo {
            version: xml_ver(),
            channel_id: 0,
            handle: 0,
            page_size: Some(50),
            stream_type: Some("mainStream".to_string()),
            cmd_version: Some(0),
            alarm_type: Some("md,people,vehicle".to_string()),
            start_time: Some(PlaybackTime::start_of_day(2024, 11, 26)),
            end_time: Some(PlaybackTime::end_of_day(2024, 11, 26)),
            file_info: Vec::new(),
        };
        let bcxml = BcXml {
            find_alarm_video: Some(original.clone()),
            ..Default::default()
        };
        let bytes = bcxml.serialize(Vec::<u8>::new()).unwrap();
        let parsed = BcXml::try_parse(bytes.as_slice()).unwrap();
        assert_eq!(parsed.find_alarm_video.unwrap(), original);
    }

    #[test]
    fn cover_preview_request_roundtrip() {
        let original = CoverPreview {
            version: xml_ver(),
            channel_id: 0,
            stream_type: Some("mainStream".to_string()),
            time: 1_700_000_000,
            file_name: None,
            picture_size: None,
        };
        let bcxml = BcXml {
            cover_preview: Some(original.clone()),
            ..Default::default()
        };
        let bytes = bcxml.serialize(Vec::<u8>::new()).unwrap();
        let parsed = BcXml::try_parse(bytes.as_slice()).unwrap();
        assert_eq!(parsed.cover_preview.unwrap(), original);
    }

    #[test]
    fn day_records_request_roundtrip() {
        let original = DayRecords {
            version: xml_ver(),
            channel_id: 0,
            year: 2024,
            month: 11,
            stream_type: None,
            days: Vec::new(),
        };
        let bcxml = BcXml {
            day_records: Some(original.clone()),
            ..Default::default()
        };
        let bytes = bcxml.serialize(Vec::<u8>::new()).unwrap();
        let parsed = BcXml::try_parse(bytes.as_slice()).unwrap();
        assert_eq!(parsed.day_records.unwrap(), original);
    }

    #[test]
    fn day_records_reply_with_days_roundtrip() {
        let original = DayRecords {
            version: xml_ver(),
            channel_id: 0,
            year: 2024,
            month: 11,
            stream_type: Some("mainStream".to_string()),
            days: vec![
                DayRecord {
                    day: 1,
                    mask: Some(7),
                },
                DayRecord {
                    day: 15,
                    mask: None,
                },
                DayRecord {
                    day: 26,
                    mask: Some(3),
                },
            ],
        };
        let bcxml = BcXml {
            day_records: Some(original.clone()),
            ..Default::default()
        };
        let bytes = bcxml.serialize(Vec::<u8>::new()).unwrap();
        let parsed = BcXml::try_parse(bytes.as_slice()).unwrap();
        assert_eq!(parsed.day_records.unwrap(), original);
    }

    #[test]
    fn file_info_with_metadata_roundtrip() {
        let original = FileInfoList {
            version: xml_ver(),
            channel_id: 0,
            handle: 42,
            page_size: None,
            stream_type: None,
            cmd_version: None,
            record_type: None,
            start_time: None,
            end_time: None,
            file_info: vec![FileInfo {
                file_name: "Mp4Record/2024-11-26/RecS01_20241126120000_120500_0_M.mp4".to_string(),
                file_size: Some(1234567),
                stream_type: Some("mainStream".to_string()),
                record_type: Some("md".to_string()),
                start_time: Some(PlaybackTime::from_components(2024, 11, 26, 12, 0, 0)),
                end_time: Some(PlaybackTime::from_components(2024, 11, 26, 12, 5, 0)),
            }],
        };
        let bcxml = BcXml {
            file_info_list: Some(original.clone()),
            ..Default::default()
        };
        let bytes = bcxml.serialize(Vec::<u8>::new()).unwrap();
        let parsed = BcXml::try_parse(bytes.as_slice()).unwrap();
        assert_eq!(parsed.file_info_list.unwrap(), original);
    }

    #[test]
    fn whole_day_range() {
        let r = PlaybackTimeRange::whole_day(2024, 11, 26);
        assert_eq!(r.start.year, 20241126);
        assert_eq!(r.start.hour, 0);
        assert_eq!(r.end.year, 20241126);
        assert_eq!(r.end.hour, 235959);
    }
}
