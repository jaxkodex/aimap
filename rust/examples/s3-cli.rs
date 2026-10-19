//! S3 inspection CLI — the transport's real-world check.
//!
//!     s3-cli <endpoint-or-aws> <bucket> <prefix> ls
//!     s3-cli ... head <key>
//!     s3-cli ... get <key>            # writes the object to stdout
//!
//! Environment: AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY / AWS_REGION.
//! `endpoint-or-aws` = an endpoint URL, or "-" for real AWS.
//! Read-only on purpose: writes go through `aimap accounts`/ingest only.

use aimap::s3::{HttpS3Client, S3Settings};
use aimap::storage::{S3Config, Store};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (endpoint, bucket, prefix, cmd, arg) = match args.as_slice() {
        [e, b, p, c] => (e.clone(), b.clone(), p.clone(), c.clone(), None),
        [e, b, p, c, a] => (e.clone(), b.clone(), p.clone(), c.clone(), Some(a.clone())),
        _ => {
            eprintln!("usage: s3-cli <endpoint|-> <bucket> <prefix> <ls|get|head> [key]");
            std::process::exit(2);
        }
    };
    let settings = S3Settings {
        endpoint_url: (endpoint != "-").then(|| endpoint.clone()),
        scheme: std::env::var("S3_SCHEME").unwrap_or_else(|_| "https".into()),
        region: std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".into()),
        access_key_id: std::env::var("AWS_ACCESS_KEY_ID").expect("AWS_ACCESS_KEY_ID"),
        secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY").expect("AWS_SECRET_ACCESS_KEY"),
        force_path_style: endpoint != "-",
    };
    let store = Store::new(
        S3Config { bucket: bucket.clone(), prefix, ..Default::default() },
        HttpS3Client::new(bucket, settings),
    );

    match (cmd.as_str(), arg) {
        ("ls", _) => {
            for key in store.list_raw().unwrap() {
                println!("{key}");
            }
        }
        ("head", Some(key)) => {
            let full = store.key(&key);
            match store.head_etag(&full).unwrap() {
                Some(etag) => println!("{etag}"),
                None => std::process::exit(1),
            }
        }
        ("get", Some(key)) => {
            let full = store.key(&key);
            match store.get_raw(&full).unwrap() {
                Some((body, _meta)) => {
                    use std::io::Write;
                    std::io::stdout().write_all(&body).unwrap();
                }
                None => std::process::exit(1),
            }
        }
        _ => {
            eprintln!("usage: s3-cli <endpoint|-> <bucket> <prefix> <ls|get|head> [key]");
            std::process::exit(2);
        }
    }
}
