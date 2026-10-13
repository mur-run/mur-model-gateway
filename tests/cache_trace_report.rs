//! The report script against the hand-written fixture: every bucket, the
//! mixed-TTL weighting, the B1 line with its overlap, the fallback rule and
//! the sanity counters, with the numbers worked out in the plan
//! (docs/superpowers/plans/2026-10-13-cache-trace.md, Task 5).

use std::process::Command;

fn run(env_client: Option<&str>) -> String {
    let root = env!("CARGO_MANIFEST_DIR");
    let mut cmd = Command::new("bash");
    cmd.arg(format!("{root}/scripts/cache-trace-report.sh"))
        .arg(format!("{root}/tests/fixtures/cache-trace/sample.log"));
    if let Some(c) = env_client {
        cmd.env("MUR_REPORT_CLIENT", c);
    }
    let out = cmd.output().expect("bash runs the report");
    assert!(
        out.status.success(),
        "report failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn buckets_estimates_and_sanity_from_the_fixture() {
    let text = run(None);
    let expected = [
        "== model claude-opus-5-5 ==",
        "lines             9  with usage 9  cold starts 7  non-streaming 0 (0.0%)",
        "writes (raw)      creation 1166346  5m 246346  1h 920000  weighted 2147932.5  fallback 0",
        "  client none         lines=6  usage=6  cold=4  creation=166346",
        "  client claude-cli   lines=3  usage=3  cold=3  creation=1000000",
        "  first          n=2  creation=80000  prefix_est=69308.0  weighted=86635.0  fallback=0  (4.0% of weighted writes)",
        "  concurrent     n=1  creation=40000  prefix_est=34654.0  weighted=43317.5  fallback=0  (2.0% of weighted writes)",
        "  sequential     n=2  creation=240000  prefix_est=164654.0  weighted=295817.5  fallback=0  (13.8% of weighted writes)",
        "  expired        n=2  creation=800000  prefix_est=260000.0  weighted=520000.0  fallback=0  (24.2% of weighted writes)",
        "avoidable (B3)    n=3  creation=280000  prefix_est=199308.0  weighted=339135.0  (15.8% of weighted writes)",
        "B1 tools bp       n=2  creation=80000  tools_est=61068.0  weighted@b1_ttl=76335.0  overlap with B3: n=1 tools_est=30534.0",
        "  by bucket       first=1  concurrent=0  sequential=1  expired=0  no_prefix_bp=0",
        "  tools_bytes     min=30534  median=30534",
        "sanity            ttl!=last(bp_ttls)=0  ttl_order=0  prefix>span=0  bp_bytes=0  split_disagree=0  status!=2xx=0  read=-:0  diverged?+=1  invariant=0  seen_vs_ttl_left=1",
        "== model claude-sonnet-5-5 ==",
        "lines             4  with usage 3  cold starts 2  non-streaming 2 (50.0%)",
        "writes (raw)      creation 60000  5m 60000  1h 0  weighted 75000.0  fallback 1",
        "  first          n=1  creation=10000  prefix_est=8000.0  weighted=10000.0  fallback=1  (13.3% of weighted writes)",
        "  no_prefix_bp   n=1  creation=50000  prefix_est=34654.0  weighted=43317.5  fallback=0  (57.8% of weighted writes)",
        "    sub-classes   no_cache_control=0  top_level_only=0  msgs_only=1",
        "avoidable (B3)    n=0  creation=0  prefix_est=0.0  weighted=0.0  (0.0% of weighted writes)",
        "B1 tools bp       n=1  creation=50000  tools_est=30534.0  weighted@b1_ttl=38167.5  overlap with B3: n=0 tools_est=0.0",
        "  by bucket       first=0  concurrent=0  sequential=0  expired=0  no_prefix_bp=1",
        "  tools_bytes     min=6000  median=30534",
        "sanity            ttl!=last(bp_ttls)=0  ttl_order=0  prefix>span=0  bp_bytes=0  split_disagree=0  status!=2xx=1  read=-:1  diverged?+=0  invariant=0  seen_vs_ttl_left=0",
    ];
    for line in expected {
        assert!(
            text.lines().any(|l| l == line),
            "missing line:\n{line}\nin report:\n{text}"
        );
    }
    let opus = text.find("== model claude-opus-5-5 ==").unwrap();
    let sonnet = text.find("== model claude-sonnet-5-5 ==").unwrap();
    assert!(opus < sonnet, "models are printed in name order");
}

#[test]
fn client_filter_drops_the_other_client() {
    let text = run(Some("none"));
    assert!(!text.contains("client claude-cli"), "{text}");
    assert!(
        text.contains("  client none         lines=6  usage=6  cold=4  creation=166346"),
        "{text}"
    );
    assert!(
        text.contains("lines             6  with usage 6  cold starts 4"),
        "{text}"
    );
}

#[test]
fn an_empty_log_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty.log");
    std::fs::write(
        &empty,
        "2026-10-13T06:00:00Z DEBUG mur_model_gateway: proxied\n",
    )
    .unwrap();
    let root = env!("CARGO_MANIFEST_DIR");
    let out = Command::new("bash")
        .arg(format!("{root}/scripts/cache-trace-report.sh"))
        .arg(&empty)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "none logged");
}
