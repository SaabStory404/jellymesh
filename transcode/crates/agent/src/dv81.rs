//! On-the-fly Dolby Vision profile 7 -> 8.1 conversion core (`docs/engineering/transcode-plan.md` P5).
//!
//! The per-access-unit NAL rewrite: Annex-B NAL splitting, enhancement-layer removal, and RPU
//! profile rewriting via the `dolby_vision` crate (the crate dovi_tool itself is built on). The
//! transport around it (an MPEG-TS stream from ffmpeg#1, rewritten PES by PES) is `dv81_ts.rs`;
//! the argv for the two ffmpegs is `dv81_plan.rs`; process wiring and fallback live in `job.rs`.
//!
//! **Which sources this can convert.** Only DV7 files whose per-frame RPU is carried *in-band*
//! as HEVC NAL type 62 in the video track. MEASURED 2026-09-27 in `tc-lab` (jellyfin-ffmpeg
//! 8.1.2): some profile-7 MKVs instead carry RPU + enhancement layer in a Matroska Block Addition
//! (mapping type `hvcE`; ffmpeg logs `unknown Block Addition Mapping type 68766345`), and an
//! ffmpeg stream copy of those contains zero type-62 NALs. `job.rs` detects that on the first
//! bytes of ffmpeg#1's output (no RPU seen) and falls back to the plain remux; reading `hvcE`
//! Block Additions is future work (docs/engineering/transcode-plan.md P5).
//!
//! **Enhancement-layer shape.** In a single-track (BD-style) DV7 bitstream the EL NALs are
//! encapsulated as `nal_unit_type` 63 (UNSPEC63) on layer 0 -- dovi_tool's `demux` treats 63 as
//! EL (INHERITED from dovi_tool's behaviour, not yet traced on a real prod file). A multi-layer
//! muxing would instead put EL NALs on `nuh_layer_id != 0`. Both are dropped; the RPU (type 62)
//! is kept and rewritten whatever its layer id.
//!
//! Deciding whether to convert at all is `job.rs`'s call, gated on BOTH `tcpool_ir::wants_dv81`
//! (Jellyfin asked) AND `probe::source_dovi` reporting profile 7 (the source really is DV7), so a
//! signal on a DV5/DV8/non-DV source is a safe no-op, never a corruption.

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use dolby_vision::rpu::ConversionMode;

/// HEVC's RPU NAL unit type (`nal_unit_type` 62, "UNSPEC62" in the spec): where Dolby Vision
/// profile 7/8 metadata rides inside an HEVC bitstream.
pub const RPU_NAL_UNIT_TYPE: u8 = 62;

/// HEVC NAL type 63 (UNSPEC63): how a single-track DV profile-7 bitstream encapsulates its
/// enhancement-layer NALs. Dropped on conversion.
pub const EL_NAL_UNIT_TYPE: u8 = 63;

/// One Annex-B NAL unit: its HEVC NAL unit type and layer id (from the 2-byte NAL header), and
/// its bytes (header + payload, no start code, emulation-prevention bytes untouched).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nal {
    pub nal_unit_type: u8,
    pub layer_id: u8,
    pub data: Vec<u8>,
}

fn nal_unit_type(header0: u8) -> u8 {
    (header0 >> 1) & 0x3f
}

fn nal_layer_id(header0: u8, header1: u8) -> u8 {
    ((header0 & 0x01) << 5) | (header1 >> 3)
}

/// Split an Annex-B byte stream (`00 00 01` start codes, 3- or 4-byte) into its NAL units.
/// Malformed input (a NAL shorter than its 2-byte header) drops that NAL rather than panicking.
pub fn split_annexb(data: &[u8]) -> Vec<Nal> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (n, &start) in starts.iter().enumerate() {
        let next_start_code = starts.get(n + 1).map(|&s| s - 3).unwrap_or(data.len());
        // Trailing zero bytes (a 4-byte start code's leading zero, or trailing_zero_8bits)
        // belong to the next start code, not this NAL's payload.
        let mut end = next_start_code;
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        if end <= start || end - start < 2 {
            continue; // zero-length (trailing padding) or too short to have a NAL header
        }
        let raw = &data[start..end];
        nals.push(Nal {
            nal_unit_type: nal_unit_type(raw[0]),
            layer_id: nal_layer_id(raw[0], raw[1]),
            data: raw.to_vec(),
        });
    }
    nals
}

/// Re-serialize NALs back to Annex-B with 4-byte start codes (matches what `dolby_vision`'s own
/// `write_hevc_unspec62_nalu` emits, so the RPU NAL and everything else use the same width).
pub fn join_annexb(nals: &[Nal]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nals.iter().map(|n| n.data.len() + 4).sum());
    for n in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&n.data);
    }
    out
}

/// Rewrite one RPU NAL's payload to profile 8.1 (`ConversionMode::To81`: dovi_tool's own default
/// DV7->8.1 conversion -- MEL sources keep their luma/chroma mapping, FEL sources have it removed,
/// i.e. "MEL loses nothing visible; FEL loses the enhancement detail").
fn rewrite_rpu_nal(nal: &Nal) -> Result<Nal, String> {
    debug_assert_eq!(nal.nal_unit_type, RPU_NAL_UNIT_TYPE);
    // `dolby_vision`'s NAL parser expects the payload without the 2-byte NAL header (it re-adds
    // its own unspec62 header on write).
    let payload = nal.data.get(2..).ok_or("RPU NAL shorter than its header")?;
    let mut rpu = DoviRpu::parse_unspec62_nalu(payload).map_err(|e| e.to_string())?;
    rpu.convert_with_mode(ConversionMode::To81)
        .map_err(|e| e.to_string())?;
    let rewritten = rpu.write_hevc_unspec62_nalu().map_err(|e| e.to_string())?;
    Ok(Nal {
        nal_unit_type: RPU_NAL_UNIT_TYPE,
        layer_id: 0, // the RPU now describes the base layer only; the EL it referenced is gone
        data: rewritten,
    })
}

/// What one access unit's conversion did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuStats {
    /// RPU NALs found and rewritten to profile 8.1.
    pub rpus: u64,
    /// Enhancement-layer NALs dropped (type 63, or `nuh_layer_id != 0`).
    pub dropped_el: u64,
}

/// Convert one Annex-B chunk (normally one access unit, i.e. one PES payload) from DV profile 7
/// to 8.1: keep every base-layer NAL byte-for-byte, rewrite each RPU NAL to profile 8.1, drop
/// enhancement-layer NALs (see the module docs for the two EL shapes). A chunk with no RPU is not
/// an error here (the caller decides what "no RPU" means for a whole stream); an RPU that fails to
/// parse or convert is, and fails the whole chunk -- never a partial conversion.
pub fn convert_access_unit(data: &[u8]) -> Result<(Vec<u8>, AuStats), String> {
    let nals = split_annexb(data);
    let mut stats = AuStats::default();
    let mut out = Vec::with_capacity(nals.len());
    for nal in &nals {
        if nal.nal_unit_type == RPU_NAL_UNIT_TYPE {
            stats.rpus += 1;
            out.push(rewrite_rpu_nal(nal)?);
        } else if nal.nal_unit_type == EL_NAL_UNIT_TYPE || nal.layer_id != 0 {
            stats.dropped_el += 1;
        } else {
            out.push(nal.clone());
        }
    }
    Ok((join_annexb(&out), stats))
}

/// Convert a whole Annex-B HEVC elementary stream from DV profile 7 to profile 8.1 (see
/// `convert_access_unit`). `Err` if the stream has no RPU NAL at all (not DV profile 7/8, or the
/// RPU is out-of-band -- the module docs' Block Addition finding) or any RPU fails to convert.
/// The live path converts PES by PES (`dv81_ts`); this whole-stream form is for tests.
#[cfg(test)]
pub fn convert_annexb_to_dv81(data: &[u8]) -> Result<Vec<u8>, String> {
    let (out, stats) = convert_access_unit(data)?;
    if stats.rpus == 0 {
        return Err(
            "no RPU NAL found: not a Dolby Vision profile 7/8 bitstream, or the RPU is \
             out-of-band (see the module docs' Block Addition finding)"
                .into(),
        );
    }
    Ok(out)
}

/// Test-only helpers shared with `dv81_ts`'s and `job`'s tests: a synthetic, parseable
/// profile-7 (MEL) RPU NAL and NAL builders. Nothing here is derived from a real source.
#[cfg(test)]
pub mod testutil {
    use dolby_vision::rpu::dovi_rpu::DoviRpu;
    use dolby_vision::rpu::generate::{GenerateConfig, GenerateProfile, VideoShot};
    use dolby_vision::rpu::ConversionMode;

    pub fn nal_bytes(nal_unit_type: u8, layer_id: u8, payload: &[u8]) -> Vec<u8> {
        // 2-byte HEVC NAL header: forbidden_zero_bit(1)=0, nal_unit_type(6), layer_id(6),
        // temporal_id_plus1(3)=1.
        let b0 = (nal_unit_type << 1) | (layer_id >> 5);
        let b1 = ((layer_id & 0x1f) << 3) | 1;
        let mut v = vec![b0, b1];
        v.extend_from_slice(payload);
        v
    }

    pub fn annexb(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(n);
        }
        out
    }

    /// A synthetic profile-7 MEL RPU NAL (2-byte header included). The `dolby_vision` crate only
    /// *generates* 5/8.1/8.4, so this generates an 8.1 RPU and runs the crate's own
    /// `ConversionMode::ToMel` on it, which sets exactly the header bits that make
    /// `get_dovi_profile()` report 7 (EL resampling on, residual enabled, 12-bit VDR) and attaches
    /// a MEL (zero-residual) NLQ block. Asserted, not assumed.
    pub fn synthetic_p7_rpu_nal() -> Vec<u8> {
        let cfg = GenerateConfig {
            profile: GenerateProfile::Profile81,
            length: 1,
            shots: vec![VideoShot {
                start: 0,
                duration: 1,
                ..VideoShot::default()
            }],
            ..GenerateConfig::default()
        };
        let mut rpu = cfg.generate_rpu_list().expect("generate a synthetic RPU")[0].clone();
        rpu.convert_with_mode(ConversionMode::ToMel)
            .expect("8.1 -> MEL");
        let nal = rpu.write_hevc_unspec62_nalu().expect("serialize");
        let reparsed = DoviRpu::parse_unspec62_nalu(&nal[2..]).expect("reparse");
        assert_eq!(
            reparsed.dovi_profile, 7,
            "the synthetic RPU must be profile 7"
        );
        nal
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn splits_and_rejoins_annexb_round_trip() {
        let vps = nal_bytes(32, 0, &[1, 2, 3]);
        let sps = nal_bytes(33, 0, &[4, 5]);
        let vcl = nal_bytes(19, 0, &[6, 7, 8, 9]);
        let data = annexb(&[vps.clone(), sps.clone(), vcl.clone()]);
        let nals = split_annexb(&data);
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0].data, vps);
        assert_eq!(nals[0].nal_unit_type, 32);
        assert_eq!(nals[1].data, sps);
        assert_eq!(nals[2].data, vcl);
        assert_eq!(join_annexb(&nals), data);
    }

    #[test]
    fn three_byte_start_codes_and_trailing_zeros_split_cleanly() {
        let mut data = vec![0, 0, 1];
        data.extend(nal_bytes(1, 0, b"ab"));
        data.extend([0, 0, 0, 0, 1]); // trailing_zero_8bits then a 3-byte start code
        data.extend(nal_bytes(1, 0, b"cd"));
        let nals = split_annexb(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].data, nal_bytes(1, 0, b"ab"));
        assert_eq!(nals[1].data, nal_bytes(1, 0, b"cd"));
    }

    #[test]
    fn layer_id_decodes_from_the_nal_header() {
        let bl = nal_bytes(1, 0, &[]);
        let el = nal_bytes(1, 1, &[]);
        assert_eq!(nal_layer_id(bl[0], bl[1]), 0);
        assert_eq!(nal_layer_id(el[0], el[1]), 1);
    }

    #[test]
    fn convert_turns_a_profile7_rpu_into_81_and_drops_both_el_shapes() {
        let vps = nal_bytes(32, 0, b"vps");
        let bl_vcl = nal_bytes(1, 0, b"bl-frame");
        let el_layer1 = nal_bytes(1, 1, b"el-layer1"); // multi-layer EL shape
        let el_unspec63 = nal_bytes(EL_NAL_UNIT_TYPE, 0, b"el-unspec63"); // BD single-track EL
        let rpu = synthetic_p7_rpu_nal();
        let data = annexb(&[vps.clone(), bl_vcl.clone(), el_layer1, el_unspec63, rpu]);

        let (out, stats) = convert_access_unit(&data).expect("conversion should succeed");
        assert_eq!(
            stats,
            AuStats {
                rpus: 1,
                dropped_el: 2
            }
        );
        let out_nals = split_annexb(&out);
        assert_eq!(out_nals.len(), 3, "vps + bl frame + rpu: {out_nals:?}");
        assert_eq!(out_nals[0].data, vps);
        assert_eq!(out_nals[1].data, bl_vcl);
        let converted = DoviRpu::parse_unspec62_nalu(&out_nals[2].data[2..])
            .expect("the rewritten RPU must still parse");
        assert_eq!(converted.dovi_profile, 8);
        assert!(converted.el_type.is_none(), "no EL type after To81");
    }

    #[test]
    fn a_chunk_without_rpu_is_not_an_error_per_au_but_is_for_a_whole_stream() {
        let data = annexb(&[nal_bytes(32, 0, b"vps"), nal_bytes(1, 0, b"frame")]);
        let (out, stats) = convert_access_unit(&data).unwrap();
        assert_eq!(stats.rpus, 0);
        assert_eq!(out, data);
        assert!(convert_annexb_to_dv81(&data).is_err());
    }

    #[test]
    fn convert_is_never_a_partial_result_on_a_malformed_rpu() {
        let bad_rpu = nal_bytes(RPU_NAL_UNIT_TYPE, 0, b"not a real rpu payload");
        let data = annexb(&[nal_bytes(32, 0, b"vps"), bad_rpu]);
        assert!(convert_access_unit(&data).is_err());
        assert!(convert_annexb_to_dv81(&data).is_err());
    }
}
