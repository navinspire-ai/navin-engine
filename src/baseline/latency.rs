//! Minimal HTTP/1.1 latency probe for localhost shadows (plain http only,
//! no TLS: probes must never target a production endpoint).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LatencyStats {
    pub requests: u64,
    pub failures: u64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub rps: f64,
}

/// Index into a sorted sample set for quantile `q` (nearest-rank).
pub fn percentile(sorted_ms: &[f64], q: f64) -> f64 {
    if sorted_ms.is_empty() {
        return 0.0;
    }
    let rank = (q * sorted_ms.len() as f64).ceil() as usize;
    sorted_ms[rank.clamp(1, sorted_ms.len()) - 1]
}

/// One HTTP GET, returning the round-trip in milliseconds. Public so the
/// Proof engine can reuse it as a health probe.
pub async fn get_once(host: &str, port: u16, path: &str) -> Result<f64> {
    one_request(host, port, path).await
}

async fn one_request(host: &str, port: u16, path: &str) -> Result<f64> {
    let started = Instant::now();
    let mut stream = TcpStream::connect((host, port)).await?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: navin-engine\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut buf = Vec::with_capacity(4096);
    stream.read_to_end(&mut buf).await?;
    anyhow::ensure!(buf.starts_with(b"HTTP/1."), "not an HTTP response");
    Ok(started.elapsed().as_secs_f64() * 1000.0)
}

/// Sequentially probe for `duration` with `concurrency` parallel loops.
pub async fn probe(
    host: &str,
    port: u16,
    path: &str,
    duration: Duration,
    concurrency: usize,
) -> LatencyStats {
    let deadline = Instant::now() + duration;
    let mut tasks = Vec::new();
    for _ in 0..concurrency.max(1) {
        let host = host.to_owned();
        let path = path.to_owned();
        tasks.push(tokio::spawn(async move {
            let mut samples = Vec::new();
            let mut failures = 0u64;
            while Instant::now() < deadline {
                match tokio::time::timeout(
                    Duration::from_secs(10),
                    one_request(&host, port, &path),
                )
                .await
                {
                    Ok(Ok(ms)) => samples.push(ms),
                    _ => failures += 1,
                }
            }
            (samples, failures)
        }));
    }

    let mut all = Vec::new();
    let mut failures = 0u64;
    for task in tasks {
        if let Ok((samples, failed)) = task.await {
            all.extend(samples);
            failures += failed;
        }
    }
    all.sort_by(|a, b| a.partial_cmp(b).expect("latency is finite"));
    let requests = all.len() as u64;
    LatencyStats {
        requests,
        failures,
        p50_ms: round1(percentile(&all, 0.50)),
        p95_ms: round1(percentile(&all, 0.95)),
        p99_ms: round1(percentile(&all, 0.99)),
        rps: round1(requests as f64 / duration.as_secs_f64().max(0.001)),
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        let samples: Vec<f64> = (1..=100).map(|n| n as f64).collect();
        assert_eq!(percentile(&samples, 0.50), 50.0);
        assert_eq!(percentile(&samples, 0.95), 95.0);
        assert_eq!(percentile(&samples, 0.99), 99.0);
        assert_eq!(percentile(&[], 0.5), 0.0);
        assert_eq!(percentile(&[7.0], 0.99), 7.0);
    }

    #[tokio::test]
    async fn probe_measures_a_tiny_http_server() {
        // One-line HTTP server: accept, answer 200, close.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                        .await;
                });
            }
        });

        let stats = probe("127.0.0.1", port, "/", Duration::from_millis(600), 2).await;
        assert!(stats.requests > 0, "no successful requests");
        assert!(stats.p50_ms > 0.0);
        assert!(stats.p95_ms >= stats.p50_ms);
    }
}
