//! G370 (BACKLOG 66 E, the shape agreed with a hosted product on 2026-10-25). An https remote
//! keeps the bytes in parts.
//!
//! For an https remote, bay's own store (`bay assets store <remote> <verb>`, the plugin protocol)
//! is `<remote>/bay-store/v1/`: a put asks for an upload, sends parts of the size the server
//! names, numbered from 1, and completes it; the server joins and hashes them. A get fetches the
//! object, an exists asks for its head: 200 held, 404 never stored (exit 1), 410 purged (exit 3).
//! Bytes the server already holds are not sent again. The login is the remote's own: the header
//! Git sends it (`http.extraHeader` for its URL), or Git's credential helper.
//!
//! Guards to mutate (each makes a case here fail):
//! - `src/assets/store.rs` `put_https`: send one part however large the file (the server's part
//!   size is broken, and it refuses the part).
//! - `src/assets/store.rs` `https`: read 410 as never stored (a purged object reads as absent).
//! - `src/assets/store.rs` `Login::fill`: drop the `http.extraHeader` line (a clone that logs in
//!   by header is refused 401).

use crate::support;

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::g340_a_large_file_saved_never_enters_git::{sha256_hex, video};
use support::TestBox;

/// What the server holds: objects by hex, purged hexes, uploads (id and parts), and how many
/// parts it was sent.
#[derive(Default)]
struct Held {
    objects: HashMap<String, Vec<u8>>,
    purged: Vec<String>,
    uploads: HashMap<String, (String, Vec<Vec<u8>>)>,
    parts_sent: usize,
    /// The `Authorization` header every request must carry, when set.
    required: Option<String>,
}

const PART_BYTES: usize = 64 * 1024;

/// One request: its method, path and body.
fn read_request(stream: &mut TcpStream) -> Option<(String, String, Option<String>, Vec<u8>)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut words = line.split_whitespace();
    let (method, path) = (words.next()?.to_owned(), words.next()?.to_owned());
    let (mut length, mut expect, mut authorization) = (0usize, false, None);
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim();
        if header.is_empty() {
            break;
        }
        let lower = header.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
        if lower.starts_with("authorization:") {
            authorization = Some(header["authorization:".len()..].trim().to_owned());
        }
        if lower.starts_with("expect:") && lower.contains("100-continue") {
            expect = true;
        }
    }
    if expect {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some((method, path, authorization, body))
}

fn answer(stream: &mut TcpStream, code: u16, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn serve(stream: &mut TcpStream, held: &Mutex<Held>) {
    let Some((method, path, authorization, body)) = read_request(stream) else {
        return;
    };
    let mut held = held.lock().unwrap();
    if held.required.is_some() && held.required != authorization {
        return answer(stream, 401, b"");
    }
    let Some(rest) = path.strip_prefix("/repo.git/bay-store/v1/") else {
        return answer(stream, 404, b"");
    };
    let parts: Vec<&str> = rest.split('/').collect();
    match (method.as_str(), parts.as_slice()) {
        ("GET" | "HEAD", ["objects", "sha256", hex]) => {
            if held.purged.iter().any(|purged| purged == hex) {
                answer(stream, 410, b"");
            } else if let Some(bytes) = held.objects.get(*hex).cloned() {
                if method == "HEAD" {
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            bytes.len()
                        )
                        .as_bytes(),
                    );
                } else {
                    answer(stream, 200, &bytes);
                }
            } else {
                answer(stream, 404, b"");
            }
        }
        ("POST", ["uploads"]) => {
            let asked: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let id = asked["id"].as_str().unwrap_or_default().to_owned();
            if held.objects.contains_key(id.trim_start_matches("sha256:")) {
                return answer(stream, 200, br#"{"stored":true}"#);
            }
            let upload = format!("u{}", held.uploads.len() + 1);
            held.uploads.insert(upload.clone(), (id, Vec::new()));
            let said = serde_json::json!({"upload": upload, "part_bytes": PART_BYTES});
            answer(stream, 200, said.to_string().as_bytes());
        }
        ("PUT", ["uploads", upload, "parts", number]) => {
            held.parts_sent += 1;
            let expected = held
                .uploads
                .get(*upload)
                .map_or(0, |(_, parts)| parts.len() + 1);
            if body.len() > PART_BYTES || number.parse::<usize>().ok() != Some(expected) {
                return answer(stream, 400, b"a part out of order or over the part size");
            }
            if let Some((_, parts)) = held.uploads.get_mut(*upload) {
                parts.push(body);
            }
            answer(stream, 200, b"");
        }
        ("POST", ["uploads", upload, "complete"]) => {
            let Some((id, parts)) = held.uploads.remove(*upload) else {
                return answer(stream, 404, b"");
            };
            let whole: Vec<u8> = parts.concat();
            let hex = sha256_hex(&whole);
            if id != format!("sha256:{hex}") {
                return answer(stream, 400, b"the bytes are not the id");
            }
            held.objects.insert(hex, whole);
            answer(
                stream,
                200,
                serde_json::json!({"id": id}).to_string().as_bytes(),
            );
        }
        _ => answer(stream, 404, b""),
    }
}

/// A server on a free local port; returns its remote URL and what it holds.
fn server() -> (String, Arc<Mutex<Held>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let held = Arc::new(Mutex::new(Held::default()));
    let shared = Arc::clone(&held);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            serve(&mut stream, &shared);
        }
    });
    (format!("http://127.0.0.1:{port}/repo.git"), held)
}

/// `bay assets store <remote> <args>` in `dir`: its exit code and stdout.
fn store(test_box: &TestBox, dir: &Path, remote: &str, args: &[&str]) -> (i32, String) {
    let out = test_box
        .bay(dir)
        .args(["assets", "store", remote])
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
    )
}

#[test]
fn a_put_goes_in_parts_and_comes_back_whole() {
    let test_box = TestBox::new();
    let (remote, held) = server();
    let dir = test_box.path();
    let bytes = video();
    let file = dir.join("clip.mov");
    std::fs::write(&file, &bytes).unwrap();
    let id = format!("sha256:{}", sha256_hex(&bytes));

    let (code, said) = store(&test_box, dir, &remote, &["put", file.to_str().unwrap()]);
    assert_eq!(code, 0, "{said}");
    assert_eq!(said, id);
    let sent = held.lock().unwrap().parts_sent;
    assert_eq!(sent, bytes.len().div_ceil(PART_BYTES), "the parts sent");

    // Bytes already held are not sent again.
    let (code, _) = store(&test_box, dir, &remote, &["put", file.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert_eq!(
        held.lock().unwrap().parts_sent,
        sent,
        "held bytes were sent again"
    );

    assert_eq!(store(&test_box, dir, &remote, &["exists", &id]).0, 0);
    let back = dir.join("back.mov");
    let (code, _) = store(
        &test_box,
        dir,
        &remote,
        &["get", &id, back.to_str().unwrap()],
    );
    assert_eq!(code, 0);
    assert_eq!(std::fs::read(&back).unwrap(), bytes);
}

#[test]
fn never_stored_and_purged_are_told_apart() {
    let test_box = TestBox::new();
    let (remote, held) = server();
    let dir = test_box.path();
    let absent = format!("sha256:{}", "1".repeat(64));
    let purged = format!("sha256:{}", "2".repeat(64));
    held.lock().unwrap().purged.push("2".repeat(64));
    assert_eq!(store(&test_box, dir, &remote, &["exists", &absent]).0, 1);
    assert_eq!(store(&test_box, dir, &remote, &["exists", &purged]).0, 3);
    let back = dir.join("back.bin");
    assert_eq!(
        store(
            &test_box,
            dir,
            &remote,
            &["get", &purged, back.to_str().unwrap()]
        )
        .0,
        3,
        "a purged object is not named purged"
    );
    assert!(!back.exists());
}

#[test]
fn the_remotes_own_header_is_its_login() {
    let test_box = TestBox::new();
    let (remote, held) = server();
    held.lock().unwrap().required = Some("Bearer s3cret".to_owned());
    let dir = test_box.path();
    let id = format!("sha256:{}", "1".repeat(64));
    assert_ne!(
        store(&test_box, dir, &remote, &["exists", &id]).0,
        1,
        "the remote answered with no login"
    );
    std::fs::write(
        &test_box.global_config,
        format!("[http \"{remote}\"]\n\textraHeader = Authorization: Bearer s3cret\n"),
    )
    .unwrap();
    assert_eq!(
        store(&test_box, dir, &remote, &["exists", &id]).0,
        1,
        "the remote's own header was not sent"
    );
}
