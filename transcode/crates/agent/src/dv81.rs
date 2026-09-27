//! On-the-fly Dolby Vision profile 7 -> 8.1 conversion core (`transcode/docs/PLAN.md` P5).
//!
//! **Not wired into `job.rs`'s exec path yet.** This module is the real, unit-tested RPU-rewrite
//! core -- Annex-B NAL splitting, enhancement-layer NAL removal, and RPU profile rewriting via the
//! `dolby_vision` crate (the crate dovi_tool itself is built on, per PLAN.md P5's own wording).
//! Wiring it into a live two-process pipeline is blocked on a real, MEASURED finding, not a TODO:
//!
//! On the one real DV profile-7 source available in `tc-lab` (a Blu-ray-remux MKV, profile 7,
//! `dv_bl_signal_compatibility_id` 6 -- the common "\[DV HDR10\]" release-group muxing), jellyfin-
//! ffmpeg 8.1.2's matroska demuxer logs `Invalid Block Addition value 0x0 for unknown Block
//! Addition Mapping type 68766345` for the video track, and a `-map 0:v:0 -c:v copy` stream copy
//! traced NAL-by-NAL (`-bsf:v trace_headers`, 301 NALs over 3 real seconds) contained **zero**
//! `nal_unit_type == 62` (RPU) NALs and **zero** `nuh_layer_id != 0` NALs. `ffprobe` still reports
//! `rpu_present_flag: 1, el_present_flag: 1` for the track (from the CodecPrivate DOVI
//! configuration record, a static per-track descriptor), but the per-frame RPU/EL bitstream itself
//! is carried in a Matroska Block Addition this ffmpeg build does not parse -- so a plain
//! `-map`/stream-copy/bsf pipeline never sees it, for this muxing.
//!
//! That rules out a pure-ffmpeg two-process pipeline (extract Annex-B, rewrite, remux) for this
//! common case. It does NOT rule out this module: `convert_annexb_to_dv81` below is correct and
//! useful for **any** Annex-B HEVC bitstream that already carries its RPU in-band as NAL type 62
//! (true of some encoders/muxings, and of whatever extraction path eventually gets the bytes out
//! of a Block-Addition-muxed MKV -- a Matroska Block-Addition-aware read, not an ffmpeg `-map`,
//! is what's needed there; see the PLAN.md P5 entry for candidates). Do not wire this into
//! `job.rs` until that extraction step exists and has been proven against a real profile-7 source
//! in `tc-lab`.
//!
//! Also NOT this module's job: deciding whether to convert at all. Once wired, that's `job.rs`'s
//! call, gated on BOTH `tcpool_ir::wants_dv81` (Jellyfin asked) AND `probe::source_dovi_profile`
//! returning `Some(7)` (the source really is DV profile 7) -- so a signal on a DV5/DV8/non-DV
//! source is a safe no-op, never a corruption.

// Not called from `job.rs` yet (see the module docs above for why); `cargo clippy -D
// warnings` would otherwise fail the build on an unused public API in a binary crate.
// Remove this once P5 is wired.
#![allow(dead_code)]

use dolby_vision::rpu::dovi_rpu::DoviRpu;
use dolby_vision::rpu::ConversionMode;

/// HEVC's RPU NAL unit type (`nal_unit_type` 62, "UNSPEC62" in the spec): where Dolby Vision
/// profile 7/8 metadata rides inside an HEVC bitstream.
const RPU_NAL_UNIT_TYPE: u8 = 62;

/// One Annex-B NAL unit: its HEVC NAL unit type and layer id (from the 2-byte NAL header), and
/// its bytes (header + payload, no start code, emulation-prevention bytes untouched).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Nal {
    nal_unit_type: u8,
    layer_id: u8,
    data: Vec<u8>,
}

fn nal_unit_type(header0: u8) -> u8 {
    (header0 >> 1) & 0x3f
}

fn nal_layer_id(header0: u8, header1: u8) -> u8 {
    ((header0 & 0x01) << 5) | (header1 >> 3)
}

/// Split an Annex-B byte stream (`00 00 01` start codes, 3- or 4-byte) into its NAL units.
/// Malformed input (a NAL shorter than its 2-byte header) drops that NAL rather than panicking;
/// callers that need "did every byte round-trip" should compare `join_annexb`'s output length
/// separately, not rely on this never dropping anything.
fn split_annexb(data: &[u8]) -> Vec<Nal> {
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
        // A 4-byte start code's leading zero belongs to the start code, not this NAL's payload.
        let end = if next_start_code > start && data[next_start_code - 1] == 0 {
            next_start_code - 1
        } else {
            next_start_code
        };
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
fn join_annexb(nals: &[Nal]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nals.iter().map(|n| n.data.len() + 4).sum());
    for n in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&n.data);
    }
    out
}

/// Rewrite one RPU NAL's payload from profile 5/7/8 to profile 8.1 (`ConversionMode::To81`:
/// dovi_tool's own default DV7->8.1 conversion -- MEL sources keep their luma/chroma mapping, FEL
/// sources have it removed, matching PLAN.md P5's "MEL loses nothing visible; FEL loses the
/// enhancement detail").
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

/// Convert one Annex-B HEVC elementary stream from DV profile 7 to profile 8.1: keep every
/// base-layer NAL (`nuh_layer_id == 0`) byte-for-byte except the RPU, which is rewritten in place;
/// drop every true enhancement-layer NAL (`nuh_layer_id != 0` and not an RPU -- this *is*
/// "dropping the enhancement layer", PLAN.md P5's own phrase for it). The RPU NAL itself is kept
/// and rewritten regardless of which layer id it arrived on: some encoders carry it on the
/// enhancement layer's NAL stream, not the base layer's (see the module docs -- this has not been
/// confirmed against a real in-band sample this session; treat the layer-id-of-the-RPU assumption
/// as INHERITED, not measured, until it has been).
///
/// Returns `Err` if the stream has no RPU NAL at all (not really DV profile 7/8, or the caller
/// mis-gated -- `job.rs` must never call this without first confirming
/// `probe::source_dovi_profile == Some(7)`) or any RPU NAL fails to convert. Never a partial or
/// best-effort conversion: a stream with some frames at profile 8.1 and others still at profile 7
/// is worse than failing the job outright, and the shim's contract for an agent failure is already
/// "fall back and re-run" (see `shim::run_batch`'s `batch_pool_failure`), so failing closed here
/// costs nothing extra.
pub fn convert_annexb_to_dv81(data: &[u8]) -> Result<Vec<u8>, String> {
    let nals = split_annexb(data);
    let mut rpu_count = 0usize;
    let mut out = Vec::with_capacity(nals.len());
    for nal in &nals {
        if nal.nal_unit_type == RPU_NAL_UNIT_TYPE {
            rpu_count += 1;
            out.push(rewrite_rpu_nal(nal)?);
        } else if nal.layer_id == 0 {
            out.push(nal.clone());
        } // else: enhancement-layer NAL, dropped
    }
    if rpu_count == 0 {
        return Err(
            "no RPU NAL found: not a Dolby Vision profile 7/8 bitstream, or the RPU is \
                     out-of-band (see the module docs' Block Addition finding)"
                .into(),
        );
    }
    Ok(join_annexb(&out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dolby_vision::rpu::generate::{GenerateConfig, GenerateProfile, VideoShot};

    fn nal_bytes(nal_unit_type: u8, layer_id: u8, payload: &[u8]) -> Vec<u8> {
        // 2-byte HEVC NAL header: forbidden_zero_bit(1)=0, nal_unit_type(6), layer_id(6),
        // temporal_id_plus1(3)=1. Only the bits this module reads are meaningful here (type,
        // layer_id); the rest is fixed to valid-looking values, not derived from any real stream.
        let b0 = (nal_unit_type << 1) | (layer_id >> 5);
        let b1 = ((layer_id & 0x1f) << 3) | 1;
        let mut v = vec![b0, b1];
        v.extend_from_slice(payload);
        v
    }

    fn annexb(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(n);
        }
        out
    }

    /// A synthetic (not derived from any real source), valid profile-5 RPU NAL payload -- the
    /// `dolby_vision` crate's `generate` module only synthesizes profile 5/8.1/8.4, not 7 (profile
    /// 7 is decode-only in real content), but `convert_with_mode(ConversionMode::To81)` runs the
    /// same call path for profile 5 (`p5_to_p81`) as for profile 7/8, so this genuinely exercises
    /// this module's NAL-splitting/rewriting/reassembly wiring end-to-end against the real crate.
    fn synthetic_profile5_rpu_nal() -> Vec<u8> {
        let cfg = GenerateConfig {
            profile: GenerateProfile::Profile5,
            length: 1,
            shots: vec![VideoShot {
                start: 0,
                duration: 1,
                ..VideoShot::default()
            }],
            ..GenerateConfig::default()
        };
        let rpus = cfg.generate_rpu_list().expect("generate a synthetic RPU");
        // already includes the 2-byte unspec62 NAL header
        rpus[0]
            .write_hevc_unspec62_nalu()
            .expect("serialize the synthetic RPU to a NAL")
    }

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
    fn layer_id_decodes_from_the_nal_header() {
        let bl = nal_bytes(1, 0, &[]);
        let el = nal_bytes(1, 1, &[]);
        assert_eq!(nal_layer_id(bl[0], bl[1]), 0);
        assert_eq!(nal_layer_id(el[0], el[1]), 1);
    }

    #[test]
    fn convert_drops_enhancement_layer_nals_and_keeps_base_layer_ones() {
        let vps = nal_bytes(32, 0, b"vps");
        let bl_vcl = nal_bytes(1, 0, b"bl-frame");
        let el_vcl = nal_bytes(1, 1, b"el-frame"); // must be dropped
        let rpu = synthetic_profile5_rpu_nal();
        let data = annexb(&[vps.clone(), bl_vcl.clone(), el_vcl, rpu]);

        let out = convert_annexb_to_dv81(&data).expect("conversion should succeed");
        let out_nals = split_annexb(&out);

        assert!(
            out_nals.iter().all(|n| n.layer_id == 0),
            "no enhancement-layer NAL should survive: {out_nals:?}"
        );
        assert!(
            out_nals.iter().any(|n| n.data == vps),
            "unrelated base-layer NALs must pass through byte-for-byte"
        );
        assert!(
            out_nals.iter().any(|n| n.data == bl_vcl),
            "base-layer VCL NALs must pass through byte-for-byte"
        );
        assert!(
            !out_nals.iter().any(|n| n.data.ends_with(b"el-frame")),
            "the enhancement-layer VCL NAL must not survive in any form"
        );
        let rpu_out = out_nals
            .iter()
            .find(|n| n.nal_unit_type == RPU_NAL_UNIT_TYPE)
            .expect("an RPU NAL must survive");
        let converted = DoviRpu::parse_unspec62_nalu(&rpu_out.data[2..])
            .expect("the rewritten RPU must still parse");
        assert_eq!(
            converted.dovi_profile, 8,
            "the RPU must report profile 8(.1) after conversion, not 5"
        );
    }

    #[test]
    fn convert_fails_closed_when_there_is_no_rpu() {
        let data = annexb(&[nal_bytes(32, 0, b"vps"), nal_bytes(1, 0, b"frame")]);
        assert!(
            convert_annexb_to_dv81(&data).is_err(),
            "a stream with no RPU NAL must be rejected, not silently passed through as DV8.1"
        );
    }

    #[test]
    fn convert_is_never_a_partial_result_on_a_malformed_rpu() {
        // A type-62 NAL whose payload doesn't parse as a real RPU must fail the whole
        // conversion, not silently drop just that one NAL and succeed with a broken stream.
        let bad_rpu = nal_bytes(RPU_NAL_UNIT_TYPE, 0, b"not a real rpu payload");
        let data = annexb(&[nal_bytes(32, 0, b"vps"), bad_rpu]);
        assert!(convert_annexb_to_dv81(&data).is_err());
    }
}
