//! Jellyfin's software video filter chain -> a backend's GPU chain.
//!
//! Only the shapes Jellyfin emits for plain transcodes are translated (`setparams`, `scale`,
//! `tonemapx`, `format`). Anything else (subtitle burn-in, deinterlace, overlays) returns `None`
//! and the job keeps the CPU chain.

use crate::Backend;

/// Split a filter chain on commas that are not escaped (scale expressions use `\,`).
pub fn split_filters(vf: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = vf.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&n) = chars.peek() {
                cur.push(c);
                cur.push(n);
                chars.next();
                continue;
            }
        }
        if c == ',' {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    parts.push(cur);
    parts
}

/// `tonemapx=tonemap=bt2390:peak=100` -> (`tonemapx`, [(tonemap, bt2390), (peak, 100)]).
/// Keeps option order; a later duplicate key replaces the earlier value in place.
pub fn filter_opts(filt: &str) -> (&str, Vec<(String, String)>) {
    let (name, rest) = match filt.split_once('=') {
        Some((n, r)) => (n, Some(r)),
        None => (filt, None),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    if let Some(rest) = rest {
        for kv in rest.split(':') {
            if let Some((k, v)) = kv.split_once('=') {
                if let Some(slot) = out.iter_mut().find(|(ek, _)| ek == k) {
                    slot.1 = v.to_string();
                } else {
                    out.push((k.to_string(), v.to_string()));
                }
            }
        }
    }
    (name, out)
}

fn get<'a>(opts: &'a [(String, String)], key: &str) -> Option<&'a str> {
    opts.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// The GPU filter chain for `backend`, or `None` to keep the CPU chain.
pub fn hw_filters(vf: &str, backend: Backend) -> Option<String> {
    let parts = split_filters(vf);
    const KNOWN: [&str; 4] = ["setparams", "scale", "tonemapx", "format"];
    if !parts.iter().all(|p| KNOWN.contains(&filter_opts(p).0)) {
        return None;
    }
    let setparams: Vec<&String> = parts
        .iter()
        .filter(|p| p.starts_with("setparams="))
        .collect();
    let scale = parts.iter().find_map(|p| p.strip_prefix("scale="));
    let tone = parts
        .iter()
        .find(|p| p.starts_with("tonemapx="))
        .map(|p| filter_opts(p).1);
    let out_fmt = tone
        .as_ref()
        .and_then(|t| get(t, "format").map(str::to_string))
        .or_else(|| {
            parts
                .iter()
                .find_map(|p| p.strip_prefix("format=").map(str::to_string))
        })
        .unwrap_or_else(|| "yuv420p".to_string());
    let ten = out_fmt.contains("10");
    let (w, h) = match scale {
        Some(s) => match s.split_once(':') {
            Some((w, h)) => (w, h),
            None => (s, ""),
        },
        None => ("", ""),
    };
    let mut chain: Vec<String> = setparams.iter().map(|s| s.to_string()).collect();
    match backend {
        Backend::Qsv => {
            let fmt = if ten { "p010" } else { "nv12" };
            if scale.is_some() {
                let f = if tone.is_some() {
                    String::new()
                } else {
                    format!(":format={fmt}")
                };
                chain.push(format!("scale_vaapi=w={w}:h={h}{f}:extra_hw_frames=24"));
            }
            if let Some(t) = &tone {
                chain.push("procamp_vaapi=b=16".into());
                chain.push(format!(
                    "tonemap_vaapi=format={fmt}:p={}:t={}:m={}:extra_hw_frames=32",
                    get(t, "p").unwrap_or("bt709"),
                    get(t, "t").unwrap_or("bt709"),
                    get(t, "m").unwrap_or("bt709"),
                ));
            }
            if scale.is_none() && tone.is_none() {
                chain.push(format!("scale_vaapi=format={fmt}"));
            }
            chain.push("hwmap=derive_device=qsv".into());
            chain.push("format=qsv".into());
            Some(chain.join(","))
        }
        Backend::Nvenc => {
            let fmt = if ten { "p010" } else { "yuv420p" };
            if let Some(t) = &tone {
                if scale.is_some() {
                    chain.push(format!("scale_cuda=w={w}:h={h}")); // keeps p010 for the tonemapper
                }
                let mut s = format!("tonemap_cuda=format={fmt}");
                for k in ["tonemap", "desat", "peak", "t", "m", "p"] {
                    if let Some(v) = get(t, k) {
                        s.push_str(&format!(":{k}={v}"));
                    }
                }
                chain.push(s);
            } else if scale.is_some() {
                chain.push(format!("scale_cuda=w={w}:h={h}:format={fmt}"));
            } else {
                chain.push(format!("scale_cuda=format={fmt}"));
            }
            Some(chain.join(","))
        }
        Backend::Cpu => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_respects_escaped_commas() {
        let vf = r"setparams=a=b,scale=trunc(min(max(iw\,ih*a)\,1920)/2)*2:trunc(ow/a/2)*2,format=yuv420p";
        assert_eq!(split_filters(vf).len(), 3);
    }

    #[test]
    fn subtitles_stay_on_cpu() {
        assert_eq!(
            hw_filters("scale=1920:-2,subtitles=f=/x.srt", Backend::Qsv),
            None
        );
    }
}
