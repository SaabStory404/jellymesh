//! Streaming MPEG-TS rewriter for the DV7 -> 8.1 pipeline (`transcode/docs/PLAN.md` P5).
//!
//! ffmpeg#1 stream-copies the source's video track into MPEG-TS on stdout. TS is the transport
//! because it carries HEVC as Annex-B (what `dv81::convert_access_unit` works on) *with* each
//! frame's PTS/DTS, so ffmpeg#2 gets the source's real timestamps and B-frame order back -- a raw
//! `-f hevc` pipe would make ffmpeg#2 invent timestamps from a fixed frame rate.
//!
//! This rewriter, fed ffmpeg#1's bytes in arbitrary chunks:
//! - passes every non-video packet through unchanged, except the PMT;
//! - rewrites the PMT so the HEVC stream carries exactly one Dolby Vision video stream
//!   descriptor (tag 0xB0), the one this job wants (profile 8, BL compat 1, no EL). Any 0xB0
//!   ffmpeg#1 wrote itself (it describes profile 7) is removed first. ffmpeg#2's mpegts demuxer
//!   turns that descriptor into the stream's DOVI configuration record, and its mp4/HLS muxer
//!   (with `-strict unofficial`) writes the matching `dvcC`/`dvvC` box -- MEASURED 2026-09-27 on
//!   ffmpeg 8.1.3 (Fedora): ffprobe of the HLS fmp4 init segment shows `DOVI configuration record:
//!   version: 1.0, profile: 8, ... compatibility id: 1`, no "Generating one" warning;
//! - reassembles each video PES, runs its payload through the caller's transform, and
//!   re-packetizes it with the original PES header (length set to 0, legal for video), the first
//!   packet's adaptation field (PCR / random-access flag) kept, and its own continuity counter.
//!
//! A video PES is emitted when the next one starts (or on `finish`), so output lags input by
//! one frame. Pure (no I/O), so every byte-level rule here is unit-testable.

const TS_PACKET: usize = 188;
const SYNC: u8 = 0x47;
const STREAM_TYPE_HEVC: u8 = 0x24;
const DOVI_DESCRIPTOR_TAG: u8 = 0xB0;

/// The Dolby Vision video stream descriptor (ETSI TS 103 572 / what ffmpeg's mpegts code reads
/// and writes): the TS twin of an mp4 `dvcC` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoviDescriptor {
    pub profile: u8,
    pub level: u8,
    pub rpu: bool,
    pub el: bool,
    pub bl: bool,
    pub compat_id: u8,
}

impl DoviDescriptor {
    /// Profile 8.1: single-layer, HDR10-compatible base layer, RPU present.
    pub fn profile_81(level: u8) -> Self {
        DoviDescriptor {
            profile: 8,
            level,
            rpu: true,
            el: false,
            bl: true,
            compat_id: 1,
        }
    }

    /// Profile 7 as a BD source describes itself (test fixtures only).
    #[cfg(test)]
    pub fn profile_7(level: u8) -> Self {
        DoviDescriptor {
            profile: 7,
            level,
            rpu: true,
            el: true,
            bl: true,
            compat_id: 6,
        }
    }

    pub fn bytes(&self) -> [u8; 7] {
        let flags: u16 = (u16::from(self.profile & 0x7f) << 9)
            | (u16::from(self.level & 0x3f) << 3)
            | (u16::from(self.rpu) << 2)
            | (u16::from(self.el) << 1)
            | u16::from(self.bl);
        // bl_present is always set here, so no dependency_pid field follows the flags.
        [
            DOVI_DESCRIPTOR_TAG,
            5,
            1, // dv_version_major
            0, // dv_version_minor
            (flags >> 8) as u8,
            flags as u8,
            (self.compat_id & 0x0f) << 4, // compat id, md_compression 0, reserved
        ]
    }
}

/// MPEG-2 CRC32 (poly 0x04C11DB7, init all-ones, no reflection) as PSI sections use.
fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

struct Pes {
    /// The first packet's adaptation field body (length byte excluded, stuffing removed).
    af: Option<Vec<u8>>,
    /// PES header + payload bytes collected so far.
    data: Vec<u8>,
}

/// Counters the caller reads to make its convert / fall-back decision.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TsStats {
    pub video_pes: u64,
    pub rpus: u64,
    pub dropped_el: u64,
}

pub struct TsRewriter<F> {
    transform: F,
    descriptor: DoviDescriptor,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    carry: Vec<u8>,
    pes: Option<Pes>,
    cc: u8,
    pub stats: TsStats,
}

/// A transform returns the new payload plus (rpus, dropped EL NALs) for the stats.
pub type TransformOut = (Vec<u8>, u64, u64);

impl<F> TsRewriter<F>
where
    F: FnMut(&[u8]) -> Result<TransformOut, String>,
{
    pub fn new(descriptor: DoviDescriptor, transform: F) -> Self {
        TsRewriter {
            transform,
            descriptor,
            pmt_pid: None,
            video_pid: None,
            carry: Vec::with_capacity(TS_PACKET),
            pes: None,
            cc: 0,
            stats: TsStats::default(),
        }
    }

    /// Feed bytes; complete output packets are appended to `out`.
    pub fn push(&mut self, mut data: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        if !self.carry.is_empty() {
            let need = TS_PACKET - self.carry.len();
            let take = need.min(data.len());
            self.carry.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.carry.len() < TS_PACKET {
                return Ok(());
            }
            let pkt = std::mem::take(&mut self.carry);
            self.packet(&pkt, out)?;
            self.carry = pkt;
            self.carry.clear();
        }
        let mut chunks = data.chunks_exact(TS_PACKET);
        for pkt in &mut chunks {
            self.packet(pkt, out)?;
        }
        self.carry.extend_from_slice(chunks.remainder());
        Ok(())
    }

    /// End of input: flush the last video PES. Trailing partial packets are an error.
    pub fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        if !self.carry.is_empty() {
            return Err(format!(
                "input ended mid-packet ({} stray bytes)",
                self.carry.len()
            ));
        }
        self.flush_pes(out)
    }

    fn packet(&mut self, pkt: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
        if pkt[0] != SYNC {
            return Err("lost MPEG-TS sync".into());
        }
        let pusi = pkt[1] & 0x40 != 0;
        let pid = (u16::from(pkt[1] & 0x1f) << 8) | u16::from(pkt[2]);
        let afc = (pkt[3] >> 4) & 0x3;
        let (af, payload) = split_packet(pkt, afc)?;

        if pid == 0 && pusi && self.pmt_pid.is_none() {
            self.pmt_pid = parse_pat(payload);
        }
        if Some(pid) == self.pmt_pid && pusi {
            let (rewritten, video) = rewrite_pmt(pkt, payload, &self.descriptor)?;
            if self.video_pid.is_none() {
                self.video_pid = video;
            }
            out.extend_from_slice(&rewritten);
            return Ok(());
        }
        if Some(pid) == self.video_pid {
            if pusi {
                self.flush_pes(out)?;
                self.pes = Some(Pes {
                    af: af.map(strip_af_stuffing),
                    data: payload.to_vec(),
                });
            } else if let Some(p) = self.pes.as_mut() {
                p.data.extend_from_slice(payload);
            } // else: continuation of a PES we never saw start; drop it
            return Ok(());
        }
        out.extend_from_slice(pkt);
        Ok(())
    }

    fn flush_pes(&mut self, out: &mut Vec<u8>) -> Result<(), String> {
        let Some(pes) = self.pes.take() else {
            return Ok(());
        };
        let Some(pid) = self.video_pid else {
            return Ok(());
        };
        let d = &pes.data;
        if d.len() < 9 || d[0..3] != [0, 0, 1] {
            return Err("video PES without a start code".into());
        }
        let header_len = 9 + usize::from(d[8]);
        if d.len() < header_len {
            return Err("video PES header truncated".into());
        }
        let (new_payload, rpus, el) = (self.transform)(&d[header_len..])?;
        self.stats.video_pes += 1;
        self.stats.rpus += rpus;
        self.stats.dropped_el += el;
        let mut pes_bytes = Vec::with_capacity(header_len + new_payload.len());
        pes_bytes.extend_from_slice(&d[..header_len]);
        pes_bytes[4] = 0; // PES_packet_length 0: unbounded, legal for video
        pes_bytes[5] = 0;
        pes_bytes.extend_from_slice(&new_payload);
        self.packetize(pid, pes.af.as_deref(), &pes_bytes, out);
        Ok(())
    }

    fn packetize(&mut self, pid: u16, first_af: Option<&[u8]>, mut data: &[u8], out: &mut Vec<u8>) {
        let mut first = true;
        while !data.is_empty() {
            let af_body: &[u8] = if first { first_af.unwrap_or(&[]) } else { &[] };
            let af_len = if af_body.is_empty() && !(first && first_af.is_some()) {
                0
            } else {
                1 + af_body.len()
            };
            let room = 184 - af_len;
            let take = room.min(data.len());
            // Short last chunk: pad with adaptation-field stuffing.
            let stuffing = room - take;
            let mut pkt = Vec::with_capacity(TS_PACKET);
            pkt.push(SYNC);
            pkt.push(if first { 0x40 } else { 0 } | ((pid >> 8) as u8 & 0x1f));
            pkt.push(pid as u8);
            let has_af = af_len > 0 || stuffing > 0;
            pkt.push(if has_af { 0x30 } else { 0x10 } | (self.cc & 0x0f));
            self.cc = self.cc.wrapping_add(1);
            if has_af {
                let total_af = af_len + stuffing; // bytes incl. the length byte
                pkt.push((total_af - 1) as u8);
                if total_af > 1 {
                    if af_body.is_empty() {
                        pkt.push(0x00); // flags byte: nothing set
                        pkt.extend(std::iter::repeat_n(0xff, total_af - 2));
                    } else {
                        pkt.extend_from_slice(af_body);
                        pkt.extend(std::iter::repeat_n(0xff, stuffing));
                    }
                }
            }
            pkt.extend_from_slice(&data[..take]);
            debug_assert_eq!(pkt.len(), TS_PACKET);
            out.extend_from_slice(&pkt);
            data = &data[take..];
            first = false;
        }
    }
}

/// (adaptation field body without its length byte, payload) of one packet.
fn split_packet(pkt: &[u8], afc: u8) -> Result<(Option<&[u8]>, &[u8]), String> {
    match afc {
        1 => Ok((None, &pkt[4..])),
        2 | 3 => {
            let len = usize::from(pkt[4]);
            if 5 + len > TS_PACKET {
                return Err("adaptation field overruns the packet".into());
            }
            let af = &pkt[5..5 + len];
            let payload = if afc == 3 { &pkt[5 + len..] } else { &[][..] };
            Ok((Some(af), payload))
        }
        _ => Ok((None, &[][..])),
    }
}

/// The used part of an adaptation field body (flags + the optional fields the flags announce),
/// without trailing 0xFF stuffing.
fn strip_af_stuffing(af: &[u8]) -> Vec<u8> {
    if af.is_empty() {
        return Vec::new();
    }
    let flags = af[0];
    let mut used = 1usize;
    if flags & 0x10 != 0 {
        used += 6; // PCR
    }
    if flags & 0x08 != 0 {
        used += 6; // OPCR
    }
    if flags & 0x04 != 0 {
        used += 1; // splice countdown
    }
    if flags & 0x02 != 0 {
        used += 1 + af.get(used).map_or(0, |&l| usize::from(l)); // private data
    }
    if flags & 0x01 != 0 {
        used += 1 + af.get(used).map_or(0, |&l| usize::from(l)); // extension
    }
    af[..used.min(af.len())].to_vec()
}

/// First program's PMT PID from a PAT packet payload (pointer field first).
fn parse_pat(payload: &[u8]) -> Option<u16> {
    let ptr = usize::from(*payload.first()?);
    let sec = payload.get(1 + ptr..)?;
    if *sec.first()? != 0x00 {
        return None;
    }
    let len = (usize::from(sec[1] & 0x0f) << 8) | usize::from(sec[2]);
    let end = (3 + len).checked_sub(4)?;
    let mut i = 8;
    while i + 4 <= end.min(sec.len()) {
        let program = u16::from_be_bytes([sec[i], sec[i + 1]]);
        let pid = (u16::from(sec[i + 2] & 0x1f) << 8) | u16::from(sec[i + 3]);
        if program != 0 {
            return Some(pid);
        }
        i += 4;
    }
    None
}

/// Rewrite a single-packet PMT so the first HEVC stream carries exactly `desc`. Returns the new
/// 188-byte packet and that stream's PID.
fn rewrite_pmt(
    pkt: &[u8],
    payload: &[u8],
    desc: &DoviDescriptor,
) -> Result<(Vec<u8>, Option<u16>), String> {
    let ptr = usize::from(*payload.first().ok_or("empty PMT payload")?);
    let sec = payload.get(1 + ptr..).ok_or("PMT pointer overruns")?;
    if sec.len() < 12 || sec[0] != 0x02 {
        return Err("not a PMT section".into());
    }
    let len = (usize::from(sec[1] & 0x0f) << 8) | usize::from(sec[2]);
    if 3 + len > sec.len() {
        return Err("PMT section spans packets (unsupported)".into());
    }
    let sec = &sec[..3 + len];
    let pil = (usize::from(sec[10] & 0x0f) << 8) | usize::from(sec[11]);
    let mut pos = 12 + pil;
    let es_end = sec.len() - 4;
    if pos > es_end {
        return Err("PMT program info overruns".into());
    }
    let mut body = sec[..pos].to_vec();
    let mut video = None;
    while pos + 5 <= es_end {
        let stype = sec[pos];
        let epid = (u16::from(sec[pos + 1] & 0x1f) << 8) | u16::from(sec[pos + 2]);
        let eil = (usize::from(sec[pos + 3] & 0x0f) << 8) | usize::from(sec[pos + 4]);
        let info = sec
            .get(pos + 5..pos + 5 + eil)
            .ok_or("PMT ES info overruns")?;
        let mut new_info = Vec::with_capacity(info.len() + 7);
        if stype == STREAM_TYPE_HEVC && video.is_none() {
            video = Some(epid);
            let mut i = 0;
            while i + 2 <= info.len() {
                let dl = usize::from(info[i + 1]);
                let d = info.get(i..i + 2 + dl).ok_or("PMT descriptor overruns")?;
                if d[0] != DOVI_DESCRIPTOR_TAG {
                    new_info.extend_from_slice(d);
                }
                i += 2 + dl;
            }
            new_info.extend_from_slice(&desc.bytes());
        } else {
            new_info.extend_from_slice(info);
        }
        body.extend_from_slice(&sec[pos..pos + 3]);
        body.push(0xf0 | ((new_info.len() >> 8) as u8 & 0x0f));
        body.push(new_info.len() as u8);
        body.extend_from_slice(&new_info);
        pos += 5 + eil;
    }
    let new_len = body.len() - 3 + 4;
    body[1] = (body[1] & 0xf0) | ((new_len >> 8) as u8 & 0x0f);
    body[2] = new_len as u8;
    let crc = crc32_mpeg2(&body);
    body.extend_from_slice(&crc.to_be_bytes());

    // Header without any adaptation field, PUSI kept, pointer 0.
    let mut out = Vec::with_capacity(TS_PACKET);
    out.extend_from_slice(&[pkt[0], pkt[1], pkt[2], 0x10 | (pkt[3] & 0x0f), 0]);
    out.extend_from_slice(&body);
    if out.len() > TS_PACKET {
        return Err("rewritten PMT no longer fits one packet".into());
    }
    out.resize(TS_PACKET, 0xff);
    Ok((out, video))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal TS: PAT (PMT on 0x1000), PMT (HEVC on 0x100 with an existing profile-7 0xB0 and
    /// an unrelated descriptor), then video PESes split over several packets.
    fn pat() -> Vec<u8> {
        let mut sec = vec![
            0x00, 0xb0, 0, 0x00, 0x01, 0xc1, 0, 0, 0x00, 0x01, 0xf0, 0x00,
        ];
        let len = sec.len() - 3 + 4;
        sec[2] = len as u8;
        let crc = crc32_mpeg2(&sec);
        sec.extend_from_slice(&crc.to_be_bytes());
        let mut p = vec![SYNC, 0x40, 0x00, 0x10, 0x00];
        p.extend(sec);
        p.resize(TS_PACKET, 0xff);
        p
    }

    fn pmt(extra_desc: &[u8]) -> Vec<u8> {
        let mut info = vec![0x05, 4, b'H', b'E', b'V', b'C'];
        info.extend_from_slice(extra_desc);
        let mut sec = vec![
            0x02, 0xb0, 0, 0x00, 0x01, 0xc1, 0, 0, 0xe1, 0x00, 0xf0, 0x00,
        ];
        sec.extend_from_slice(&[STREAM_TYPE_HEVC, 0xe1, 0x00, 0xf0, info.len() as u8]);
        sec.extend_from_slice(&info);
        sec.extend_from_slice(&[0x0f, 0xe1, 0x01, 0xf0, 0x00]); // an AAC stream
        let len = sec.len() - 3 + 4;
        sec[2] = len as u8;
        let crc = crc32_mpeg2(&sec);
        sec.extend_from_slice(&crc.to_be_bytes());
        let mut p = vec![SYNC, 0x50, 0x00, 0x10, 0x00];
        p.extend(sec);
        p.resize(TS_PACKET, 0xff);
        p
    }

    fn pes(payload: &[u8], pts: u8) -> Vec<u8> {
        let mut v = vec![0, 0, 1, 0xe0, 0, 0, 0x80, 0x80, 5, 0x21, 0, pts, 0, 1];
        v.extend_from_slice(payload);
        v
    }

    /// Packetize a PES on PID 0x100 with a PCR adaptation field in the first packet.
    fn packets(pes: &[u8], cc: &mut u8) -> Vec<u8> {
        let mut out = Vec::new();
        let mut data = pes;
        let mut first = true;
        while !data.is_empty() {
            let mut p = vec![SYNC, if first { 0x41 } else { 0x01 }, 0x00];
            let af: Vec<u8> = if first {
                vec![0x50, 1, 2, 3, 4, 5, 6] // RAI + PCR flag, 6 PCR bytes
            } else {
                vec![]
            };
            let room = 184 - if af.is_empty() { 0 } else { 1 + af.len() };
            let take = room.min(data.len());
            let stuffing = room - take;
            if af.is_empty() && stuffing == 0 {
                p.push(0x10 | *cc);
            } else {
                p.push(0x30 | *cc);
                if af.is_empty() {
                    p.push((stuffing - 1) as u8);
                    if stuffing >= 2 {
                        p.push(0);
                        p.extend(std::iter::repeat_n(0xff, stuffing - 2));
                    }
                } else {
                    p.push((af.len() + stuffing) as u8);
                    p.extend_from_slice(&af);
                    p.extend(std::iter::repeat_n(0xff, stuffing));
                }
            }
            p.extend_from_slice(&data[..take]);
            assert_eq!(p.len(), TS_PACKET);
            out.extend(p);
            *cc = (*cc + 1) & 0x0f;
            data = &data[take..];
            first = false;
        }
        out
    }

    /// Demux helper for assertions: (PMT ES info of the HEVC stream, reassembled video PESes).
    fn demux(ts: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>, Vec<u8>) {
        let mut pmt_info = Vec::new();
        let mut pes: Vec<Vec<u8>> = Vec::new();
        let mut ccs = Vec::new();
        for p in ts.chunks_exact(TS_PACKET) {
            assert_eq!(p[0], SYNC);
            let pid = (u16::from(p[1] & 0x1f) << 8) | u16::from(p[2]);
            let (_, payload) = split_packet(p, (p[3] >> 4) & 3).unwrap();
            if pid == 0x1000 {
                let sec = &payload[1..];
                let len = (usize::from(sec[1] & 0x0f) << 8) | usize::from(sec[2]);
                assert_eq!(
                    crc32_mpeg2(&sec[..3 + len - 4]).to_be_bytes(),
                    sec[3 + len - 4..3 + len],
                    "PMT CRC must be recomputed"
                );
                let eil = usize::from(sec[16]);
                pmt_info = sec[17..17 + eil].to_vec();
            } else if pid == 0x100 {
                ccs.push(p[3] & 0x0f);
                if p[1] & 0x40 != 0 {
                    pes.push(Vec::new());
                }
                pes.last_mut().unwrap().extend_from_slice(payload);
            }
        }
        (pmt_info, pes, ccs)
    }

    fn upper(payload: &[u8]) -> Result<TransformOut, String> {
        Ok((payload.to_ascii_uppercase(), 1, 0))
    }

    #[test]
    fn pmt_gets_exactly_one_profile81_descriptor_and_a_valid_crc() {
        let old = DoviDescriptor::profile_7(6).bytes();
        let mut input = pat();
        input.extend(pmt(&old));
        let mut rw = TsRewriter::new(DoviDescriptor::profile_81(6), upper);
        let mut out = Vec::new();
        rw.push(&input, &mut out).unwrap();
        rw.finish(&mut out).unwrap();
        let (info, _, _) = demux(&out);
        let mut want = vec![0x05, 4, b'H', b'E', b'V', b'C'];
        want.extend_from_slice(&DoviDescriptor::profile_81(6).bytes());
        assert_eq!(
            info, want,
            "old 0xB0 removed, other descriptors kept, ours added"
        );
    }

    #[test]
    fn descriptor_bytes_match_the_measured_ffprobe_decoding() {
        // Same bytes the 2026-09-27 experiment injected; ffprobe decoded them as
        // "version: 1.0, profile: 8, level: 6, rpu flag: 1, el flag: 0, bl flag: 1,
        // compatibility id: 1, compression: 0".
        assert_eq!(
            DoviDescriptor::profile_81(6).bytes(),
            [0xb0, 5, 1, 0, 0x10, 0x35, 0x10]
        );
    }

    #[test]
    fn video_pes_payloads_are_transformed_and_headers_and_pcr_kept() {
        let mut cc = 0;
        let mut input = pat();
        input.extend(pmt(&[]));
        let big: Vec<u8> = (0..500u32).map(|i| b'a' + (i % 26) as u8).collect();
        let small = b"tail frame".to_vec();
        input.extend(packets(&pes(&big, 1), &mut cc));
        input.extend(packets(&pes(&small, 2), &mut cc));

        // Feed in awkward chunk sizes to exercise the carry path.
        let mut rw = TsRewriter::new(DoviDescriptor::profile_81(6), upper);
        let mut out = Vec::new();
        for chunk in input.chunks(97) {
            rw.push(chunk, &mut out).unwrap();
        }
        rw.finish(&mut out).unwrap();
        assert_eq!(out.len() % TS_PACKET, 0);
        assert_eq!(rw.stats.video_pes, 2);
        assert_eq!(rw.stats.rpus, 2);

        let (_, pes_out, ccs) = demux(&out);
        assert_eq!(pes_out.len(), 2);
        let mut want = pes(&big.to_ascii_uppercase(), 1);
        want[4] = 0;
        want[5] = 0;
        assert_eq!(pes_out[0], want);
        assert!(pes_out[1].ends_with(b"TAIL FRAME"));
        for w in ccs.windows(2) {
            assert_eq!(
                w[1],
                (w[0] + 1) & 0x0f,
                "continuity counter must be contiguous"
            );
        }
        // PCR adaptation field survives on each PES's first packet.
        let first_video = out
            .chunks_exact(TS_PACKET)
            .find(|p| p[1] & 0x1f == 0x01 && p[1] & 0x40 != 0)
            .unwrap();
        assert_eq!(&first_video[4..12], &[7, 0x50, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn transform_errors_propagate() {
        let mut cc = 0;
        let mut input = pat();
        input.extend(pmt(&[]));
        input.extend(packets(&pes(b"x", 1), &mut cc));
        let mut rw = TsRewriter::new(DoviDescriptor::profile_81(6), |_: &[u8]| {
            Err::<TransformOut, _>("bad rpu".to_string())
        });
        let mut out = Vec::new();
        rw.push(&input, &mut out).unwrap();
        assert_eq!(rw.finish(&mut out), Err("bad rpu".to_string()));
    }

    #[test]
    fn lost_sync_and_stray_bytes_are_errors() {
        let mut rw = TsRewriter::new(DoviDescriptor::profile_81(6), upper);
        let mut out = Vec::new();
        assert!(rw.push(&[0u8; TS_PACKET], &mut out).is_err());
        let mut rw = TsRewriter::new(DoviDescriptor::profile_81(6), upper);
        rw.push(&pat()[..10], &mut out).unwrap();
        assert!(rw.finish(&mut out).is_err());
    }
}
