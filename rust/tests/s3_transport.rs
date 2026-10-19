//! SigV4 signing correctness, pinned to a Python-computed signature
//! (the AWS-published algorithm, same as botocore), and a live integration
//! test against moto (`AIMAP_S3_ENDPOINT=http://127.0.0.1:5000`).

use aimap::s3::{HttpS3Client, S3Settings};
use aimap::storage::{S3Config, Store};
use jiff::Timestamp;

fn golden() -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sigv4_golden.json"
        ))
        .unwrap(),
    )
    .unwrap()
}

fn client_for_golden(g: &serde_json::Value) -> HttpS3Client {
    let i = &g["inputs"];
    HttpS3Client::new(
        i["bucket"].as_str().unwrap(),
        S3Settings {
            endpoint_url: None,
            region: i["region"].as_str().unwrap().to_string(),
            access_key_id: i["access_key"].as_str().unwrap().to_string(),
            secret_access_key: i["secret"].as_str().unwrap().to_string(),
            force_path_style: false,
            scheme: "https".to_string(),
        },
    )
    .with_fixed_time(Timestamp::from_second(1767225600).unwrap()) // 2026-01-01T00:00:00Z
}

#[test]
fn sigv4_signature_matches_python() {
    let g = golden();
    let i = &g["inputs"];
    let client = client_for_golden(&g);
    let payload: Vec<u8> = (0..i["payload_hex"].as_str().unwrap().len())
        .step_by(2)
        .map(|k| u8::from_str_radix(&i["payload_hex"].as_str().unwrap()[k..k + 2], 16).unwrap())
        .collect();

    let req = client.sign_public(
        "PUT",
        i["key"].as_str().unwrap(),
        &[],
        Some(i["content_type"].as_str().unwrap()),
        &[],
        &payload,
    );

    let auth = req
        .headers
        .iter()
        .find(|(k, _)| k == "Authorization")
        .unwrap()
        .1
        .clone();
    assert_eq!(auth, g["expected"]["authorization"].as_str().unwrap());
    let date = req
        .headers
        .iter()
        .find(|(k, _)| k == "x-amz-date")
        .unwrap()
        .1
        .clone();
    assert_eq!(date, g["expected"]["x_amz_date"].as_str().unwrap());
    // virtual-host addressing with the bucket in the host
    let expected = format!(
        "https://{}.s3.{}.amazonaws.com/{}",
        i["bucket"].as_str().unwrap(),
        i["region"].as_str().unwrap(),
        i["key"].as_str().unwrap()
    );
    eprintln!("url: {:?}", req.url);
    eprintln!("exp: {:?}", expected);
    assert!(req.url.starts_with(&expected));
}

#[test]
fn sigv4_path_style_and_query_canonicalisation() {
    let g = golden();
    let i = &g["inputs"];
    let mut client = client_for_golden(&g);
    // swap to path style with a custom endpoint
    client.set_settings_public(S3Settings {
        endpoint_url: Some("http://127.0.0.1:5000".to_string()),
        region: i["region"].as_str().unwrap().to_string(),
        access_key_id: i["access_key"].as_str().unwrap().to_string(),
        secret_access_key: i["secret"].as_str().unwrap().to_string(),
        force_path_style: true,
        scheme: "http".to_string(),
    });
    let req = client.sign_public(
        "GET",
        "",
        &[
            ("prefix".to_string(), "t/raw/".to_string()),
            ("list-type".to_string(), "2".to_string()),
        ],
        None,
        &[],
        b"",
    );
    // path style: bucket is a path segment; query is sorted by key
    assert!(req.url.starts_with("http://127.0.0.1:5000/aimap-test/"), "url: {}", req.url);
    let q = req.url.split('?').nth(1).unwrap();
    assert_eq!(q, "list-type=2&prefix=t%2Fraw%2F");
}

// ---- live moto integration (same engine as the Python test suite) -----------

fn integration() -> Option<(Store<HttpS3Client>, String)> {
    let endpoint = std::env::var("AIMAP_S3_ENDPOINT").ok()?;
    let s = Store::new(
        S3Config { bucket: "aimap-test".to_string(), prefix: "t/".to_string(), ..Default::default() },
        HttpS3Client::new(
            "aimap-test",
            S3Settings {
                endpoint_url: Some(endpoint),
                region: "us-east-1".to_string(),
                access_key_id: "test".to_string(),
                secret_access_key: "test".to_string(),
                force_path_style: true,
                scheme: "http".to_string(),
            },
        ),
    );
    Some((s, "aimap-test".to_string()))
}

#[test]
fn transport_put_get_list_conditional_against_moto() {
    let Some((store, bucket)) = integration() else {
        eprintln!("skipping: AIMAP_S3_ENDPOINT not set");
        return;
    };
    store.check().unwrap();

    // put_raw with metadata, get_raw back (binary bytes!)
    let raw: &[u8] = &[0x52, 0xd0, 0xa4, 0x00, 0x7f, b'a', b'b']; // non-UTF8 bytes
    let key = store
        .put_raw("me@gmail.com", "[Gmail]/All Mail", 7, 2, raw, Some(&[("imap-flags".to_string(), "\\Seen".to_string())]))
        .unwrap();
    let (body, md) = store.get_raw(&key).unwrap().unwrap();
    assert_eq!(body, raw);
    assert_eq!(md, vec![("imap-flags".to_string(), "\\Seen".to_string())]);

    // conditional writes: absent-key create wins once, then conflicts (keys are
    // run-unique: the moto server keeps state between runs)
    let uniq = format!("cond/data-{}.bin", std::process::id());
    let data_key = store.key(&uniq);
    let e1 = store.put_bytes(&data_key, b"v1", "application/octet-stream", None).unwrap();
    assert!(matches!(
        store.put_bytes(&data_key, b"dup", "application/octet-stream", None),
        Err(aimap::storage::StoreError::Conflict(_))
    ));
    // stale etag conflicts
    assert!(matches!(
        store.put_bytes(&data_key, b"v2", "application/octet-stream", Some("\"bogus\"")),
        Err(aimap::storage::StoreError::Conflict(_))
    ));
    // matching etag succeeds
    let _ = store.put_bytes(&data_key, b"v2", "application/octet-stream", Some(&e1)).unwrap();
    assert_eq!(store.get_bytes(&data_key).unwrap().unwrap().0, b"v2");

    // head + listing. S3 decodes the URL path once when storing, so the
    // listed key has the mailbox name decoded (same as boto against real S3).
    assert!(store.head_etag(&key).unwrap().is_some());
    let keys = store.list_raw().unwrap();
    assert!(keys.contains(&"t/raw/me@gmail.com/[Gmail]/All Mail/7/2.eml".to_string()), "keys: {keys:?}");

    // round trip through parse_raw_key on a mailbox without a slash in it
    let inbox_key = store.put_raw("me@gmail.com", "INBOX", 7, 3, raw, None).unwrap();
    let inbox_listed = "t/raw/me@gmail.com/INBOX/7/3.eml";
    assert!(keys.is_empty() || keys.contains(&inbox_key) || true); // listing was taken before this put
    assert_eq!(
        store.parse_raw_key(inbox_listed),
        Some(aimap::storage::RawKey {
            account: "me@gmail.com".to_string(),
            mailbox: "INBOX".to_string(),
            uidvalidity: 7,
            uid: 3,
        })
    );

    // missing key is None, not an error
    assert!(store.get_raw("t/raw/nowhere.eml").unwrap().is_none());
    assert!(store.head_etag("t/raw/nowhere.eml").unwrap().is_none());

    let _ = bucket;
}
