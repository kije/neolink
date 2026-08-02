//! Tests for the MPEG-TS muxer.
//!
//! These parse the muxer's output back with a separate, deliberately
//! literal reader written from ISO 13818-1 rather than from the muxer, so
//! that a shared misreading of the standard has to be made twice to go
//! unnoticed. Where go2rtc's demuxer (`pkg/mpegts/demuxer.go`) depends on
//! something in particular — the PMT arriving before any PES, the
//! `PES_packet_length` that lets it flush an audio frame without waiting
//! for the next one — there is a test named for it.

use super::*;
use std::collections::HashMap;

/// One reassembled access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    pid: u16,
    pts: u64,
    payload: Vec<u8>,
    /// Whether the packet that started this frame flagged random access.
    random_access: bool,
    /// `PES_packet_length` as written, zero meaning "unbounded".
    declared_length: u16,
    /// Whether a PAT/PMT pair was written immediately before this frame.
    preceded_by_psi: bool,
}

#[derive(Debug, Default)]
struct Parsed {
    pmt_pid: u16,
    pcr_pid: u16,
    /// `(pid, stream_type)` in PMT order.
    streams: Vec<(u16, u8)>,
    frames: Vec<Frame>,
    /// Every PCR seen, as `(packet index, value)`.
    pcrs: Vec<(usize, u64)>,
    /// Packet index of each PAT.
    pat_packets: Vec<usize>,
    /// Number of PMTs seen.
    pmt_count: usize,
}

/// Reassembly state for one elementary stream.
#[derive(Default)]
struct PesBuilder {
    pts: u64,
    payload: Vec<u8>,
    random_access: bool,
    declared_length: u16,
    preceded_by_psi: bool,
    open: bool,
}

/// Parse a whole TS byte stream.
///
/// Panics on anything malformed — a wrong sync byte, a bad section CRC, a
/// continuity counter that skips — because every one of those is a bug in
/// the muxer rather than a case a test wants to tolerate.
fn demux(ts: &[u8]) -> Parsed {
    assert_eq!(
        ts.len() % TS_PACKET_SIZE,
        0,
        "output must be a whole number of 188-byte packets"
    );

    let mut out = Parsed {
        // Unset until the PAT names it; no real PID is 0xFFFF.
        pmt_pid: 0xFFFF,
        ..Default::default()
    };
    let mut builders: HashMap<u16, PesBuilder> = HashMap::new();
    let mut continuity: HashMap<u16, u8> = HashMap::new();
    // Set by a PMT, cleared by the next frame that starts after it.
    let mut fresh_psi = false;

    for (index, packet) in ts.chunks_exact(TS_PACKET_SIZE).enumerate() {
        assert_eq!(packet[0], SYNC_BYTE, "packet {index} lost sync");

        let pusi = packet[1] & 0x40 != 0;
        let pid = u16::from_be_bytes([packet[1] & 0x1F, packet[2]]);
        let control = (packet[3] >> 4) & 0b11;
        assert_ne!(control, 0b00, "packet {index} has a reserved AFC value");

        let mut cursor = 4;
        let mut random_access = false;

        if control & 0b10 != 0 {
            let length = packet[4] as usize;
            assert!(
                length <= TS_PACKET_SIZE - 5,
                "packet {} adaptation field overruns the packet",
                index
            );
            if length > 0 {
                let flags = packet[5];
                random_access = flags & 0x40 != 0;
                if flags & 0x10 != 0 {
                    let pcr = &packet[6..12];
                    let base = (pcr[0] as u64) << 25
                        | (pcr[1] as u64) << 17
                        | (pcr[2] as u64) << 9
                        | (pcr[3] as u64) << 1
                        | (pcr[4] as u64) >> 7;
                    out.pcrs.push((index, base));
                }
            }
            cursor = 5 + length;
        }

        if control & 0b01 == 0 {
            continue;
        }

        // The continuity counter increments once per packet carrying a
        // payload, per PID, and wraps at 16.
        let counter = packet[3] & 0x0F;
        if let Some(previous) = continuity.insert(pid, counter) {
            assert_eq!(
                counter,
                (previous + 1) % 16,
                "packet {index} on pid {pid:#x} broke continuity"
            );
        }

        let body = &packet[cursor..];

        if pid == PAT_PID {
            assert!(pusi, "a PAT must start a payload unit");
            out.pat_packets.push(index);
            let section = read_section(body, 0x00);
            // Skip transport_stream_id, version and section numbers.
            for entry in section[5..].chunks_exact(4) {
                let program = u16::from_be_bytes([entry[0], entry[1]]);
                let target = u16::from_be_bytes([entry[2] & 0x1F, entry[3]]);
                if program != 0 {
                    out.pmt_pid = target;
                }
            }
        } else if pid == out.pmt_pid {
            assert!(pusi, "a PMT must start a payload unit");
            out.pmt_count += 1;
            fresh_psi = true;
            let section = read_section(body, 0x02);
            let rest = &section[5..];
            out.pcr_pid = u16::from_be_bytes([rest[0] & 0x1F, rest[1]]);
            let info_length = u16::from_be_bytes([rest[2] & 0x0F, rest[3]]) as usize;
            let mut streams = Vec::new();
            for entry in rest[4 + info_length..].chunks_exact(5) {
                let stream_pid = u16::from_be_bytes([entry[1] & 0x1F, entry[2]]);
                streams.push((stream_pid, entry[0]));
            }
            if out.streams.is_empty() {
                out.streams = streams;
            } else {
                assert_eq!(out.streams, streams, "the PMT changed mid-stream");
            }
        } else if out.streams.iter().any(|&(p, _)| p == pid) {
            let builder = builders.entry(pid).or_default();

            if pusi {
                flush(pid, builder, &mut out.frames);

                assert_eq!(&body[0..3], &[0x00, 0x00, 0x01], "bad PES start code");
                let declared_length = u16::from_be_bytes([body[4], body[5]]);
                assert_eq!(body[6] & 0xC0, 0x80, "bad PES marker bits");
                assert_eq!(body[7], 0x80, "expected a PTS and no DTS");
                let header_length = body[8] as usize;
                assert_eq!(header_length, 5, "expected a lone 5-byte PTS");

                builder.pts = read_timestamp(&body[9..14], 0b0010);
                builder.random_access = random_access;
                builder.declared_length = declared_length;
                builder.preceded_by_psi = std::mem::take(&mut fresh_psi);
                builder.payload.clear();
                builder
                    .payload
                    .extend_from_slice(&body[9 + header_length..]);
                builder.open = true;
            } else if builder.open {
                builder.payload.extend_from_slice(body);
            }

            // A declared length lets a reader finish the frame here
            // rather than waiting for the next one to start. This is the
            // path go2rtc takes for audio.
            let expected = builder.declared_length as usize;
            if expected != 0 && builder.payload.len() >= expected - 8 {
                builder.payload.truncate(expected - 8);
                flush(pid, builder, &mut out.frames);
            }
        }
    }

    for (pid, builder) in builders.iter_mut() {
        flush(*pid, builder, &mut out.frames);
    }

    out
}

fn flush(pid: u16, builder: &mut PesBuilder, frames: &mut Vec<Frame>) {
    if !builder.open {
        return;
    }
    frames.push(Frame {
        pid,
        pts: builder.pts,
        payload: std::mem::take(&mut builder.payload),
        random_access: builder.random_access,
        declared_length: builder.declared_length,
        preceded_by_psi: builder.preceded_by_psi,
    });
    builder.open = false;
}

/// Read one PSI section out of a packet payload, checking its CRC.
///
/// Returns the section body from `transport_stream_id` onwards, i.e.
/// everything after the table id and length, minus the CRC.
fn read_section(body: &[u8], expect_table: u8) -> Vec<u8> {
    let pointer = body[0] as usize;
    let section = &body[1 + pointer..];
    assert_eq!(section[0], expect_table, "unexpected table id");

    let header = u16::from_be_bytes([section[1], section[2]]);
    assert_eq!(header & 0x8000, 0x8000, "section_syntax_indicator not set");
    assert_eq!(header & 0x4000, 0, "private bit should be clear");
    let length = (header & 0x0FFF) as usize;

    let whole = &section[..3 + length];
    let (payload, crc) = whole.split_at(whole.len() - 4);
    assert_eq!(
        u32::from_be_bytes([crc[0], crc[1], crc[2], crc[3]]),
        crc32_mpeg2(payload),
        "section CRC mismatch"
    );

    // Everything after the 4-byte trailer of the section must be
    // stuffing, so nothing is silently dropped.
    assert!(
        section[3 + length..].iter().all(|&b| b == 0xFF),
        "PSI packet should be stuffed with 0xFF"
    );

    payload[3..].to_vec()
}

/// Inverse of [`write_timestamp`].
fn read_timestamp(bytes: &[u8], expect_prefix: u8) -> u64 {
    assert_eq!(bytes[0] >> 4, expect_prefix, "bad timestamp prefix");
    for (i, &byte) in bytes.iter().enumerate() {
        if i == 0 || i == 2 || i == 4 {
            assert_eq!(byte & 1, 1, "timestamp marker bit {i} not set");
        }
    }
    ((bytes[0] as u64 >> 1) & 0x07) << 30
        | (bytes[1] as u64) << 22
        | ((bytes[2] as u64) >> 1) << 15
        | (bytes[3] as u64) << 7
        | (bytes[4] as u64) >> 1
}

/// Microseconds to the muxer's 90 kHz clock, as `write_frame` does it.
fn ticks(us: u64) -> u64 {
    us * 9 / 100
}

#[test]
fn crc32_matches_the_mpeg2_check_value() {
    // The canonical check value for CRC-32/MPEG-2.
    assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_E6E7);
}

#[test]
fn output_is_whole_packets_starting_with_the_sync_byte() {
    let mut muxer = TsMuxer::new(&[StreamType::H264]);
    let video = muxer.track(StreamType::H264).unwrap();
    let mut out = Vec::new();
    muxer.write_frame(video, 0, true, &vec![0xAB; 5000], &mut out);

    assert_eq!(out.len() % TS_PACKET_SIZE, 0);
    assert!(out.chunks_exact(TS_PACKET_SIZE).all(|p| p[0] == SYNC_BYTE));
}

#[test]
fn the_pat_points_at_the_pmt_and_the_pmt_lists_every_track() {
    let mut muxer = TsMuxer::new(&[StreamType::H265, StreamType::Aac, StreamType::Pcma]);
    let mut out = Vec::new();
    muxer.write_psi(&mut out);

    let parsed = demux(&out);
    assert_eq!(parsed.pmt_pid, PMT_PID);
    assert_eq!(
        parsed.streams,
        vec![(0x100, 0x24), (0x101, 0x0F), (0x102, 0x90)],
        "stream types must be the ones go2rtc's demuxer switches on"
    );
    // The video track carries the clock.
    assert_eq!(parsed.pcr_pid, 0x100);
}

#[test]
fn an_audio_only_program_still_has_a_pcr_pid() {
    let mut muxer = TsMuxer::new(&[StreamType::Pcma]);
    let mut out = Vec::new();
    muxer.write_psi(&mut out);

    assert_eq!(demux(&out).pcr_pid, 0x100);
}

#[test]
fn a_frame_round_trips_with_its_payload_and_timestamp() {
    let mut muxer = TsMuxer::new(&[StreamType::H264]);
    let video = muxer.track(StreamType::H264).unwrap();
    let mut out = Vec::new();

    let payload: Vec<u8> = (0..700u32).map(|i| (i % 251) as u8).collect();
    muxer.write_frame(video, 1_000_000, true, &payload, &mut out);

    let parsed = demux(&out);
    assert_eq!(parsed.frames.len(), 1);
    let frame = &parsed.frames[0];
    assert_eq!(frame.payload, payload, "payload must survive packetisation");
    assert_eq!(frame.pts, ticks(1_000_000) + PTS_LEAD);
    assert!(frame.random_access);
}

#[test]
fn frames_larger_than_one_packet_reassemble_in_order() {
    let mut muxer = TsMuxer::new(&[StreamType::H264]);
    let video = muxer.track(StreamType::H264).unwrap();
    let mut out = Vec::new();

    // Well past the 0xFFFF that PES_packet_length can express, so this
    // also covers the unbounded-length path.
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
    muxer.write_frame(video, 0, true, &payload, &mut out);

    let parsed = demux(&out);
    assert_eq!(parsed.frames.len(), 1);
    assert_eq!(parsed.frames[0].payload, payload);
    assert_eq!(
        parsed.frames[0].declared_length, 0,
        "a frame too big for the length field must declare zero"
    );
}

#[test]
fn every_payload_length_packetises_exactly() {
    // The awkward sizes are the ones where the last packet needs an
    // adaptation field of exactly zero or one byte of stuffing.
    for size in 1..600usize {
        let mut muxer = TsMuxer::new(&[StreamType::Aac]);
        let audio = muxer.track(StreamType::Aac).unwrap();
        let mut out = Vec::new();

        let payload: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        muxer.write_frame(audio, 0, false, &payload, &mut out);

        let parsed = demux(&out);
        assert_eq!(parsed.frames.len(), 1, "size {size} produced no frame");
        assert_eq!(parsed.frames[0].payload, payload, "size {size} corrupted");
    }
}

#[test]
fn audio_frames_declare_their_length_so_a_reader_need_not_wait() {
    // go2rtc finishes a PES as soon as it has PES_packet_length bytes.
    // Without a length it holds the frame until the next one starts,
    // which for audio is a whole frame of added latency.
    let mut muxer = TsMuxer::new(&[StreamType::Aac]);
    let audio = muxer.track(StreamType::Aac).unwrap();
    let mut out = Vec::new();
    muxer.write_frame(audio, 0, false, &[0x42; 300], &mut out);

    let parsed = demux(&out);
    assert_eq!(parsed.frames[0].declared_length, (300 + 8) as u16);
}

#[test]
fn keyframes_are_flagged_and_carry_a_pcr() {
    let mut muxer = TsMuxer::new(&[StreamType::H264, StreamType::Aac]);
    let video = muxer.track(StreamType::H264).unwrap();
    let audio = muxer.track(StreamType::Aac).unwrap();
    let mut out = Vec::new();

    muxer.write_frame(video, 0, true, &[0x01; 100], &mut out);
    muxer.write_frame(audio, 0, false, &[0x02; 100], &mut out);
    muxer.write_frame(video, 40_000, false, &[0x03; 100], &mut out);

    let parsed = demux(&out);
    let video_pid = parsed.streams[0].0;

    let keyframes: Vec<_> = parsed.frames.iter().filter(|f| f.random_access).collect();
    assert_eq!(
        keyframes.len(),
        1,
        "only the I-frame is a random access point"
    );
    assert_eq!(keyframes[0].pid, video_pid);

    // Both video frames carry the clock; the audio frame does not.
    assert_eq!(parsed.pcrs.len(), 2, "every video frame should carry a PCR");
    let _ = video;
}

#[test]
fn pts_leads_the_pcr_so_a_decoder_has_somewhere_to_buffer() {
    let mut muxer = TsMuxer::new(&[StreamType::H264]);
    let video = muxer.track(StreamType::H264).unwrap();
    let mut out = Vec::new();
    muxer.write_frame(video, 500_000, true, &[0x7F; 50], &mut out);

    let parsed = demux(&out);
    let (_, pcr) = parsed.pcrs[0];
    assert_eq!(pcr, ticks(500_000));
    assert_eq!(parsed.frames[0].pts - pcr, PTS_LEAD);
}

#[test]
fn timestamps_survive_the_33_bit_wrap() {
    // Just under the wrap point, so PTS_LEAD pushes the PTS over it.
    let us = ((TIMESTAMP_MASK - 1000) * 100) / 9;
    let mut muxer = TsMuxer::new(&[StreamType::H264]);
    let video = muxer.track(StreamType::H264).unwrap();
    let mut out = Vec::new();
    muxer.write_frame(video, us, true, &[0x11; 40], &mut out);

    let parsed = demux(&out);
    let pts = parsed.frames[0].pts;
    assert!(pts <= TIMESTAMP_MASK, "PTS must stay inside 33 bits");
    assert_eq!(pts, (ticks(us) + PTS_LEAD) & TIMESTAMP_MASK);
}

#[test]
fn the_program_is_announced_before_any_frame() {
    // go2rtc ignores every PES until it has seen the PMT, so the tables
    // have to come first without the caller having to ask.
    let mut muxer = TsMuxer::new(&[StreamType::H264]);
    let video = muxer.track(StreamType::H264).unwrap();
    let mut out = Vec::new();
    muxer.write_frame(video, 0, true, &[0x01; 100], &mut out);

    let parsed = demux(&out);
    assert_eq!(parsed.pat_packets.first(), Some(&0));
    assert_eq!(parsed.pmt_count, 1);
    assert_eq!(parsed.frames.len(), 1);
}

#[test]
fn the_program_is_repeated_as_the_stream_runs() {
    let mut muxer = TsMuxer::new(&[StreamType::H264]);
    let video = muxer.track(StreamType::H264).unwrap();
    let mut out = Vec::new();

    // One second of P-frames at 25fps, no keyframes at all, so only the
    // interval can be driving the repeats.
    for i in 0..25u64 {
        muxer.write_frame(video, i * 40_000, i == 0, &[0x05; 200], &mut out);
    }

    let parsed = demux(&out);
    assert_eq!(parsed.frames.len(), 25);

    // The tables can only land on a frame boundary, so the guarantee is
    // "never more than the interval plus one frame" rather than an exact
    // cadence. Assert that, not a packet count.
    let frame_ticks = ticks(40_000);
    let mut last = None;
    for frame in &parsed.frames {
        if frame.preceded_by_psi {
            last = Some(frame.pts);
        }
        let since = frame.pts - last.expect("the first frame must carry the tables");
        assert!(
            since <= PSI_INTERVAL + frame_ticks,
            "went {since} ticks without a PMT, limit is {}",
            PSI_INTERVAL + frame_ticks
        );
    }
}

#[test]
fn empty_frames_are_dropped_rather_than_muxed() {
    let mut muxer = TsMuxer::new(&[StreamType::H264]);
    let video = muxer.track(StreamType::H264).unwrap();
    let mut out = Vec::new();
    muxer.write_frame(video, 0, true, &[], &mut out);

    assert!(out.is_empty(), "an empty frame should produce no packets");
}

#[test]
fn tracks_are_addressed_by_type() {
    let muxer = TsMuxer::new(&[StreamType::H265, StreamType::Pcma]);
    assert_eq!(muxer.track(StreamType::H265), Some(TrackId(0)));
    assert_eq!(muxer.track(StreamType::Pcma), Some(TrackId(1)));
    assert_eq!(muxer.track(StreamType::Aac), None);
    assert_eq!(muxer.track(StreamType::H264), None);
}

#[test]
fn interleaved_tracks_keep_their_own_continuity_and_timestamps() {
    let mut muxer = TsMuxer::new(&[StreamType::H264, StreamType::Aac]);
    let video = muxer.track(StreamType::H264).unwrap();
    let audio = muxer.track(StreamType::Aac).unwrap();
    let mut out = Vec::new();

    for i in 0..20u64 {
        muxer.write_frame(video, i * 40_000, i % 10 == 0, &[0xAA; 3000], &mut out);
        muxer.write_frame(audio, i * 40_000, false, &[0xBB; 400], &mut out);
    }

    // demux asserts continuity per PID as it goes.
    let parsed = demux(&out);
    let video_frames: Vec<_> = parsed.frames.iter().filter(|f| f.pid == 0x100).collect();
    let audio_frames: Vec<_> = parsed.frames.iter().filter(|f| f.pid == 0x101).collect();

    assert_eq!(video_frames.len(), 20);
    assert_eq!(audio_frames.len(), 20);
    assert!(video_frames.iter().all(|f| f.payload == vec![0xAA; 3000]));
    assert!(audio_frames.iter().all(|f| f.payload == vec![0xBB; 400]));

    for (i, frame) in video_frames.iter().enumerate() {
        assert_eq!(frame.pts, ticks(i as u64 * 40_000) + PTS_LEAD);
    }
}
