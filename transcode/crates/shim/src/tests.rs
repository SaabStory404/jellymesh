//! Unit tests for worker discovery and ranking (the parts that are pure functions).

use super::*;

fn target(name: &str, addr: &str) -> Target {
    Target {
        name: name.into(),
        addr: addr.into(),
    }
}

#[test]
fn dns_target_defaults_to_the_agent_port() {
    assert_eq!(
        parse_dns_target("tcpool-agents.media.svc.cluster.local."),
        Some((
            "tcpool-agents.media.svc.cluster.local.".into(),
            tcpool_proto::DEFAULT_PORT
        ))
    );
}

#[test]
fn dns_target_keeps_the_trailing_dot_and_reads_the_port() {
    assert_eq!(
        parse_dns_target("tcpool-agents.media.svc.cluster.local.:9901"),
        Some(("tcpool-agents.media.svc.cluster.local.".into(), 9901))
    );
    assert_eq!(
        parse_dns_target(" tcpool-agents:19901 "),
        Some(("tcpool-agents".into(), 19901))
    );
}

#[test]
fn dns_target_accepts_a_bracketed_v6_literal() {
    assert_eq!(
        parse_dns_target("[fd00::1]:9901"),
        Some(("fd00::1".into(), 9901))
    );
    assert_eq!(
        parse_dns_target("[fd00::1]"),
        Some(("fd00::1".into(), tcpool_proto::DEFAULT_PORT))
    );
}

#[test]
fn dns_target_refuses_what_it_cannot_read_unambiguously() {
    // unset, a bare v6 literal (is the last group a port?), a junk port, no host
    assert_eq!(parse_dns_target(""), None);
    assert_eq!(parse_dns_target("   "), None);
    assert_eq!(parse_dns_target("fd00::1:9901"), None);
    assert_eq!(parse_dns_target("tcpool-agents:not-a-port"), None);
    assert_eq!(parse_dns_target(":9901"), None);
    assert_eq!(parse_dns_target("[]:9901"), None);
}

#[test]
fn static_workers_fill_in_addresses_dns_did_not_return() {
    let dns = vec![target("10.42.0.7:9901", "10.42.0.7:9901")];
    let statics = vec![
        // same address, already known from DNS: not duplicated
        ("qsv".to_string(), "10.42.0.7:9901".to_string()),
        ("nv".to_string(), "10.42.1.9:9901".to_string()),
    ];
    assert_eq!(
        merge_targets(dns, statics),
        vec![
            target("10.42.0.7:9901", "10.42.0.7:9901"),
            target("nv", "10.42.1.9:9901"),
        ]
    );
}

#[test]
fn merge_keeps_the_static_list_alone_when_dns_is_unset() {
    assert_eq!(
        merge_targets(Vec::new(), vec![("cpu".into(), "127.0.0.1:9901".into())]),
        vec![target("cpu", "127.0.0.1:9901")]
    );
}

/// `connect_lazy` dials nothing, but it does need a reactor in scope, so the tests that build a
/// `Worker` are `#[tokio::test]`. `rank()` itself is synchronous and only reads `caps`.
fn worker(name: &str, kind: &str, capacity: f64, used: f64, order: usize) -> Worker {
    Worker {
        name: name.into(),
        channel: Endpoint::from_static("http://127.0.0.1:1").connect_lazy(),
        caps: Caps {
            name: name.into(),
            kind: kind.into(),
            outputs: vec!["h264".into()],
            capacity,
            units_used: used,
            ..Default::default()
        },
        order,
    }
}

#[tokio::test]
async fn an_idle_pool_prefers_a_gpu_over_the_cpu_spill() {
    // Everything free, and DNS handed the CPU worker back first: kind must break the tie.
    let ws = vec![
        worker("cpu", "cpu", 4.0, 0.0, 0),
        worker("nv", "nvenc", 6.0, 0.0, 1),
        worker("qsv", "qsv", 14.0, 0.0, 2),
    ];
    let names: Vec<String> = rank(ws, Some("h264"))
        .iter()
        .map(|w| w.label().to_string())
        .collect();
    assert_eq!(names, vec!["nv", "qsv", "cpu"]);
}

#[tokio::test]
async fn free_capacity_still_wins_over_kind() {
    let ws = vec![
        worker("qsv", "qsv", 14.0, 13.0, 0), // 7% free
        worker("cpu", "cpu", 4.0, 0.0, 1),   // idle
    ];
    let names: Vec<String> = rank(ws, None)
        .iter()
        .map(|w| w.label().to_string())
        .collect();
    assert_eq!(names, vec!["cpu", "qsv"]);
}

#[tokio::test]
async fn a_worker_without_the_required_output_is_dropped() {
    let ws = vec![worker("qsv", "qsv", 14.0, 0.0, 0)];
    assert!(rank(ws, Some("av1")).is_empty());
}

#[tokio::test]
async fn the_log_label_falls_back_to_the_dialled_address() {
    let mut w = worker("10.42.0.7:9901", "qsv", 14.0, 0.0, 0);
    w.caps.name = String::new();
    assert_eq!(w.label(), "10.42.0.7:9901");
}
