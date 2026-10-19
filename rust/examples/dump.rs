//! Demo / diff tool: read a raw RFC 822 message on stdin, print the parsed
//! metadata and classifier body as JSON.
//!
//! Usage: dump < fixture.eml
//!
//! Useful to diff against the Python implementation when porting further
//! modules (`parse_meta` output must match `aimap.parse.parse_meta`).

use aimap::parse::{body_text, parse_meta};

fn main() {
    let mut raw = Vec::new();
    std::io::Read::read_to_end(&mut std::io::stdin(), &mut raw).expect("read stdin");
    let meta = parse_meta(&raw);
    let out = serde_json::json!({
        "meta": meta,
        "body_text": body_text(&raw, 1500),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}
