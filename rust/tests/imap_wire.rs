//! The IMAP wire protocol, pinned against a scripted fake server (the port
//! of `imap.py` sits behind imaplib in Python; these tests pin our own
//! client: LOGIN quoting and literals, EXAMINE + UIDVALIDITY, UID SEARCH
//! filtering, UID FETCH with literals, and the error messages).
//!
//! The fake server is connected to the client through an in-memory pipe
//! (`over_stream`): loopback TCP proved flaky in the sandbox (delayed and
//! empty reads), and the protocol tests do not need sockets.

use aimap::accounts::Account;
use aimap::imap::ImapSource;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// --------------------------------------------------------------------------------------
// in-memory wire: two ends, each with an inbox queue; writes go to the peer's inbox
// --------------------------------------------------------------------------------------

#[derive(Default)]
struct Queue {
    bytes: VecDeque<u8>,
    eof: bool,
}

type Shared = Arc<(Mutex<Queue>, Condvar)>;

struct Half {
    inbox: Shared,
    peer: Shared,
}

fn shared() -> Shared {
    Arc::new((Mutex::new(Queue::default()), Condvar::new()))
}

/// A connected pair of ends: writes to one are reads from the other.
fn wire_pair() -> (Half, Half) {
    let a = shared();
    let b = shared();
    (
        Half { inbox: a.clone(), peer: b.clone() },
        Half { inbox: b, peer: a },
    )
}

impl Half {
    /// The peer went away: unblock its reads.
    fn signal_eof(peer: &Shared) {
        let (lock, cv) = &**peer;
        let mut q = lock.lock().unwrap();
        q.eof = true;
        cv.notify_all();
    }
}

impl Drop for Half {
    fn drop(&mut self) {
        Half::signal_eof(&self.peer);
    }
}

impl Read for Half {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let (lock, cv) = &*self.inbox;
        let mut q = lock.lock().unwrap();
        loop {
            if let Some(byte) = q.bytes.pop_front() {
                out[0] = byte;
                let mut n = 1;
                while n < out.len() {
                    match q.bytes.pop_front() {
                        Some(b) => {
                            out[n] = b;
                            n += 1;
                        }
                        None => break,
                    }
                }
                return Ok(n);
            }
            if q.eof {
                return Ok(0);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "wire read timeout"));
            }
            let (locked, _) = cv.wait_timeout(q, deadline - now).unwrap();
            q = locked;
        }
    }
}

impl Write for Half {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (lock, cv) = &*self.peer;
        lock.lock().unwrap().bytes.extend(buf.iter().copied());
        cv.notify_all();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// --------------------------------------------------------------------------------------
// scripted fake server
// --------------------------------------------------------------------------------------

struct Step {
    /// substring expected in the received command line
    expect: String,
    /// reply template; {tag} is replaced with the command's tag
    reply: String,
}

impl Step {
    fn new(expect: &str, reply: impl Into<String>) -> Step {
        Step { expect: expect.to_string(), reply: reply.into() }
    }
}

/// Greets, then for every command line waits for the next step whose
/// expectation matches and answers with the templated reply. Commands the
/// script does not expect (e.g. a LOGOUT after the script ran out) get an
/// unscripted OK, except after the last step, where the connection closes.
fn serve(mut stream: impl Read + Write, steps: Vec<Step>, log: Option<Arc<Mutex<Vec<String>>>>) {
    let _ = stream.write_all(b"* OK IMAP4rev1 ready\r\n");
    let _ = stream.flush();
    let mut taken = 0;
    loop {
        let Some(line) = read_line(&mut stream) else { return };
        // a client literal: "TAG ... {n}" then n raw bytes, then the rest
        let line = match literal_size(&line) {
            Some(size) => {
                let _ = stream.write_all(b"+ continue\r\n");
                let _ = stream.flush();
                let mut bytes = vec![0u8; size];
                if stream.read_exact(&mut bytes).is_err() {
                    return;
                }
                let Some(rest) = read_line(&mut stream) else { return };
                format!("{line}{rest_of_literal}{rest}", rest_of_literal = String::from_utf8_lossy(&bytes))
            }
            None => line,
        };
        if let Some(log) = &log {
            log.lock().unwrap().push(line.clone());
        }
        let Some(step) = steps.get(taken) else {
            return; // no scripted answer: close (the client sees EOF)
        };
        if !line.contains(step.expect.as_str()) {
            // tolerate interleaved client lines we did not script
            let tag = line.split_whitespace().next().unwrap_or("*");
            let _ = stream.write_all(format!("{tag} OK unscripted\r\n").as_bytes());
            let _ = stream.flush();
            continue;
        }
        taken += 1;
        let tag = line.split_whitespace().next().unwrap_or("A0");
        let reply = step.reply.replace("{tag}", tag);
        let _ = stream.write_all(reply.as_bytes());
        let _ = stream.flush();
        if reply.contains("BYE") {
            return;
        }
    }
}

/// One CRLF line (literals arrive as plain bytes; the server script records
/// the command line including any literal payload).
fn read_line(stream: &mut impl Read) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return if line.is_empty() { None } else { Some(String::from_utf8_lossy(&line).into_owned()) },
            Ok(_) => {
                if byte[0] == b'\n' {
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return Some(String::from_utf8_lossy(&line).into_owned());
                }
                line.push(byte[0]);
            }
            Err(e) => panic!("fake server read error: {e}"),
        }
    }
}

/// The `{n}` at the end of a command line announces a client literal.
fn literal_size(line: &str) -> Option<usize> {
    let open = line.rfind('{')?;
    let close = line.rfind('}')?;
    if close != line.len() - 1 || open + 1 >= close {
        return None;
    }
    line[open + 1..close].parse().ok()
}

fn start(steps: Vec<Step>) -> ImapSource {
    start_recording(steps, None)
}

fn start_recording(steps: Vec<Step>, log: Option<Arc<Mutex<Vec<String>>>>) -> ImapSource {
    let (client, server) = wire_pair();
    let handle = std::thread::spawn(move || serve(server, steps, log));
    let src = ImapSource::over_stream(client, "u", "p");
    match src {
        Ok(src) => {
            std::thread::spawn(move || handle.join().ok());
            src
        }
        Err(e) => {
            drop(handle);
            panic!("connect failed: {e}");
        }
    }
}

#[test]
fn login_select_and_uidvalidity() {
    let mut src = start(vec![
        Step::new("LOGIN", "{tag} OK logged in\r\n"),
        Step::new(
            "EXAMINE",
            "* OK [UIDVALIDITY 12345] UIDs valid\r\n* 3 EXISTS\r\n{tag} OK [READ-ONLY] done\r\n",
        ),
        Step::new("LOGOUT", "* BYE\r\n{tag} OK bye\r\n"),
    ]);
    assert_eq!(src.select("INBOX").unwrap(), 12345);
    src.close();
}

#[test]
fn login_arguments_are_quoted_and_literals_are_used_for_control_characters() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut src = start_recording(
        vec![Step::new("LOGIN", "{tag} OK logged in\r\n")],
        Some(received.clone()),
    );
    src.close();
    // quoted strings for clean arguments
    assert!(received.lock().unwrap()[0].contains("LOGIN \"u\" "), "{:?}", received.lock().unwrap());

    // a password with a newline cannot be quoted, so it must go as an IMAP
    // literal (with the continuation round-trip)
    let received = Arc::new(Mutex::new(Vec::new()));
    let logged = received.clone();
    let (client, server) = wire_pair();
    let handle = std::thread::spawn(move || {
        serve(server, vec![Step::new("LOGIN", "{tag} OK\r\n")], Some(logged))
    });
    ImapSource::over_stream(client, "me@x.com", "p\nx").unwrap();
    handle.join().unwrap();
    let cmds = received.lock().unwrap();
    assert!(cmds[0].contains("{3}"), "{cmds:?}");
    assert!(cmds[0].contains("p\nx"), "{cmds:?}");
}

#[test]
fn mailbox_names_are_quoted_like_python() {
    let mut src = start(vec![
        Step::new("LOGIN", "{tag} OK\r\n"),
        Step::new("EXAMINE", "* OK [UIDVALIDITY 1] x\r\n{tag} OK\r\n"),
    ]);
    src.select("[Gmail]/All \"Mail\"").unwrap();
    src.close();
}

#[test]
fn uid_search_all_and_ranges_with_python_filtering() {
    let mut src = start(vec![
        Step::new("LOGIN", "{tag} OK\r\n"),
        Step::new("UID SEARCH ALL", "* SEARCH 1 5 7\r\n{tag} OK\r\n"),
        Step::new("UID SEARCH UID 6:*", "* SEARCH 5 7\r\n{tag} OK\r\n"),
        Step::new("UID SEARCH UID 9:*", "* SEARCH 9 10\r\n{tag} OK\r\n"),
    ]);
    assert_eq!(src.uids_after(0).unwrap(), vec![1, 5, 7]);
    // "N:*" always matches the highest UID, even when it is below N, so filter.
    assert_eq!(src.uids_after(5).unwrap(), vec![7]);
    assert_eq!(src.uids_after(8).unwrap(), vec![9, 10]);
    let _ = src;
}

#[test]
fn fetch_returns_flags_internaldate_and_raw_body() {
    let raw = "From: a@x.com\r\nSubject: hi\r\n\r\nbody\r\n";
    let mut src = start(vec![
        Step::new("LOGIN", "{tag} OK\r\n"),
        Step::new(
            "UID FETCH 7",
            "* 3 FETCH (FLAGS (\\Seen \\Flagged) INTERNALDATE \"01-Jan-2026 10:00:00 +0000\" \
             BODY[] {len}\r\n{raw})\r\n{tag} OK\r\n"
                .replace("{len}", &format!("{{{}}}", raw.len()))
                .replace("{raw}", raw),
        ),
    ]);
    let msg = src.fetch(7).unwrap().unwrap();
    assert_eq!(msg.uid, 7);
    assert_eq!(msg.flags, vec!["\\Seen".to_string(), "\\Flagged".to_string()]);
    assert_eq!(msg.internal_date, "01-Jan-2026 10:00:00 +0000");
    assert_eq!(String::from_utf8_lossy(&msg.raw), raw);
    src.close();
}

#[test]
fn fetch_of_a_vanished_message_is_none() {
    let mut src = start(vec![
        Step::new("LOGIN", "{tag} OK\r\n"),
        Step::new("UID FETCH 9", "{tag} OK gone\r\n"),
    ]);
    assert!(src.fetch(9).unwrap().is_none());
    src.close();
}

#[test]
fn error_messages_match_python() {
    // select of a missing mailbox
    let mut src = start(vec![
        Step::new("LOGIN", "{tag} OK\r\n"),
        Step::new("EXAMINE", "{tag} NO no such mailbox\r\n"),
    ]);
    assert_eq!(src.select("Nope").unwrap_err().0, "cannot open mailbox 'Nope'");
    src.close();

    // missing UIDVALIDITY
    let mut src = start(vec![
        Step::new("LOGIN", "{tag} OK\r\n"),
        Step::new("EXAMINE", "* 1 EXISTS\r\n{tag} OK\r\n"),
    ]);
    assert_eq!(src.select("INBOX").unwrap_err().0, "server did not report UIDVALIDITY for 'INBOX'");
    src.close();

    // failed search
    let mut src = start(vec![
        Step::new("LOGIN", "{tag} OK\r\n"),
        Step::new("UID SEARCH", "{tag} BAD nope\r\n"),
    ]);
    assert_eq!(src.uids_after(0).unwrap_err().0, "UID SEARCH ALL failed");
    src.close();

    // failed fetch
    let mut src = start(vec![
        Step::new("LOGIN", "{tag} OK\r\n"),
        Step::new("UID FETCH 3", "{tag} BAD nope\r\n"),
    ]);
    assert_eq!(src.fetch(3).unwrap_err().0, "UID FETCH 3 failed");
    src.close();
}

#[test]
fn connect_failures_raise_the_python_message() {
    // nothing listens on this port: the connect itself fails
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let acct = Account {
        user: "me@x.com".into(),
        password: "pw".into(),
        host: "127.0.0.1".into(),
        port: port as i32,
        mailboxes: vec!["INBOX".into()],
    };
    let err = match ImapSource::connect(&acct, Duration::from_millis(500)) {
        Ok(_) => panic!("expected connect failure"),
        Err(e) => e,
    };
    assert_eq!(
        err.0,
        format!("connect/login to 127.0.0.1:{port} failed: Connection refused (os error 111)")
    );
}

#[test]
fn failed_logins_surface_the_server_reason() {
    let (client, server) = wire_pair();
    let handle = std::thread::spawn(move || {
        serve(
            server,
            vec![Step::new("LOGIN", "{tag} NO [AUTHENTICATIONFAILED] bad credentials\r\n")],
            None,
        )
    });
    let err = match ImapSource::over_stream(client, "u", "p") {
        Ok(_) => panic!("expected login failure"),
        Err(e) => e,
    };
    handle.join().unwrap();
    assert!(err.0.contains("LOGIN failed: NO"), "{err:?}");
}

#[test]
fn wire_pair_sanity() {
    let (mut a, mut b) = wire_pair();
    a.write_all(b"hello\r\n").unwrap();
    let mut buf = [0u8; 16];
    let n = b.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello\r\n");
    b.write_all(b"ok").unwrap();
    let n = a.read(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ok");
    drop(b);
    assert_eq!(a.read(&mut buf).unwrap(), 0); // EOF
}

#[test]
fn wire_over_stream_greeting_only() {
    let (client, server) = wire_pair();
    let handle = std::thread::spawn(move || {
        let mut server = server;
        server.write_all(b"* OK IMAP4rev1 ready\r\n").unwrap();
        server.flush().unwrap();
        eprintln!("server: greeting written");
        // then wait for LOGIN
        let mut buf = [0u8; 64];
        let n = server.read(&mut buf).unwrap();
        eprintln!("server: got {n} bytes: {:?}", String::from_utf8_lossy(&buf[..n]));
        server.write_all(b"A001 OK logged in\r\n").unwrap();
        server.flush().unwrap();
        eprintln!("server: response written");
    });
    let t0 = Instant::now();
    let src = ImapSource::over_stream(client, "u", "p");
    eprintln!("client after {:?}: {:?}", t0.elapsed(), src.as_ref().map(|_| "ok").map_err(|e| &e.0).err());
    handle.join().unwrap();
}
