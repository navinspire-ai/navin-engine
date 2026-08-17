//! Scan a service log for known failure signatures. Substring matching on
//! lowercased text keeps this dependency-free and predictable; each hit
//! carries the offending line as evidence.

/// A recognised log signature and the cause it points at.
#[derive(Debug, Clone, Copy)]
pub struct LogSignal {
    /// Lowercased marker searched for in each line.
    pub marker: &'static str,
    /// Stable slug used to correlate with symptoms.
    pub id: &'static str,
    pub family: &'static str,
    pub cause: &'static str,
}

/// The catalogue, most specific first (first match per line wins).
pub const SIGNALS: &[LogSignal] = &[
    LogSignal { marker: "panicked at", id: "rust_panic", family: "reliability", cause: "a Rust panic aborted a request handler" },
    LogSignal { marker: "traceback (most recent call last)", id: "py_exception", family: "reliability", cause: "an unhandled Python exception" },
    LogSignal { marker: "segmentation fault", id: "segfault", family: "memory", cause: "a segmentation fault (memory-safety bug)" },
    LogSignal { marker: "sigsegv", id: "segfault", family: "memory", cause: "a segmentation fault (memory-safety bug)" },
    LogSignal { marker: "out of memory", id: "oom", family: "memory", cause: "the process ran out of memory" },
    LogSignal { marker: "cannot allocate memory", id: "oom", family: "memory", cause: "an allocation failed (memory pressure)" },
    LogSignal { marker: "address already in use", id: "port_in_use", family: "reliability", cause: "the listen port was still held from a previous instance" },
    LogSignal { marker: "eaddrinuse", id: "port_in_use", family: "reliability", cause: "the listen port was still held from a previous instance" },
    LogSignal { marker: "too many open files", id: "fd_exhaustion", family: "reliability", cause: "file-descriptor exhaustion under load" },
    LogSignal { marker: "emfile", id: "fd_exhaustion", family: "reliability", cause: "file-descriptor exhaustion under load" },
    LogSignal { marker: "connection refused", id: "conn_refused", family: "reliability", cause: "a dependency the service needs was unreachable" },
    LogSignal { marker: "deadlock", id: "deadlock", family: "concurrency", cause: "a lock ordering / deadlock condition" },
    LogSignal { marker: "uncaught exception", id: "uncaught", family: "reliability", cause: "an uncaught exception crashed the handler" },
    LogSignal { marker: "unhandled rejection", id: "uncaught", family: "reliability", cause: "an unhandled promise rejection" },
];

#[derive(Debug, Clone)]
pub struct SignalHit {
    pub id: &'static str,
    pub family: &'static str,
    pub cause: &'static str,
    /// The trimmed log line that matched.
    pub line: String,
}

/// Return the distinct signals found in `log_text`, each with the first
/// line that triggered it.
pub fn scan(log_text: &str) -> Vec<SignalHit> {
    let mut hits: Vec<SignalHit> = Vec::new();
    for raw in log_text.lines() {
        let lower = raw.to_lowercase();
        for sig in SIGNALS {
            if lower.contains(sig.marker) {
                let already = hits.iter().any(|h| h.id == sig.id);
                if !already {
                    hits.push(SignalHit {
                        id: sig.id,
                        family: sig.family,
                        cause: sig.cause,
                        line: raw.trim().chars().take(200).collect(),
                    });
                }
                break; // one signal per line
            }
        }
    }
    hits
}

/// Convenience: does the log contain a signal with this id?
pub fn has(hits: &[SignalHit], id: &str) -> bool {
    hits.iter().any(|h| h.id == id)
}

/// The first hit for an id, if any.
pub fn find<'a>(hits: &'a [SignalHit], id: &str) -> Option<&'a SignalHit> {
    hits.iter().find(|h| h.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_panic_and_a_port_conflict() {
        let log = "starting up\nthread 'main' panicked at src/main.rs:10\nError: listen EADDRINUSE address already in use :::3000\nserving";
        let hits = scan(log);
        assert!(has(&hits, "rust_panic"));
        assert!(has(&hits, "port_in_use"));
        assert!(find(&hits, "rust_panic").unwrap().line.contains("panicked"));
    }

    #[test]
    fn deduplicates_repeated_signals() {
        let log = "out of memory\nout of memory\nout of memory";
        let hits = scan(log);
        assert_eq!(hits.iter().filter(|h| h.id == "oom").count(), 1);
    }

    #[test]
    fn clean_log_yields_nothing() {
        assert!(scan("all good\nrequest served in 3ms\n").is_empty());
    }
}
