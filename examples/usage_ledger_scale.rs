//! Usage-ledger scale probe: every one of `N` tokens active in every hour of
//! `DAYS` days and every minute of the last day, flushed as production would.
//! Prints peak RSS growth, store size, worst flush, and query latencies.
//!
//! ```bash
//! cargo build --release --locked --example usage_ledger_scale
//! N=100 DAYS=31 DIR=$(mktemp -d ~/.ctmp/usage.XXXX) \
//!   target/release/examples/usage_ledger_scale
//! ```
//!
//! Timings are observational; do not turn them into CI thresholds.

#![allow(clippy::pedantic)] // A throwaway measurement harness, not product code.

use ramjet::{usage::Accumulator, usage_ledger::*};
use std::time::{Duration, Instant};

fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

fn main() {
    let n: usize = std::env::var("N").unwrap().parse().unwrap();
    let days: u64 = std::env::var("DAYS")
        .map(|d| d.parse().unwrap())
        .unwrap_or(90);
    let dir = std::path::PathBuf::from(std::env::var("DIR").unwrap());
    let ledger = UsageLedger::new(LedgerSettings::default(), None, Some(&dir)).unwrap();
    let names: Vec<String> = (0..n).map(|i| format!("{i:012x}")).collect();
    let usage = Accumulator {
        prompt: Some(20000.0),
        cached: Some(15000.0),
        completion: Some(300.0),
        finish_reason: "stop".into(),
        ..Default::default()
    };
    let now = unix_ms() - unix_ms() % HOUR_MS;
    let base = rss_kib();
    let started = Instant::now();
    let mut max_flush = Duration::ZERO;
    let mut peak = 0;
    let record = |t: u64, p: &str, i: usize| {
        ledger.record_at(
            t,
            Completion {
                principal: p,
                model: Some("qwen3.8-flash-next"),
                endpoint: "chat",
                upstream: Some(i % 2),
                status: 200,
                stream: true,
                result: "complete",
                usage: Some(&usage),
                first_token: Some(Duration::from_millis(100 + (i as u64 * 37) % 3000)),
                elapsed: Duration::from_millis(2000 + (i as u64 * 91) % 30000),
                route_outcome: Some("affinity"),
                request_bytes: 80000,
                response_bytes: 4000,
            },
        );
    };
    let mut flush = |t: u64| {
        let s = Instant::now();
        ledger.flush(t).unwrap();
        max_flush = max_flush.max(s.elapsed());
        peak = peak.max(rss_kib());
    };
    for h in (24..days * 24).rev() {
        let t = now - h * HOUR_MS;
        for (i, p) in names.iter().enumerate() {
            record(t, p, i + h as usize);
        }
        flush(t);
    }
    for m in (0..24 * 60).rev() {
        let t = now - m * MINUTE_MS;
        for (i, p) in names.iter().enumerate() {
            record(t, p, i + m as usize);
        }
        flush(t);
    }
    let fill = started.elapsed();
    let size: u64 = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    let time = |q: SeriesQuery| {
        let t = Instant::now();
        let r = ledger.series(&q, now).unwrap();
        (t.elapsed(), r.totals.len())
    };
    let (all30, groups) = time(SeriesQuery {
        granularity: Granularity::Hour,
        from_ms: Some(now - 30 * DAY_MS),
        to_ms: Some(now),
        ..Default::default()
    });
    let (day90, _) = time(SeriesQuery {
        granularity: Granularity::Day,
        group_by: GroupBy::None,
        from_ms: Some(now - 90 * DAY_MS),
        to_ms: Some(now),
        ..Default::default()
    });
    let (one30, _) = time(SeriesQuery {
        granularity: Granularity::Hour,
        token: Some(names[n / 2].clone()),
        group_by: GroupBy::Model,
        from_ms: Some(now - 30 * DAY_MS),
        to_ms: Some(now),
        ..Default::default()
    });
    let (min24, _) = time(SeriesQuery {
        granularity: Granularity::Minute,
        token: Some(names[0].clone()),
        from_ms: Some(now - DAY_MS),
        to_ms: Some(now),
        ..Default::default()
    });
    let t = Instant::now();
    let r = ledger
        .recent(&RecentQuery {
            token: Some(names[1].clone()),
            ..Default::default()
        })
        .unwrap();
    let recent = t.elapsed();
    println!(
        "tokens={n} days={days} fill={fill:.1?} peak_rss=+{}MiB db={}MiB max_flush={max_flush:.1?} | 30d_by_token={all30:.0?} ({groups} groups) 90d_fleet_daily={day90:.0?} 30d_one_token={one30:.0?} 24h_one_token_minutely={min24:.0?} recent_one_token={recent:.0?} ({} rows)",
        (peak.max(rss_kib()) - base) / 1024,
        size >> 20,
        r.len()
    );
}
