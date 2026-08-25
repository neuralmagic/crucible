//! Throughput of [`StreamJsonParser`] over a deterministic synthetic `stream-json` corpus.
//!
//! Prints the crucible measure contract as its last stdout line
//! (`{"valid", "score", "note"}`, score = ns per input line, lower is better). The event stream
//! the parser emits is hashed and checked against `EXPECTED_HASH`, so a candidate that drops,
//! reorders, or reshapes events is reported invalid rather than fast. `--print-hash` prints the
//! hash and exits, for re-baking after an intentional event-shape change.

use crucible_harness::StreamJsonParser;
use std::fmt::Write as _;
use std::time::Instant;

const EXPECTED_HASH: u64 = 0x80b1_cfd9_c5a2_dac5;
const MESSAGES: usize = 1_200;
const RUNS: usize = 15;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

const WORDS: &[&str] = &[
    "the",
    "parser",
    "buffers",
    "each",
    "delta",
    "until",
    "a",
    "newline",
    "so",
    "one",
    "event",
    "is",
    "one",
    "line",
    "tokens",
    "accrue",
    "across",
    "messages",
    "tool",
    "input",
    "arrives",
    "in",
    "chunks",
    "cargo",
    "test",
    "passes",
    "fn",
    "main()",
    "{",
    "}",
    "unwrap",
    "Result<()>",
    "\"quoted\"",
    "tab\there",
    "unicode-\u{e9}\u{4e2d}",
    "back\\slash",
];

fn prose(rng: &mut Rng, words: usize) -> String {
    let mut s = String::new();
    for i in 0..words {
        if i > 0 {
            s.push(if rng.chance(12) { '\n' } else { ' ' });
        }
        s.push_str(WORDS[rng.below(WORDS.len())]);
    }
    s
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

fn stream_event(inner: &str) -> String {
    format!(
        r#"{{"type":"stream_event","event":{inner},"session_id":"25d79fe0-9bba-48cc-a66f-f61a816b351c","parent_tool_use_id":null,"uuid":"594ae1ee-a70e-46f4-b962-19d10b646144"}}"#
    )
}

/// Split `s` into random-length chunks on char boundaries, the way the API streams deltas.
fn chunks(rng: &mut Rng, s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let n = 1 + rng.below(24);
        out.push(chars[i..(i + n).min(chars.len())].iter().collect());
        i += n;
    }
    out
}

fn text_block(rng: &mut Rng, lines: &mut Vec<String>, index: usize, kind: &str, key: &str) {
    lines.push(stream_event(&format!(
        r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"{kind}","{key}":""}}}}"#
    )));
    let words = 20 + rng.below(200);
    let body = prose(rng, words);
    for c in chunks(rng, &body) {
        lines.push(stream_event(&format!(
            r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"{kind}_delta","{key}":{}}}}}"#,
            json_str(&c)
        )));
    }
    lines.push(stream_event(&format!(
        r#"{{"type":"content_block_stop","index":{index}}}"#
    )));
}

fn tool_block(rng: &mut Rng, lines: &mut Vec<String>, index: usize, id: &str) -> String {
    let (name, input) = match rng.below(4) {
        0 => (
            "Bash",
            format!(
                r#"{{"command":{},"description":"run it"}}"#,
                json_str(&format!(
                    "cargo test -p crucible-harness -- {}",
                    prose(rng, 6)
                ))
            ),
        ),
        1 => (
            "Read",
            r#"{"file_path":"/work/crucible-harness/src/stream_json.rs","offset":100,"limit":80}"#
                .to_string(),
        ),
        2 => (
            "Edit",
            format!(
                r#"{{"file_path":"/work/src/lib.rs","old_string":{},"new_string":{}}}"#,
                json_str(&prose(rng, 40)),
                json_str(&prose(rng, 60))
            ),
        ),
        _ => (
            "Agent",
            format!(
                r#"{{"description":"explore","prompt":{},"subagent_type":"Explore"}}"#,
                json_str(&prose(rng, 120))
            ),
        ),
    };
    lines.push(stream_event(&format!(
        r#"{{"type":"content_block_start","index":{index},"content_block":{{"type":"tool_use","id":"{id}","name":"{name}","input":{{}}}}}}"#
    )));
    for c in chunks(rng, &input) {
        lines.push(stream_event(&format!(
            r#"{{"type":"content_block_delta","index":{index},"delta":{{"type":"input_json_delta","partial_json":{}}}}}"#,
            json_str(&c)
        )));
    }
    lines.push(stream_event(&format!(
        r#"{{"type":"content_block_stop","index":{index}}}"#
    )));
    name.to_string()
}

fn corpus() -> Vec<String> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut lines = Vec::new();
    lines.push(r#"{"type":"system","subtype":"init","model":"claude-opus-4-8","permissionMode":"bypassPermissions","tools":["Task","Bash","Edit","Read","Write"],"mcp_servers":[],"agents":[]}"#.to_string());
    let mut input = 4_000u64;
    let mut output = 0u64;
    let mut cost = 0.0f64;
    for m in 0..MESSAGES {
        input += 300 + rng.below(3_000) as u64;
        let cache = input / 2;
        lines.push(stream_event(&format!(
            r#"{{"type":"message_start","message":{{"model":"claude-opus-4-8","id":"msg_{m:024}","type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{{"input_tokens":{input},"cache_creation_input_tokens":{},"cache_read_input_tokens":{cache},"output_tokens":1}}}}}}"#,
            cache / 3
        )));
        if rng.chance(8) {
            lines.push(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1782356400,"rateLimitType":"five_hour"}}"#.to_string());
        }
        if rng.chance(3) {
            lines.push(r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":10,"error":"overloaded_error"}"#.to_string());
        }
        if rng.chance(2) {
            lines.push(format!("bare stderr line {m} that is not json"));
        }
        let mut index = 0;
        if rng.chance(60) {
            text_block(&mut rng, &mut lines, index, "thinking", "thinking");
            index += 1;
        }
        if rng.chance(70) {
            text_block(&mut rng, &mut lines, index, "text", "text");
            index += 1;
        }
        let mut tools = Vec::new();
        for _ in 0..rng.below(3) {
            let id = format!("toolu_{m:06}_{index}");
            tool_block(&mut rng, &mut lines, index, &id);
            tools.push(id);
            index += 1;
        }
        // The `assistant` echo duplicates the streamed content; the parser skips it, but it is
        // a big line the decoder still has to reject cheaply.
        lines.push(format!(
            r#"{{"type":"assistant","message":{{"model":"claude-opus-4-8","id":"msg_{m:024}","type":"message","role":"assistant","content":[{{"type":"text","text":{}}}]}},"session_id":"25d79fe0","uuid":"x"}}"#,
            json_str(&prose(&mut rng, 300))
        ));
        output += 20 + rng.below(1_500) as u64;
        lines.push(stream_event(&format!(
            r#"{{"type":"message_delta","delta":{{"stop_reason":"end_turn","stop_sequence":null}},"usage":{{"input_tokens":{input},"output_tokens":{output}}}}}"#
        )));
        lines.push(stream_event(r#"{"type":"message_stop"}"#));
        if !tools.is_empty() {
            let mut content = String::new();
            for (i, id) in tools.iter().enumerate() {
                if i > 0 {
                    content.push(',');
                }
                let words = 100 + rng.below(1_500);
                let _ = write!(
                    content,
                    r#"{{"type":"tool_result","tool_use_id":"{id}","content":{},"is_error":false}}"#,
                    json_str(&prose(&mut rng, words))
                );
            }
            lines.push(format!(
                r#"{{"type":"user","message":{{"role":"user","content":[{content}]}},"session_id":"25d79fe0","uuid":"y"}}"#
            ));
        }
        cost += 0.01 + (rng.below(100) as f64) / 1_000.0;
    }
    lines.push(format!(
        r#"{{"type":"result","subtype":"success","is_error":false,"duration_ms":1827,"num_turns":{MESSAGES},"result":"done","total_cost_usd":{cost:.5},"usage":{{"input_tokens":{input},"output_tokens":{output}}}}}"#
    ));
    lines
}

fn fnv1a(hash: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *hash ^= u64::from(*b);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

fn parse(lines: &[String], tool_io: bool) -> (usize, u64) {
    let mut p = StreamJsonParser::default().with_tool_io(tool_io);
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut n = 0;
    for line in lines {
        for ev in p.push(line) {
            n += 1;
            fnv1a(
                &mut hash,
                serde_json::to_string(&ev).unwrap_or_default().as_bytes(),
            );
        }
    }
    for ev in p.flush() {
        n += 1;
        fnv1a(
            &mut hash,
            serde_json::to_string(&ev).unwrap_or_default().as_bytes(),
        );
    }
    (n, hash)
}

fn main() {
    let print_hash = std::env::args().any(|a| a == "--print-hash");
    let lines = corpus();
    let bytes: usize = lines.iter().map(|l| l.len() + 1).sum();

    let (events, h0) = parse(&lines, false);
    let (events_io, h1) = parse(&lines, true);
    let mut combined = 0xcbf2_9ce4_8422_2325u64;
    fnv1a(&mut combined, &h0.to_le_bytes());
    fnv1a(&mut combined, &h1.to_le_bytes());
    if print_hash {
        println!("{combined:#018x}");
        return;
    }
    if combined != EXPECTED_HASH {
        println!(
            r#"{{"valid":false,"score":0,"note":"event stream changed: hash {combined:#018x} != expected {EXPECTED_HASH:#018x} ({events}+{events_io} events)"}}"#
        );
        return;
    }

    let mut best = f64::MAX;
    for _ in 0..RUNS {
        let t = Instant::now();
        let (a, _) = parse(&lines, false);
        let (b, _) = parse(&lines, true);
        let ns = t.elapsed().as_nanos() as f64;
        std::hint::black_box((a, b));
        best = best.min(ns / (2 * lines.len()) as f64);
    }
    let mib_s = (2 * bytes) as f64 / (best * (2 * lines.len()) as f64) * 1e9 / (1024.0 * 1024.0);
    println!(
        r#"{{"valid":true,"score":{best:.1},"note":"{best:.1} ns/line, {mib_s:.0} MiB/s, {} lines, {events}+{events_io} events, min of {RUNS}"}}"#,
        lines.len()
    );
}
