//! Shared test support: an in-memory S3 fake (the moto role) and a real
//! Postgres test database (the pytest `pg_dsn`/`pool` fixtures).

// Each test binary links this module and uses a different subset.
#![allow(dead_code)]

use aimap::storage::{
    PutCondition, S3Client, S3Error, S3Object,
};
use postgres::{Client, NoTls};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// In-memory S3, mimicking moto's behavior for the subset the Store uses.
/// Keys are per bucket; etags are quoted counters like real S3. Cheap to
/// clone: clones share the bucket, so a test and the Store see the same
/// objects.
#[derive(Clone)]
pub struct MemoryS3 {
    objects: Arc<Mutex<BTreeMap<(String, String), Obj>>>,
    etag_counter: Arc<AtomicU64>,
}

#[derive(Clone)]
struct Obj {
    body: Vec<u8>,
    etag: String,
    content_type: String,
    metadata: Vec<(String, String)>,
}

impl MemoryS3 {
    pub fn new() -> Self {
        MemoryS3 {
            objects: Arc::new(Mutex::new(BTreeMap::new())),
            etag_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn put_plain(&self, bucket: &str, key: &str, body: &[u8]) {
        let etag = self.next_etag();
        self.objects.lock().unwrap().insert(
            (bucket.to_string(), key.to_string()),
            Obj { body: body.to_vec(), etag, content_type: "application/octet-stream".into(), metadata: vec![] },
        );
    }

    /// Remove an object outright (moto's delete_object in the Python tests).
    pub fn delete_object(&self, bucket: &str, key: &str) {
        self.objects.lock().unwrap().remove(&(bucket.to_string(), key.to_string()));
    }

    /// Put an object with user metadata (the tests stand-in for boto3 put_object).
    pub fn put_with_metadata(&self, bucket: &str, key: &str, body: &[u8], metadata: &[(&str, &str)]) {
        let meta: Vec<(String, String)> =
            metadata.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let etag = self.next_etag();
        self.objects.lock().unwrap().insert(
            (bucket.to_string(), key.to_string()),
            Obj { body: body.to_vec(), etag, content_type: "application/octet-stream".into(), metadata: meta },
        );
    }

    fn next_etag(&self) -> String {
        let n = self.etag_counter.fetch_add(1, Ordering::SeqCst);
        format!("\"etag-{n}\"")
    }
}

impl Default for MemoryS3 {
    fn default() -> Self {
        Self::new()
    }
}

impl S3Client for MemoryS3 {
    fn head_object(&self, bucket: &str, key: &str) -> Result<Option<String>, S3Error> {
        match self.objects.lock().unwrap().get(&(bucket.to_string(), key.to_string())) {
            Some(obj) => Ok(Some(obj.etag.clone())),
            None => Err(S3Error::new("404")),
        }
    }

    fn get_object(&self, bucket: &str, key: &str) -> Result<Option<S3Object>, S3Error> {
        match self.objects.lock().unwrap().get(&(bucket.to_string(), key.to_string())) {
            Some(obj) => Ok(Some(S3Object {
                body: obj.body.clone(),
                etag: obj.etag.clone(),
                metadata: obj.metadata.clone(),
            })),
            None => Err(S3Error::new("NoSuchKey")),
        }
    }

    fn put_object(
        &self,
        bucket: &str,
        key: &str,
        body: &[u8],
        content_type: &str,
        metadata: &[(String, String)],
        condition: &PutCondition,
    ) -> Result<String, S3Error> {
        let mut objects = self.objects.lock().unwrap();
        let slot = objects.get(&(bucket.to_string(), key.to_string()));
        match condition {
            PutCondition::IfMatch(etag) => {
                if slot.map(|o| o.etag.as_str()) != Some(etag.as_str()) {
                    return Err(S3Error::new("PreconditionFailed"));
                }
            }
            PutCondition::IfNoneMatchAny => {
                if slot.is_some() {
                    return Err(S3Error::new("PreconditionFailed"));
                }
            }
            PutCondition::None => {}
        }
        let etag = self.next_etag();
        objects.insert(
            (bucket.to_string(), key.to_string()),
            Obj {
                body: body.to_vec(),
                etag: etag.clone(),
                content_type: content_type.to_string(),
                metadata: metadata.to_vec(),
            },
        );
        Ok(etag)
    }

    fn list_objects(&self, bucket: &str, prefix: &str) -> Result<Vec<String>, S3Error> {
        // S3 lists keys in lexicographic order
        Ok(self
            .objects
            .lock()
            .unwrap()
            .keys()
            .filter(|(b, k)| b == bucket && k.starts_with(prefix))
            .map(|(_, k)| k.clone())
            .collect())
    }

    fn head_bucket(&self, bucket: &str) -> Result<(), S3Error> {
        // the fake always has the bucket; a wrong one would 404 like moto
        let _ = bucket;
        Ok(())
    }
}

/// A throwaway Postgres database with all migrations applied.
/// Needs `TEST_DATABASE_URL` pointing at a server we may create databases on
/// (CI provides one; locally point it at your dev instance).
pub struct TestDb {
    pub name: String,
    admin_url: String,
}

impl TestDb {
    pub fn new() -> TestDb {
        let db = TestDb::new_no_migrate();
        let mut conn = db.connect();
        aimap::db::migrate_on(&mut conn).expect("migrate test database");
        db
    }

    /// Fresh database WITHOUT migrations (for testing `migrate` itself).
    pub fn new_no_migrate() -> TestDb {
        let admin_url = std::env::var("TEST_DATABASE_URL").unwrap_or_default();
        if admin_url.is_empty() {
            panic!("TEST_DATABASE_URL is not set; point it at a disposable Postgres (see rust/README.md)");
        }
        let name = format!(
            "aimap_rs_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        {
            let mut admin = Client::connect(&admin_url, NoTls).expect("connect to TEST_DATABASE_URL");
            admin
                .execute(&format!("CREATE DATABASE {name}"), &[])
                .expect("create test database");
        }
        TestDb { name, admin_url }
    }

    pub fn url(&self) -> String {
        // reuse the admin URL's credentials/server, but select the test database
        set_db_name(&self.admin_url, &self.name)
    }

    pub fn connect(&self) -> Client {
        Client::connect(&self.url(), NoTls).expect("connect to test database")
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        if let Ok(mut admin) = Client::connect(&self.admin_url, NoTls) {
            let _ = admin.execute(
                &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name),
                &[],
            );
        }
    }
}

fn set_db_name(url: &str, db: &str) -> String {
    // replace only the path segment after the last slash (host/credentials untouched)
    match url.rfind('/') {
        Some(i) if url[..i].contains("://") => format!("{}/{}", &url[..i], db),
        _ => format!("{}/{}", url, db),
    }
}

// --------------------------------------------------------------------------------------
// in-memory wire: two ends, each with an inbox queue; writes go to the peer's
// inbox (the pair the imap and http wire tests drive)
// --------------------------------------------------------------------------------------

#[derive(Default)]
struct WireQueue {
    bytes: std::collections::VecDeque<u8>,
    eof: bool,
}

type WireShared = std::sync::Arc<(std::sync::Mutex<WireQueue>, std::sync::Condvar)>;

pub struct WireHalf {
    inbox: WireShared,
    peer: WireShared,
}

pub fn wire_pair() -> (WireHalf, WireHalf) {
    fn shared() -> WireShared {
        std::sync::Arc::new((std::sync::Mutex::new(WireQueue::default()), std::sync::Condvar::new()))
    }
    let a = shared();
    let b = shared();
    (
        WireHalf { inbox: a.clone(), peer: b.clone() },
        WireHalf { inbox: b, peer: a },
    )
}

impl WireHalf {
    fn signal_eof(peer: &WireShared) {
        let (lock, cv) = &**peer;
        let mut q = lock.lock().unwrap();
        q.eof = true;
        cv.notify_all();
    }
}

impl Drop for WireHalf {
    fn drop(&mut self) {
        Self::signal_eof(&self.peer);
    }
}

impl std::io::Read for WireHalf {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let (lock, cv) = &*self.inbox;
        let mut q = lock.lock().unwrap();
        loop {
            if !q.bytes.is_empty() {
                let n = buf.len().min(q.bytes.len());
                for slot in buf.iter_mut().take(n) {
                    *slot = q.bytes.pop_front().unwrap();
                }
                return Ok(n);
            }
            if q.eof {
                return Ok(0);
            }
            let (nq, _timeout) = cv
                .wait_timeout(q, std::time::Duration::from_secs(30))
                .unwrap();
            q = nq;
        }
    }
}

impl std::io::Write for WireHalf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (lock, cv) = &*self.peer;
        let mut q = lock.lock().unwrap();
        if q.eof {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "peer went away"));
        }
        q.bytes.extend(buf.iter().copied());
        cv.notify_all();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
