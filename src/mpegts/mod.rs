//! A small MPEG-TS muxer.
//!
//! This exists so that [`crate::stream`] can hand a camera's elementary
//! streams to another program over a pipe without going anywhere near
//! GStreamer. The camera already gives us exactly what MPEG-TS wants to
//! carry — Annex-B H264/H265 and ADTS-framed AAC — so muxing is the only
//! step between the Baichuan socket and a playable byte stream, and doing
//! it here keeps the whole `neolink stream` path available in builds
//! without the `gstreamer` feature.
//!
//! The output targets go2rtc's `exec:` pipe source, whose demuxer
//! (`pkg/mpegts/demuxer.go`) recognises the stream types written here:
//! H264 (`0x1B`), H265 (`0x24`), AAC (`0x0F`) and A-law PCM (`0x90`). It
//! is ordinary MPEG-TS otherwise — PAT/PMT, PES with 90 kHz PTS, and PCR
//! on the video PID — so `ffprobe`, `ffplay` and VLC read it too, which
//! matters mostly for being able to debug the thing by hand.
//!
//! What this deliberately does *not* do: B-frames (the Baichuan protocol
//! never sends any, so PTS == DTS and only PTS is written), multiple
//! programs, or descriptors beyond the bare stream list.

use std::iter::repeat_n;

/// Every MPEG-TS packet is exactly this long.
pub(crate) const TS_PACKET_SIZE: usize = 188;

/// Bytes left in a packet once the 4-byte transport header is written.
const TS_PAYLOAD_SIZE: usize = TS_PACKET_SIZE - 4;

const SYNC_BYTE: u8 = 0x47;

/// PID of the Program Association Table. Fixed by the standard.
const PAT_PID: u16 = 0x0000;
/// PID we publish the Program Map Table on. Arbitrary but conventional.
const PMT_PID: u16 = 0x1000;
/// First elementary stream PID; one per track from here up.
const FIRST_ES_PID: u16 = 0x0100;
/// "No PID", used for `PCR_PID` when there is nothing to put there.
const NULL_PID: u16 = 0x1FFF;

/// The single program we publish.
const PROGRAM_NUMBER: u16 = 1;

/// The three reserved bits that sit above every 13-bit PID field in a
/// PSI table. The standard says to set reserved bits, not clear them.
const RESERVED_3: u16 = 0b111 << 13;

/// Four reserved bits, two zeroed length bits and a ten-bit length of
/// zero: the encoding of "no descriptors here", used for both
/// `program_info_length` and `ES_info_length`.
const EMPTY_INFO_LENGTH: u16 = 0b1111 << 12;

/// The `payload_unit_start_indicator` bit, as it sits in the 16-bit word
/// it shares with the PID.
const PUSI: u16 = 1 << 14;

/// PES and PCR timestamps run on a 90 kHz clock.
const CLOCK_HZ: u64 = 90_000;

/// PTS/PCR are 33-bit and wrap roughly every 26.5 hours.
const TIMESTAMP_MASK: u64 = (1 << 33) - 1;

/// How far PTS runs ahead of PCR.
///
/// PCR says "this is the time now", PTS says "show this frame then", so
/// PTS must lead PCR by at least the decoder's buffering delay or a
/// strict player has to either stall or drop. Offsetting every PTS by a
/// fixed amount buys that headroom without making the first PCR negative.
/// It costs no real latency: nothing waits on the clock, the numbers are
/// simply labelled 200 ms apart, and go2rtc ignores PCR entirely and
/// works from PTS deltas.
const PTS_LEAD: u64 = CLOCK_HZ / 5;

/// Longest gap between PAT/PMT repeats, in 90 kHz ticks.
///
/// The standard asks for 100 ms. A consumer reading our pipe from the
/// start only ever needs the first copy, but repeating keeps the stream
/// well-formed for anything that joins late (`tail -f` on a dump file,
/// say) and costs two packets per tenth of a second.
const PSI_INTERVAL: u64 = CLOCK_HZ / 10;

/// An elementary stream type, as it appears in the PMT.
///
/// The values are from ISO 13818-1 except [`StreamType::Pcma`], which is
/// the private type go2rtc adopted from Tapo cameras and understands as
/// G.711 A-law (`StreamTypePCMATapo`, `pkg/mpegts/demuxer.go`). We use it
/// because it is the only audio go2rtc can hand to a WebRTC consumer
/// without transcoding.
///
/// A caveat on that last one, verified against both builds: go2rtc's
/// MPEG-TS *probe* only creates a media for the stream types it lists in
/// `Producer.probe`, and **1.9.14 — the current release — does not list
/// `StreamTypePCMATapo` there**, only H264, H265, AAC and Opus. The
/// constant exists for the Tapo-specific producer, and the generic TS
/// path grew support for it after that release. So an A-law track is
/// read by go2rtc built from master and silently ignored by 1.9.14 and
/// earlier — ignored harmlessly, since an unlisted type is skipped
/// rather than waited for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StreamType {
    /// H264 video, in Annex-B framing.
    H264,
    /// H265 video, in Annex-B framing.
    H265,
    /// AAC audio, in ADTS framing.
    Aac,
    /// G.711 A-law audio at 8 kHz, raw samples.
    Pcma,
}

impl StreamType {
    /// The `stream_type` byte written into the PMT.
    fn id(self) -> u8 {
        match self {
            StreamType::H264 => 0x1B,
            StreamType::H265 => 0x24,
            StreamType::Aac => 0x0F,
            StreamType::Pcma => 0x90,
        }
    }

    /// Whether this track carries video, which decides both the PES
    /// `stream_id` range and which track gets to carry the PCR.
    fn is_video(self) -> bool {
        matches!(self, StreamType::H264 | StreamType::H265)
    }

    /// The PES `stream_id`. The standard reserves `0xE0..=0xEF` for video
    /// and `0xC0..=0xDF` for audio; one of each is all we need.
    fn stream_id(self) -> u8 {
        if self.is_video() {
            0xE0
        } else {
            0xC0
        }
    }
}

/// Handle for one track of a [`TsMuxer`], returned by
/// [`TsMuxer::track`] and taken by [`TsMuxer::write_frame`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct TrackId(usize);

struct Track {
    pid: u16,
    kind: StreamType,
    /// Continuity counter, 4 bits, incremented per packet carrying
    /// payload on this PID.
    continuity: u8,
}

/// Muxes elementary stream frames into an MPEG-TS byte stream.
///
/// The track list is fixed at construction because the PMT it produces
/// has to be stable: a consumer reads the program's shape once, and
/// go2rtc pins its downstream WebRTC/MSE sessions to what it saw. Decide
/// the tracks up front, then only feed frames.
pub(crate) struct TsMuxer {
    tracks: Vec<Track>,
    /// Which track carries the PCR — video if there is any, else the
    /// first track, so the clock keeps ticking on an audio-only stream.
    pcr_track: Option<usize>,
    pat_continuity: u8,
    pmt_continuity: u8,
    /// PCR of the last PAT/PMT pair, for [`PSI_INTERVAL`].
    last_psi: Option<u64>,
}

impl TsMuxer {
    /// Build a muxer publishing exactly `kinds`, in order, on
    /// consecutive PIDs from [`FIRST_ES_PID`].
    pub(crate) fn new(kinds: &[StreamType]) -> Self {
        let tracks: Vec<Track> = kinds
            .iter()
            .enumerate()
            .map(|(i, &kind)| Track {
                pid: FIRST_ES_PID + i as u16,
                kind,
                continuity: 0,
            })
            .collect();

        let pcr_track = tracks
            .iter()
            .position(|t| t.kind.is_video())
            .or(if tracks.is_empty() { None } else { Some(0) });

        Self {
            tracks,
            pcr_track,
            pat_continuity: 0,
            pmt_continuity: 0,
            last_psi: None,
        }
    }

    /// The handle for the first track of this type, if it was declared.
    pub(crate) fn track(&self, kind: StreamType) -> Option<TrackId> {
        self.tracks.iter().position(|t| t.kind == kind).map(TrackId)
    }

    /// Append a PAT and a PMT.
    ///
    /// [`write_frame`](Self::write_frame) emits these before the first
    /// frame and repeats them afterwards, so there is normally no need to
    /// call this at all.
    ///
    /// In particular, resist the temptation to announce the program as
    /// soon as the process starts. go2rtc blocks indefinitely waiting for
    /// the first byte of the pipe, but once bytes arrive it gives the
    /// stream only `core.ProbeTimeout` (5 s) to show it one packet of
    /// every type the PMT declares. Staying quiet until there is a frame
    /// to send spends that window on frames instead of on waiting for a
    /// battery camera to wake up.
    pub(crate) fn write_psi(&mut self, out: &mut Vec<u8>) {
        let pat = self.pat_section();
        write_psi_packet(PAT_PID, &mut self.pat_continuity, &pat, out);
        let pmt = self.pmt_section();
        write_psi_packet(PMT_PID, &mut self.pmt_continuity, &pmt, out);
    }

    /// Append one access unit as a PES packet.
    ///
    /// `pts_us` is a presentation timestamp in microseconds on whatever
    /// timeline the caller is keeping; only differences matter. `payload`
    /// must already be in the framing the stream type implies — Annex-B
    /// for video, ADTS for AAC, raw samples for A-law.
    ///
    /// `random_access` marks a point a decoder can start from. Set it on
    /// video keyframes: it drives both the adaptation field's
    /// `random_access_indicator` and PAT/PMT repetition.
    pub(crate) fn write_frame(
        &mut self,
        track: TrackId,
        pts_us: u64,
        random_access: bool,
        payload: &[u8],
        out: &mut Vec<u8>,
    ) {
        if payload.is_empty() {
            return;
        }

        // The PES clock. PTS runs PTS_LEAD ahead of it; see PTS_LEAD.
        let clock = (pts_us.saturating_mul(9) / 100) & TIMESTAMP_MASK;
        let pts = (clock + PTS_LEAD) & TIMESTAMP_MASK;

        let is_pcr_track = self.pcr_track == Some(track.0);
        if self.psi_due(clock, random_access) {
            self.write_psi(out);
            self.last_psi = Some(clock);
        }

        let Some(track) = self.tracks.get_mut(track.0) else {
            return;
        };

        let mut header = [0u8; 14];
        // Packet start code prefix and stream id.
        header[0..3].copy_from_slice(&[0x00, 0x00, 0x01]);
        header[3] = track.kind.stream_id();

        // PES_packet_length counts everything after this field: the two
        // flag bytes, the header length byte, the 5-byte PTS and the
        // payload. Zero means "unbounded", which is legal for video only
        // and makes the consumer wait for the next packet to know the
        // frame ended — so set a real length whenever one fits.
        let body = 3 + 5 + payload.len();
        let length = if body <= u16::MAX as usize {
            body as u16
        } else {
            0
        };
        header[4..6].copy_from_slice(&length.to_be_bytes());

        header[6] = 0x80; // '10' marker, no scrambling, no priority
        header[7] = 0x80; // PTS present, DTS absent
        header[8] = 5; // PES_header_data_length: just the PTS
        write_timestamp(&mut header[9..14], 0b0010, pts);

        let pcr = is_pcr_track.then_some(clock);
        emit_packets(track, Chain::new(&header, payload), pcr, random_access, out);
    }

    /// Whether a PAT/PMT pair is due before the frame at `clock`.
    ///
    /// Keyframes always get one so that a consumer joining at a random
    /// access point has the program table immediately in front of it.
    fn psi_due(&self, clock: u64, random_access: bool) -> bool {
        match self.last_psi {
            None => true,
            Some(last) => {
                random_access || (clock.wrapping_sub(last) & TIMESTAMP_MASK) >= PSI_INTERVAL
            }
        }
    }

    /// The PAT section body, from `table_id` to the last byte before the
    /// CRC (which [`write_psi_packet`] appends).
    fn pat_section(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(4);
        body.extend_from_slice(&PROGRAM_NUMBER.to_be_bytes());
        body.extend_from_slice(&(RESERVED_3 | PMT_PID).to_be_bytes());
        psi_section(0x00, PROGRAM_NUMBER, &body)
    }

    /// The PMT section body, likewise without its CRC.
    fn pmt_section(&self) -> Vec<u8> {
        let pcr_pid = self
            .pcr_track
            .and_then(|i| self.tracks.get(i))
            .map(|t| t.pid)
            .unwrap_or(NULL_PID);

        let mut body = Vec::with_capacity(4 + self.tracks.len() * 5);
        body.extend_from_slice(&(RESERVED_3 | pcr_pid).to_be_bytes());
        // Four reserved bits, two zeroed length bits, then a
        // program_info_length of zero: no program level descriptors.
        body.extend_from_slice(&EMPTY_INFO_LENGTH.to_be_bytes());

        for track in &self.tracks {
            body.push(track.kind.id());
            body.extend_from_slice(&(RESERVED_3 | track.pid).to_be_bytes());
            // As above, but ES_info_length: no per stream descriptors.
            body.extend_from_slice(&EMPTY_INFO_LENGTH.to_be_bytes());
        }

        psi_section(0x02, PROGRAM_NUMBER, &body)
    }
}

/// Wrap a PSI table body in the section header every table shares.
///
/// Returns everything from `table_id` up to but excluding the CRC.
fn psi_section(table_id: u8, extension: u16, body: &[u8]) -> Vec<u8> {
    let mut section = Vec::with_capacity(8 + body.len() + 4);
    section.push(table_id);

    // section_length counts from just after this field to the end of the
    // CRC: the five bytes below, the body, and four bytes of CRC.
    let length = 5 + body.len() as u16 + 4;
    // section_syntax_indicator, a zeroed private bit, two reserved bits,
    // then the 12-bit length.
    section.extend_from_slice(&(0b1011 << 12 | length).to_be_bytes());

    section.extend_from_slice(&extension.to_be_bytes());
    // Two reserved bits, version_number zero, current_next_indicator set.
    const RESERVED_2: u8 = 0b11 << 6;
    const CURRENT: u8 = 1;
    section.push(RESERVED_2 | CURRENT);
    section.push(0); // section_number
    section.push(0); // last_section_number
    section.extend_from_slice(body);
    section
}

/// Write one PSI section as a single TS packet.
///
/// Both our tables fit comfortably inside one packet, so there is no
/// section spanning to handle: pointer field, section, CRC, then 0xFF
/// stuffing to the packet boundary.
fn write_psi_packet(pid: u16, continuity: &mut u8, section: &[u8], out: &mut Vec<u8>) {
    let start = out.len();

    out.push(SYNC_BYTE);
    // payload_unit_start_indicator set; adaptation_field_control = 01.
    out.extend_from_slice(&(PUSI | pid).to_be_bytes());
    out.push(0b0001_0000 | (*continuity & 0x0F));
    *continuity = continuity.wrapping_add(1);

    out.push(0); // pointer_field: the section starts right here
    out.extend_from_slice(section);
    out.extend_from_slice(&crc32_mpeg2(section).to_be_bytes());

    let written = out.len() - start;
    out.extend(repeat_n(0xFF, TS_PACKET_SIZE - written));
}

/// Split a PES packet across TS packets on one PID.
///
/// `pcr` and `random_access` apply to the first packet only — they
/// describe the access unit, and the adaptation field that carries them
/// belongs with its start.
fn emit_packets(
    track: &mut Track,
    mut chain: Chain<'_>,
    mut pcr: Option<u64>,
    mut random_access: bool,
    out: &mut Vec<u8>,
) {
    let mut start = true;

    while !chain.is_empty() {
        // An adaptation field is needed to carry the PCR and the random
        // access flag, and again at the end of a frame to stuff the last
        // packet out to 188 bytes.
        let flagged = random_access || pcr.is_some();
        let reserved = if flagged {
            2 + if pcr.is_some() { 6 } else { 0 }
        } else {
            0
        };
        let take = chain.len().min(TS_PAYLOAD_SIZE - reserved);
        let field = TS_PAYLOAD_SIZE - take;

        out.push(SYNC_BYTE);
        let pusi = if start { PUSI } else { 0 };
        out.extend_from_slice(&(pusi | track.pid).to_be_bytes());
        let control = if field > 0 { 0b11 } else { 0b01 };
        out.push(control << 4 | (track.continuity & 0x0F));
        track.continuity = track.continuity.wrapping_add(1);

        if field == 1 {
            // No room for flags, and none needed: a lone length byte of
            // zero is the standard way to spend exactly one byte.
            out.push(0);
        } else if field > 1 {
            out.push((field - 1) as u8);
            let mut flags = 0u8;
            if random_access {
                flags |= 0x40;
            }
            if pcr.is_some() {
                flags |= 0x10;
            }
            out.push(flags);
            let mut used = 2;
            if let Some(pcr) = pcr {
                write_pcr(out, pcr);
                used += 6;
            }
            out.extend(repeat_n(0xFF, field - used));
        }

        chain.take(take, out);

        start = false;
        random_access = false;
        pcr = None;
    }
}

/// Write a 33-bit timestamp in the 5-byte PES form.
///
/// `prefix` is the 4-bit tag the standard puts in front: `0b0010` for a
/// lone PTS, `0b0011`/`0b0001` for a PTS/DTS pair we never emit.
fn write_timestamp(out: &mut [u8], prefix: u8, ts: u64) {
    out[0] = prefix << 4 | ((ts >> 30) as u8 & 0x07) << 1 | 1;
    out[1] = (ts >> 22) as u8;
    out[2] = ((ts >> 15) as u8 & 0x7F) << 1 | 1;
    out[3] = (ts >> 7) as u8;
    out[4] = (ts as u8 & 0x7F) << 1 | 1;
}

/// Write a Program Clock Reference: a 33-bit 90 kHz base, six reserved
/// bits, and a 9-bit 27 MHz extension we leave at zero.
fn write_pcr(out: &mut Vec<u8>, base: u64) {
    out.push((base >> 25) as u8);
    out.push((base >> 17) as u8);
    out.push((base >> 9) as u8);
    out.push((base >> 1) as u8);
    out.push((base as u8 & 1) << 7 | 0x7E);
    out.push(0);
}

/// CRC-32/MPEG-2: polynomial 0x04C11DB7, all ones in, no reflection and
/// no final xor. Every PSI section ends with one, big-endian.
fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// A cursor over the PES header and its payload as one logical run of
/// bytes, so a frame is copied into the output once rather than being
/// concatenated into a scratch buffer first.
struct Chain<'a> {
    parts: [&'a [u8]; 2],
}

impl<'a> Chain<'a> {
    fn new(head: &'a [u8], tail: &'a [u8]) -> Self {
        Self {
            parts: [head, tail],
        }
    }

    fn len(&self) -> usize {
        self.parts[0].len() + self.parts[1].len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Move the next `count` bytes into `out`.
    fn take(&mut self, mut count: usize, out: &mut Vec<u8>) {
        for part in self.parts.iter_mut() {
            let n = count.min(part.len());
            out.extend_from_slice(&part[..n]);
            *part = &part[n..];
            count -= n;
            if count == 0 {
                return;
            }
        }
    }
}

#[cfg(test)]
mod roundtrip;
#[cfg(test)]
mod tests;
