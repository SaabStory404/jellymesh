//! Command allowlist: the agent's security boundary.
//!
//! A worker runs ffmpeg with arguments that arrive over the network, and ffmpeg can read and
//! write arbitrary files and open network protocols. mTLS decides *who* may call; this decides
//! *what* may run: only the shape Jellyfin emits for an HLS transcode.
//!
//! - Inputs: `file:` or absolute paths under the input roots; no other protocol.
//! - Exactly one output: the final `.m3u8`, under the output root; segment and init files too.
//! - Filters may not open files or sockets (`movie`, `amovie`, `sendcmd`, `zmq`, `azmq`), except
//!   subtitle burn-in reading from the read-only roots.
//! - Script/attachment/report options are refused outright.

/// Where a job may read and write, after path mapping (worker-side paths).
#[derive(Debug, Clone)]
pub struct Policy {
    pub input_roots: Vec<String>,
    /// Extra read-only roots for subtitle files and fonts (burn-in).
    pub read_roots: Vec<String>,
    pub output_root: String,
    /// Where a `Shape::Trickplay` job may write its `%0Nd.jpg` frames. `None` (the default)
    /// refuses every trickplay job outright: a missing knob fails closed, not open.
    pub trickplay_output_root: Option<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            input_roots: vec!["/media".into(), "/data/media".into()],
            read_roots: vec![
                "/config/data/data/subtitles".into(),
                "/config/data/data/attachments".into(),
            ],
            output_root: "/transcodes".into(),
            trickplay_output_root: None,
        }
    }
}

/// Options that take no value (Jellyfin's usage plus common ffmpeg booleans).
const FLAGS: &[&str] = &[
    "-y",
    "-n",
    "-re",
    "-copyts",
    "-noautorotate",
    "-autorotate",
    "-noautoscale",
    "-start_at_zero",
    "-accurate_seek",
    "-noaccurate_seek",
    "-vn",
    "-an",
    "-sn",
    "-dn",
    "-shortest",
    "-hide_banner",
    "-nostdin",
    "-stats",
    "-nostats",
    "-copyinkf",
    "-ignore_unknown",
];

/// Options refused regardless of value.
const FORBIDDEN_OPTS: &[&str] = &[
    "-filter_script",
    "-filter_complex_script",
    "-/filter",
    "-/filter_complex",
    "-attach",
    "-dump_attachment",
    "-report",
    "-protocol_whitelist",
    "-protocol_blacklist",
    "-lavfi",
    "-filter_complex_threads",
    "-sdp_file",
    "-vstats_file",
    "-passlogfile",
    "-progress",
];

/// Filters that can open files or sockets.
const FORBIDDEN_FILTERS: &[&str] = &[
    "movie", "amovie", "sendcmd", "asendcmd", "zmq", "azmq", "lv2", "ladspa",
];

fn is_flag(a: &str) -> bool {
    FLAGS.contains(&a)
}

fn is_option(a: &str) -> bool {
    // "-1", "-0:s" etc. are values (e.g. `-map -0:s`, `-hls_list_size 0`), not options
    a.len() > 1 && a.starts_with('-') && !a[1..].starts_with(|c: char| c.is_ascii_digit())
}

fn under(path: &str, roots: &[String]) -> bool {
    if !path.starts_with('/') || path.split('/').any(|c| c == "..") {
        return false;
    }
    roots.iter().any(|r| {
        let r = r.trim_end_matches('/');
        path == r || path.starts_with(&format!("{r}/"))
    })
}

fn check_input(v: &str, p: &Policy) -> Result<(), String> {
    let path = v.strip_prefix("file:").unwrap_or(v);
    let path = path.trim_matches('"');
    if !under(path, &p.input_roots) {
        return Err(format!("input not under the input roots: {v}"));
    }
    Ok(())
}

/// Split a filter *graph* into filters: on unescaped `,` and `;`, outside single quotes.
fn split_graph(graph: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = graph.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '\'' => {
                quoted = !quoted;
                cur.push(c);
            }
            ',' | ';' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// `[in][x]overlay=a=b[out]` -> ("overlay", "overlay=a=b"): link labels stripped both sides.
fn filter_name(f: &str) -> (String, String) {
    let mut s = f.trim();
    while let Some(rest) = s.strip_prefix('[') {
        s = rest
            .split_once(']')
            .map(|(_, r)| r)
            .unwrap_or("")
            .trim_start();
    }
    let mut body = s.to_string();
    while body.ends_with(']') {
        match body.rfind('[') {
            Some(i) => body.truncate(i),
            None => break,
        }
    }
    let name: String = body
        .chars()
        .take_while(|c| *c != '=' && *c != '[')
        .collect();
    (name.trim().to_string(), body)
}

fn check_filters(chain: &str, p: &Policy) -> Result<(), String> {
    for f in split_graph(chain) {
        let (name, body) = filter_name(&f);
        // ffmpeg's own filter-graph parser matches filter names exactly as registered
        // (lowercase); fold case here too so `Movie=`/`MOVIE=` etc. cannot slip past a
        // literal lowercase comparison (defense in depth if that ever changes).
        let lower = name.to_ascii_lowercase();
        let name = lower.as_str();
        let body = body.as_str();
        if FORBIDDEN_FILTERS.contains(&name) {
            return Err(format!("filter {name} may open files or sockets"));
        }
        if name == "subtitles" || name == "ass" {
            let (_, opts) = crate::filters::filter_opts(body);
            for (k, v) in opts {
                if matches!(k.as_str(), "f" | "filename" | "fontsdir") {
                    let path = v.trim_matches('\'');
                    let mut roots = p.read_roots.clone();
                    roots.extend(p.input_roots.iter().cloned());
                    if !under(path, &roots) {
                        return Err(format!("subtitle path not under the read roots: {path}"));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Accept only Jellyfin's HLS transcode shape, or (for `Shape::Trickplay`) its trickplay sprite
/// extraction shape -- see the module docs. `shape` is a parameter (not re-derived from `args`
/// here) so the two shapes' rules can never be cross-called by mistake.
pub fn validate(args: &[String], p: &Policy, shape: crate::Shape) -> Result<(), String> {
    let out_root = std::slice::from_ref(&p.output_root);
    let mut inputs = 0;
    let mut outputs = 0;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if FORBIDDEN_OPTS
            .iter()
            .any(|f| a == *f || a.starts_with(&format!("{f}:")))
        {
            return Err(format!("option {a} is not allowed"));
        }
        if is_flag(a) {
            i += 1;
            continue;
        }
        if is_option(a) {
            let v = args
                .get(i + 1)
                .ok_or_else(|| format!("option {a} has no value"))?
                .as_str();
            match a {
                "-i" => {
                    check_input(v, p)?;
                    inputs += 1;
                }
                "-hls_segment_filename" => {
                    if !under(v, out_root) {
                        return Err(format!("segment path not under the output root: {v}"));
                    }
                }
                "-hls_fmp4_init_filename" => {
                    let v = v.trim_matches('"');
                    if v.contains('/') && !under(v, out_root) {
                        return Err(format!("init path not under the output root: {v}"));
                    }
                }
                "-f" if v == "tee" => return Err("output format tee is not allowed".into()),
                _ if a == "-vf"
                    || a.starts_with("-vf:")
                    || a.starts_with("-filter")
                    || a == "-af"
                    || a.starts_with("-af:") =>
                {
                    check_filters(v, p)?
                }
                // Same "unknown 0-arity option swallows the next token" gap as the path check
                // below, but for a *forbidden* option hidden as the value instead of a path: e.g.
                // `-xerror` isn't in FLAGS, so `-xerror -report` would otherwise let `-report`
                // (writes an ffmpeg-*.log; see FORBIDDEN_OPTS's doc comment) sail through as
                // `-xerror`'s "value" without ever being checked against FORBIDDEN_OPTS as `a`.
                _ if FORBIDDEN_OPTS
                    .iter()
                    .any(|f| v == *f || v.starts_with(&format!("{f}:"))) =>
                {
                    return Err(format!(
                        "option {a} hides the forbidden option {v} as its value"
                    ));
                }
                // Any other option's value is normally opaque (a bitrate, a preset name, a
                // metadata string, ...). But we don't know every ffmpeg option's arity, and a
                // 0-arity option we don't recognise as a flag would (in the real ffmpeg parser)
                // leave its *next* token as a fresh positional, i.e. a second output, while our
                // scanner would instead swallow that token here as this option's "value" and
                // never check it. Bound the damage: an absolute-path-shaped value must land under
                // THIS shape's own output root -- the only place a hidden second output could
                // legitimately go and still be caught by the "exactly one output" accounting.
                // input_roots/read_roots must NOT satisfy this: those are read locations, and a
                // value landing there is exactly the hidden-second-output case this branch exists
                // to bound (e.g. `-xerror /media/movies/newfile.mkv`, a genuine 0-arity ffmpeg
                // global not in FLAGS, parses as a second output positional in real ffmpeg -- an
                // agent write into the Jellyfin media library, not merely an unchecked read).
                _ if v.starts_with('/') => {
                    let own_output_root = match shape {
                        crate::Shape::Hls => Some(p.output_root.clone()),
                        crate::Shape::Trickplay => p.trickplay_output_root.clone(),
                        crate::Shape::Other => None,
                    };
                    let ok =
                        own_output_root.is_some_and(|root| under(v, std::slice::from_ref(&root)));
                    if !ok {
                        return Err(format!("option {a} has an unexpected path value: {v}"));
                    }
                }
                _ => {}
            }
            i += 2;
            continue;
        }
        // positional: the only one allowed is the final output, whose shape depends on `shape`
        let is_final_output = i == args.len() - 1
            && match shape {
                crate::Shape::Hls => a.ends_with(".m3u8"),
                crate::Shape::Trickplay => crate::is_trickplay_pattern(a),
                crate::Shape::Other => false,
            };
        if is_final_output {
            match shape {
                crate::Shape::Hls => {
                    if !under(a, out_root) {
                        return Err(format!("output not under the output root: {a}"));
                    }
                }
                crate::Shape::Trickplay => {
                    let root = p
                        .trickplay_output_root
                        .as_ref()
                        .ok_or_else(|| "trickplay output root is not configured".to_string())?;
                    if !under(a, std::slice::from_ref(root)) {
                        return Err(format!(
                            "trickplay output not under the trickplay output root: {a}"
                        ));
                    }
                }
                crate::Shape::Other => unreachable!("is_final_output is false for Shape::Other"),
            }
            outputs += 1;
        } else {
            return Err(format!(
                "unexpected positional argument (a second output?): {a}"
            ));
        }
        i += 1;
    }
    if inputs == 0 {
        return Err("no input".into());
    }
    if outputs != 1 {
        return Err("exactly one output is required".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Shape;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn base() -> Vec<String> {
        s(&[
            "-i",
            "file:/media/movies/x.mkv",
            "-codec:v:0",
            "libx264",
            "-map",
            "-0:s",
            "-f",
            "hls",
            "-hls_segment_filename",
            "/transcodes/tc/a%d.ts",
            "-y",
            "/transcodes/tc/a.m3u8",
        ])
    }

    #[test]
    fn accepts_the_jellyfin_shape() {
        assert_eq!(validate(&base(), &Policy::default(), Shape::Hls), Ok(()));
    }

    #[test]
    fn rejects_attacks() {
        let p = Policy::default();
        let cases: Vec<Vec<String>> = vec![
            // network / other protocols and paths outside the roots
            {
                let mut a = base();
                a[1] = "http://evil/x".into();
                a
            },
            {
                let mut a = base();
                a[1] = "concat:/media/a|/etc/passwd".into();
                a
            },
            {
                let mut a = base();
                a[1] = "file:/media/../etc/shadow".into();
                a
            },
            {
                let mut a = base();
                a[1] = "/etc/passwd".into();
                a
            }, // a second output
            {
                let mut a = base();
                a.insert(a.len() - 1, "/tmp/leak.mkv".into());
                a
            }, // writes outside the transcode dir
            {
                let mut a = base();
                let n = a.len();
                a[n - 1] = "/config/x.m3u8".into();
                a
            },
            {
                let mut a = base();
                a[9] = "/etc/cron.d/x%d.ts".into();
                a
            }, // file/socket-opening filters and script options
            {
                let mut a = base();
                a.splice(4..4, s(&["-vf", "movie=/etc/passwd[x];[in][x]overlay"]));
                a
            },
            {
                let mut a = base();
                a.splice(4..4, s(&["-vf", "subtitles=f='/etc/passwd'"]));
                a
            },
            {
                let mut a = base();
                a.splice(4..4, s(&["-filter_script:v", "/tmp/f"]));
                a
            },
            {
                let mut a = base();
                a.splice(4..4, s(&["-f", "tee"]));
                a
            },
        ];
        for (n, c) in cases.iter().enumerate() {
            assert!(
                validate(c, &p, Shape::Hls).is_err(),
                "case {n} should be rejected: {c:?}"
            );
        }
    }

    // Regressions for bypasses found while fuzzing/property-testing the allowlist. Each of
    // these was accepted by an earlier version of `validate()`.

    #[test]
    fn bypass_stream_specifier_on_vf_skipped_the_filter_check() {
        // `-vf:v` (or `-vf:0`) didn't match the exact `a == "-vf"` check, so `check_filters`
        // was never called and `movie=` sailed through.
        let mut a = base();
        a.splice(4..4, s(&["-vf:v", "movie=/etc/passwd"]));
        assert!(validate(&a, &Policy::default(), Shape::Hls).is_err());
    }

    #[test]
    fn bypass_stream_specifier_on_af_skipped_the_filter_check() {
        let mut a = base();
        a.splice(4..4, s(&["-af:a", "movie=/etc/passwd"]));
        assert!(validate(&a, &Policy::default(), Shape::Hls).is_err());
    }

    #[test]
    fn bypass_uppercase_filter_name() {
        // Filter-name comparisons were case-sensitive; `Movie=`/`MOVIE=` slipped past the
        // literal-lowercase `FORBIDDEN_FILTERS` check.
        let mut a = base();
        a.splice(4..4, s(&["-vf", "Movie=/etc/passwd[x];[in][x]overlay"]));
        assert!(validate(&a, &Policy::default(), Shape::Hls).is_err());
        let mut a = base();
        a.splice(4..4, s(&["-vf", "SENDCMD=/etc/passwd"]));
        assert!(validate(&a, &Policy::default(), Shape::Hls).is_err());
    }

    #[test]
    fn bypass_uppercase_subtitles_filter_reads_outside_roots() {
        let mut a = base();
        a.splice(4..4, s(&["-vf", "Subtitles=f='/etc/passwd'"]));
        assert!(validate(&a, &Policy::default(), Shape::Hls).is_err());
    }

    #[test]
    fn bypass_unknown_zero_arity_option_hides_a_second_output() {
        // A 0-arity option we don't recognise as a FLAG (e.g. a real ffmpeg boolean option not
        // in our list) would, in the real ffmpeg parser, leave its next token as a fresh
        // positional output. Our scanner used to swallow that token unchecked as the "value" of
        // the unrecognised option instead of validating it.
        let mut a = base();
        a.splice(4..4, s(&["-xerror", "/tmp/leak.mkv"]));
        assert!(validate(&a, &Policy::default(), Shape::Hls).is_err());
    }

    #[test]
    fn bypass_unknown_zero_arity_option_smuggles_output_under_an_input_root() {
        // The catch-all above bounds a swallowed 0-arity option's value to *some* already-reachable
        // root, but used to accept input_roots/read_roots too, not just the shape's own output
        // root. That doesn't bound the hidden-second-output case at all: real ffmpeg parses
        // `-xerror /media/movies/newfile.mkv` as a second output (a fresh write into the Jellyfin
        // media library), not as `-xerror`'s value, and `/media/...` sits squarely under
        // Policy::default()'s input_roots. Only the shape's own output root may satisfy this
        // branch.
        let mut a = base();
        a.splice(4..4, s(&["-xerror", "/media/movies/newfile.mkv"]));
        assert!(
            validate(&a, &Policy::default(), Shape::Hls).is_err(),
            "an absolute path under an input root must not satisfy the swallowed-value check"
        );
        let p = Policy {
            trickplay_output_root: Some("/transcodes/trickplay".into()),
            ..Policy::default()
        };
        let mut t = trickplay();
        let n = t.len();
        t.splice(n - 1..n - 1, s(&["-xerror", "/media/movies/newfile.mkv"]));
        assert!(
            validate(&t, &p, Shape::Trickplay).is_err(),
            "same smuggle under Shape::Trickplay, still under an input root, not the trickplay root"
        );
    }

    #[test]
    fn under_rejects_relative_and_dotdot_and_double_slash_tricks() {
        let roots = vec!["/media".to_string()];
        assert!(!under("media/x", &roots)); // relative
        assert!(!under("/media/../etc/passwd", &roots)); // dotdot
        assert!(!under("//etc/passwd", &roots)); // not under /media at all
        assert!(!under("/medias/evil", &roots)); // prefix-but-not-a-path-segment
        assert!(under("/media/x", &roots));
        assert!(under("/media", &roots));
    }

    fn trickplay() -> Vec<String> {
        s(&[
            "-loglevel",
            "error",
            "-i",
            "file:/media/movies/x.mkv",
            "-map",
            "0:0",
            "-an",
            "-sn",
            "-vf",
            "setpts=N/23.976/TB,fps=0.1,scale=320:-2",
            "-c:v",
            "mjpeg",
            "-qscale:v",
            "4",
            "-vsync",
            "0",
            "-f",
            "image2",
            "/transcodes/trickplay/guid1/%08d.jpg",
        ])
    }

    #[test]
    fn trickplay_shape_accepted_under_its_output_root() {
        let p = Policy {
            trickplay_output_root: Some("/transcodes/trickplay".into()),
            ..Policy::default()
        };
        assert_eq!(validate(&trickplay(), &p, Shape::Trickplay), Ok(()));
    }

    #[test]
    fn trickplay_shape_rejected_when_root_is_unset() {
        // A missing knob fails closed: Policy::default() leaves trickplay_output_root None.
        assert!(validate(&trickplay(), &Policy::default(), Shape::Trickplay).is_err());
    }

    #[test]
    fn trickplay_shape_rejected_when_output_escapes_the_root() {
        let p = Policy {
            trickplay_output_root: Some("/transcodes/trickplay".into()),
            ..Policy::default()
        };
        let mut a = trickplay();
        let n = a.len();
        a[n - 1] = "/transcodes/other/%08d.jpg".into();
        assert!(validate(&a, &p, Shape::Trickplay).is_err());
        let mut b = trickplay();
        let n = b.len();
        b[n - 1] = "/transcodes/trickplay/../other/%08d.jpg".into();
        assert!(validate(&b, &p, Shape::Trickplay).is_err());
    }

    #[test]
    fn trickplay_shape_rejects_a_non_pattern_final_positional() {
        let p = Policy {
            trickplay_output_root: Some("/transcodes/trickplay".into()),
            ..Policy::default()
        };
        let mut a = trickplay();
        let n = a.len();
        a[n - 1] = "/transcodes/trickplay/guid1/frame.jpg".into(); // chapter-image shape, no %0Nd
        assert!(validate(&a, &p, Shape::Trickplay).is_err());
    }

    #[test]
    fn hls_shape_still_rejects_a_trickplay_looking_output() {
        // The two shapes' rules must not cross over: an HLS-shaped call is still only accepted
        // with a trailing .m3u8, even if it happens to end in a %0Nd.jpg pattern.
        let mut a = trickplay();
        let n = a.len();
        a[n - 1] = "/transcodes/trickplay/guid1/%08d.jpg".into();
        assert!(validate(&a, &Policy::default(), Shape::Hls).is_err());
    }

    #[test]
    fn bypass_unknown_zero_arity_option_hides_a_forbidden_option() {
        // Same class as `bypass_unknown_zero_arity_option_hides_a_second_output`, but for a
        // FORBIDDEN_OPTS entry instead of a path: `-xerror` isn't in FLAGS, so `-report` used to
        // sail through unchecked as its "value".
        let mut a = base();
        a.splice(4..4, s(&["-xerror", "-report"]));
        assert!(validate(&a, &Policy::default(), Shape::Hls).is_err());
        // The `:`-suffixed form (mirrors how FORBIDDEN_OPTS itself matches `-report:x`) too.
        let mut b = base();
        b.splice(4..4, s(&["-xerror", "-passlogfile:v"]));
        assert!(validate(&b, &Policy::default(), Shape::Hls).is_err());
    }

    // P5 (docs/engineering/transcode-plan.md): the DV7->8.1 signal (`crate::DV81_SIGNAL_FLAG`/
    // `DV81_SIGNAL_VALUE`) is not special-cased anywhere in this allowlist -- it is just another
    // `-metadata:s:vN` option/value pair, which was already opaque to `validate()` before P5
    // existed. This test is regression coverage for that fact, not a widening: if a future change
    // to this file ever makes the signal rejected (or, worse, makes it trigger different
    // acceptance behaviour), this must fail loudly.
    #[test]
    fn dv81_signal_is_already_opaque_to_the_allowlist() {
        let mut a = base();
        a.splice(
            4..4,
            s(&[crate::DV81_SIGNAL_FLAG, crate::DV81_SIGNAL_VALUE]),
        );
        assert_eq!(validate(&a, &Policy::default(), Shape::Hls), Ok(()));
    }

    #[test]
    fn dv81_signal_alias_is_also_opaque_to_the_allowlist() {
        let mut a = base();
        a.splice(
            4..4,
            s(&[crate::DV81_SIGNAL_FLAG, crate::DV81_SIGNAL_VALUE_ALIAS]),
        );
        assert_eq!(validate(&a, &Policy::default(), Shape::Hls), Ok(()));
    }
}
