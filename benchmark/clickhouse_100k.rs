use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Instant;

const TABLE: &str = "tracelake_span_bench";

fn main() {
    let addr =
        std::env::var("CLICKHOUSE_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:8123".to_string());

    post(&addr, &format!("DROP TABLE IF EXISTS {TABLE}"));
    post(
        &addr,
        &format!(
            "CREATE TABLE {TABLE} (
                tenant_id String,
                namespace String,
                trace_id String,
                span_id String,
                start_time_ns UInt64,
                payload String
            ) ENGINE = MergeTree
            ORDER BY (tenant_id, namespace, trace_id, start_time_ns, span_id)"
        ),
    );

    let generate_started = Instant::now();
    let mut rows = String::new();
    for trace_num in 0..100 {
        for span_num in 0..1_000 {
            rows.push_str(&format!(
                "tenantid-1\tdefault\ttrace-{trace_num:03}\tspan-{span_num:04}\t{}\t{}\n",
                trace_num * 1_000 + span_num,
                r#"{"prompt":"hello","text":"world"}"#
            ));
        }
    }
    let generate_elapsed = generate_started.elapsed();

    let write_started = Instant::now();
    post(&addr, &format!("INSERT INTO {TABLE} FORMAT TSV\n{rows}"));
    let write_elapsed = write_started.elapsed();

    let read_started = Instant::now();
    let count = post(
        &addr,
        &format!(
            "SELECT count()
             FROM {TABLE}
             WHERE tenant_id = 'tenantid-1'
               AND namespace = 'default'
               AND trace_id = 'trace-042'"
        ),
    );
    let read_elapsed = read_started.elapsed();

    assert_eq!(count.trim(), "1000");
    println!(
        "ClickHouse 100K check: generate={generate_elapsed:?} write={write_elapsed:?} read_count_1k={read_elapsed:?}"
    );
}

fn post(addr: &str, sql: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("ClickHouse HTTP server should accept TCP");
    let request = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sql}",
        sql.len()
    );

    stream
        .write_all(request.as_bytes())
        .expect("request should write");

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("response should read");

    let (headers, body) = response
        .split_once("\r\n\r\n")
        .expect("response should contain headers");

    assert!(
        headers.starts_with("HTTP/1.1 200"),
        "ClickHouse query failed:\n{}",
        body
    );

    body.to_string()
}
