//! Where the OTLP exporter actually POSTs.
//!
//! A real socket stands in for the collector: `telemetry::init` runs against it, a span is emitted,
//! and the guard's flush drives one real export. Its own test binary, so setting
//! `OTEL_EXPORTER_OTLP_ENDPOINT` cannot race another test's environment.

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

/// A span exported through the daemon's telemetry setup must arrive at the collector's
/// `/v1/traces` signal path, not at the endpoint root. Passing the endpoint to the builder puts it
/// on the wire verbatim, so every export 404s and nothing surfaces the failure.
#[test]
fn spans_export_to_the_v1_traces_path() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("binding the collector stand-in");
    let port = listener
        .local_addr()
        .expect("reading the collector's port")
        .port();
    let (tx, rx) = mpsc::channel();

    let collector = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accepting the export connection");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        // Read only the request line + headers; the protobuf body is irrelevant to the assertion.
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => head.push(byte[0]),
            }
        }
        let request_line = String::from_utf8_lossy(&head)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\n\r\n",
        );
        let _ = stream.flush();
        let _ = tx.send(request_line);
    });

    // SAFETY: this test binary is single-threaded up to this point and no other test in it reads
    // the environment.
    unsafe {
        std::env::set_var(
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            format!("http://127.0.0.1:{port}"),
        );
    }

    {
        let _guard = crucible_controller::telemetry::init("postgres://localhost/crucible");
        tracing::info_span!("otlp_export_probe").in_scope(|| {
            tracing::info!("probe");
        });
        // Dropping the guard shuts the provider down, which flushes the batch.
    }

    let request_line = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the collector stand-in received no export");
    collector.join().expect("joining the collector thread");

    assert_eq!(
        request_line, "POST /v1/traces HTTP/1.1",
        "the exporter POSTed to the wrong path"
    );
}
