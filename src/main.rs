use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::SystemTime,
};

use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

const INDEX_HTML: &str = include_str!("../static/index.html");

fn projects_root() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .expect("USERPROFILE / HOME not set");
    PathBuf::from(home).join(".claude").join("projects")
}

fn list_jsonl(root: &Path) -> Vec<(PathBuf, String, String)> {
    // (path, session_id, project_dir_name)
    let mut out = Vec::new();
    let projs = match fs::read_dir(root) {
        Ok(d) => d,
        Err(_) => return out,
    };
    for p in projs.flatten() {
        if !p.path().is_dir() {
            continue;
        }
        let proj_name = p.file_name().to_string_lossy().into_owned();
        let files = match fs::read_dir(p.path()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for f in files.flatten() {
            let path = f.path();
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                let id = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                out.push((path, id, proj_name.clone()));
            }
        }
    }
    out
}

struct Meta {
    title: String,
    first_msg: String,
    msg_count: u32,
    cwd: String,
}

fn scan_meta(path: &Path) -> Meta {
    let mut m = Meta {
        title: String::new(),
        first_msg: String::new(),
        msg_count: 0,
        cwd: String::new(),
    };
    let f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return m,
    };
    let reader = BufReader::new(f);
    for line in reader.lines().flatten() {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(t) = v.get("type").and_then(|x| x.as_str()) {
            if t == "ai-title" {
                if let Some(s) = v.get("aiTitle").and_then(|x| x.as_str()) {
                    m.title = s.to_string();
                }
            }
        }
        if m.cwd.is_empty() {
            if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                m.cwd = c.to_string();
            }
        }
        if let Some(msg) = v.get("message") {
            m.msg_count += 1;
            if m.first_msg.is_empty() && msg.get("role").and_then(|x| x.as_str()) == Some("user") {
                if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                    m.first_msg = s.chars().take(120).collect();
                }
            }
        }
    }
    m
}

fn decode_project(name: &str) -> String {
    let bytes = name.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b'-' && bytes[2] == b'-' {
        let drive = &name[..1];
        let rest = name[3..].replace('-', "\\");
        format!("{}:\\{}", drive, rest)
    } else {
        name.to_string()
    }
}

fn list_sessions(root: &Path) -> Vec<Value> {
    let files = list_jsonl(root);
    let mut out = Vec::with_capacity(files.len());
    for (path, id, proj_name) in files {
        let fs_meta = path.metadata().ok();
        let modified = fs_meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let size = fs_meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let m = scan_meta(&path);
        let cwd = if m.cwd.is_empty() {
            decode_project(&proj_name)
        } else {
            m.cwd
        };
        out.push(json!({
            "id": id,
            "project": proj_name,
            "cwd": cwd,
            "title": m.title,
            "first_msg": m.first_msg,
            "modified": modified,
            "size": size,
            "msg_count": m.msg_count,
        }));
    }
    out
}

fn parse_session(path: &Path) -> Vec<Value> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(f);
    let mut out = Vec::new();
    for (i, line_res) in reader.lines().enumerate() {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => continue,
        };
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let line1 = (i + 1) as u32;
        if let Some(t) = v.get("type").and_then(|x| x.as_str()) {
            if matches!(
                t,
                "permission-mode"
                    | "file-history-snapshot"
                    | "ai-title"
                    | "last-prompt"
                    | "system"
            ) {
                continue;
            }
        }
        if v.get("attachment").is_some() {
            continue;
        }
        let msg = match v.get("message") {
            Some(m) => m,
            None => continue,
        };
        let role = msg.get("role").and_then(|x| x.as_str()).unwrap_or("");
        let content = match msg.get("content") {
            Some(c) => c,
            None => continue,
        };
        match content {
            Value::String(s) => {
                let kind = if role == "user" {
                    "user_text"
                } else {
                    "assistant_text"
                };
                out.push(json!({"kind": kind, "text": s, "line": line1}));
            }
            Value::Array(arr) => {
                for block in arr {
                    let bt = block.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    match bt {
                        "text" => {
                            let t = block.get("text").and_then(|x| x.as_str()).unwrap_or("");
                            out.push(
                                json!({"kind": "assistant_text", "text": t, "line": line1}),
                            );
                        }
                        "thinking" => {
                            let t = block
                                .get("thinking")
                                .and_then(|x| x.as_str())
                                .unwrap_or("");
                            out.push(json!({"kind": "thinking", "text": t, "line": line1}));
                        }
                        "tool_use" => {
                            out.push(json!({
                                "kind": "tool_use",
                                "name": block.get("name").cloned().unwrap_or(Value::Null),
                                "input": block.get("input").cloned().unwrap_or(Value::Null),
                                "id": block.get("id").cloned().unwrap_or(Value::Null),
                                "line": line1,
                            }));
                        }
                        "tool_result" => {
                            out.push(json!({
                                "kind": "tool_result",
                                "tool_use_id": block.get("tool_use_id").cloned().unwrap_or(Value::Null),
                                "content": block.get("content").cloned().unwrap_or(Value::Null),
                                "line": line1,
                            }));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn search_all(root: &Path, q: &str, limit: usize) -> Vec<Value> {
    let q_lower = q.to_lowercase();
    let files = list_jsonl(root);
    let mut out = Vec::new();
    for (path, id, _) in files {
        let f = match File::open(&path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let reader = BufReader::new(f);
        for (i, line_res) in reader.lines().enumerate() {
            let line = match line_res {
                Ok(l) => l,
                Err(_) => continue,
            };
            if !line.to_lowercase().contains(&q_lower) {
                continue;
            }
            let snippet = extract_snippet(&line, &q_lower);
            out.push(json!({
                "session_id": id,
                "line": i + 1,
                "snippet": snippet,
            }));
            if out.len() >= limit {
                return out;
            }
        }
    }
    out
}

fn extract_snippet(line: &str, q_lower: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(line) {
        if let Some(snip) = find_text_with(&v, q_lower) {
            return snip;
        }
    }
    snip_around(line, q_lower)
}

fn snip_around(s: &str, q_lower: &str) -> String {
    let lower = s.to_lowercase();
    let idx = lower.find(q_lower).unwrap_or(0);
    let start = floor_char_boundary(s, idx.saturating_sub(60));
    let end = ceil_char_boundary(s, (idx + q_lower.len() + 100).min(s.len()));
    let mut out = s[start..end].to_string();
    if start > 0 {
        out.insert(0, '…');
    }
    if end < s.len() {
        out.push('…');
    }
    out
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn find_text_with(v: &Value, q_lower: &str) -> Option<String> {
    match v {
        Value::String(s) => {
            if s.to_lowercase().contains(q_lower) {
                Some(snip_around(s, q_lower))
            } else {
                None
            }
        }
        Value::Array(a) => a.iter().find_map(|x| find_text_with(x, q_lower)),
        Value::Object(o) => o.values().find_map(|x| find_text_with(x, q_lower)),
        _ => None,
    }
}

fn find_session_file(root: &Path, id: &str) -> Option<PathBuf> {
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        return None;
    }
    let target = format!("{}.jsonl", id);
    for entry in fs::read_dir(root).ok()?.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let p = entry.path().join(&target);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn parse_query(q: &str, key: &str) -> Option<String> {
    for part in q.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            if k == key {
                return Some(url_decode(v));
            }
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut buf = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                buf.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let h = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(h, 16) {
                    buf.push(b);
                    i += 3;
                } else {
                    buf.push(b'%');
                    i += 1;
                }
            }
            c => {
                buf.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn respond_json(req: Request, v: &Value) {
    let body = serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string());
    let resp = Response::from_string(body).with_header(
        Header::from_bytes(
            b"Content-Type".as_ref(),
            b"application/json; charset=utf-8".as_ref(),
        )
        .unwrap(),
    );
    let _ = req.respond(resp);
}

fn handle(req: Request, root: &Path) {
    let url = req.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let query = url
        .split_once('?')
        .map(|(_, q)| q.to_string())
        .unwrap_or_default();

    if req.method() != &Method::Get {
        let _ = req.respond(Response::from_string("405").with_status_code(405));
        return;
    }

    if path == "/" || path == "/index.html" {
        let resp = Response::from_string(INDEX_HTML).with_header(
            Header::from_bytes(
                b"Content-Type".as_ref(),
                b"text/html; charset=utf-8".as_ref(),
            )
            .unwrap(),
        );
        let _ = req.respond(resp);
        return;
    }

    if path == "/api/sessions" {
        let sessions = list_sessions(root);
        respond_json(req, &Value::Array(sessions));
        return;
    }

    if let Some(id) = path.strip_prefix("/api/session/") {
        if let Some(file) = find_session_file(root, id) {
            let msgs = parse_session(&file);
            respond_json(req, &json!({"messages": msgs}));
        } else {
            let _ = req.respond(Response::from_string("not found").with_status_code(404));
        }
        return;
    }

    if path == "/api/search" {
        let q = parse_query(&query, "q").unwrap_or_default();
        if q.is_empty() {
            respond_json(req, &Value::Array(Vec::new()));
            return;
        }
        let limit = parse_query(&query, "limit")
            .and_then(|s| s.parse().ok())
            .unwrap_or(300);
        let results = search_all(root, &q, limit);
        respond_json(req, &Value::Array(results));
        return;
    }

    let _ = req.respond(Response::from_string("not found").with_status_code(404));
}

fn main() {
    let root = projects_root();
    if !root.exists() {
        eprintln!("not found: {}", root.display());
        std::process::exit(1);
    }
    let addr = std::env::var("CCLOG_ADDR").unwrap_or_else(|_| "127.0.0.1:7777".to_string());
    let server = Server::http(&addr).expect("bind failed");
    println!("cclog viewer: http://{}", addr);
    println!("scanning   : {}", root.display());
    let root = Arc::new(root);
    for req in server.incoming_requests() {
        let r = Arc::clone(&root);
        thread::spawn(move || handle(req, &r));
    }
}
