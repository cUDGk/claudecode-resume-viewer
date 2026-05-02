use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use serde_json::{json, Value};

const INDEX_HTML: &str = include_str!("../static/index.html");
const DATA_PLACEHOLDER: &str = "/* __CCLOG_DATA__ */";

fn projects_root() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .expect("USERPROFILE / HOME not set");
    PathBuf::from(home).join(".claude").join("projects")
}

fn output_html_path() -> PathBuf {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .expect("USERPROFILE / HOME not set");
    PathBuf::from(home).join(".claude").join("cclog-view.html")
}

fn list_jsonl(root: &Path) -> Vec<(PathBuf, String, String)> {
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

struct ParsedSession {
    title: String,
    first_msg: String,
    cwd: String,
    messages: Vec<Value>,
    first_ts: String,
    last_ts: String,
    counts: Counts,
}

#[derive(Default)]
struct Counts {
    user_text: u32,
    assistant_text: u32,
    thinking: u32,
    tool_use: u32,
    tool_result: u32,
}

// Per-field byte caps. Without these, tool_result blobs (file contents,
// command output) blow the embedded HTML up to 100MB+.
const MAX_TEXT_BYTES: usize = 32 * 1024;
const MAX_THINKING_BYTES: usize = 16 * 1024;
const MAX_TOOL_INPUT_BYTES: usize = 4 * 1024;
const MAX_TOOL_RESULT_BYTES: usize = 4 * 1024;

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let cut = floor_char_boundary(s, max);
    format!(
        "{}\n... [+{} bytes truncated]",
        &s[..cut],
        s.len() - cut
    )
}

fn truncate_strings_in(v: &mut Value, max: usize) {
    match v {
        Value::String(s) => {
            if s.len() > max {
                *s = truncate_str(s, max);
            }
        }
        Value::Array(a) => {
            for item in a.iter_mut() {
                truncate_strings_in(item, max);
            }
        }
        Value::Object(o) => {
            for (_, val) in o.iter_mut() {
                truncate_strings_in(val, max);
            }
        }
        _ => {}
    }
}

// Parse a transcript with deduplication.
// Claude Code records the same assistant message across multiple JSONL lines as
// new tool_use blocks accumulate, so the same text/tool_use/tool_result blocks
// would otherwise appear repeatedly.
fn parse_full(path: &Path) -> ParsedSession {
    let mut p = ParsedSession {
        title: String::new(),
        first_msg: String::new(),
        cwd: String::new(),
        messages: Vec::new(),
        first_ts: String::new(),
        last_ts: String::new(),
        counts: Counts::default(),
    };
    let f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return p,
    };
    let reader = BufReader::new(f);

    let mut seen_text: HashSet<String> = HashSet::new();
    let mut seen_thinking: HashSet<(String, String)> = HashSet::new();
    let mut seen_tool_use: HashSet<String> = HashSet::new();
    let mut seen_tool_result: HashSet<String> = HashSet::new();

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
            if t == "ai-title" {
                if let Some(s) = v.get("aiTitle").and_then(|x| x.as_str()) {
                    p.title = s.to_string();
                }
            }
        }
        if p.cwd.is_empty() {
            if let Some(c) = v.get("cwd").and_then(|x| x.as_str()) {
                p.cwd = c.to_string();
            }
        }

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

        let timestamp = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if !timestamp.is_empty() {
            if p.first_ts.is_empty() {
                p.first_ts = timestamp.clone();
            }
            p.last_ts = timestamp.clone();
        }

        let msg = match v.get("message") {
            Some(m) => m,
            None => continue,
        };
        let role = msg.get("role").and_then(|x| x.as_str()).unwrap_or("");
        let msg_id = msg
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let content = match msg.get("content") {
            Some(c) => c,
            None => continue,
        };

        match content {
            Value::String(s) => {
                if role == "user" {
                    if p.first_msg.is_empty() {
                        p.first_msg = s.chars().take(120).collect();
                    }
                    let txt = truncate_str(s, MAX_TEXT_BYTES);
                    p.messages.push(json!({
                        "kind": "user_text", "text": txt, "line": line1, "ts": &timestamp,
                    }));
                    p.counts.user_text += 1;
                } else if role == "assistant" {
                    let txt = truncate_str(s, MAX_TEXT_BYTES);
                    p.messages.push(json!({
                        "kind": "assistant_text", "text": txt, "line": line1, "ts": &timestamp,
                    }));
                    p.counts.assistant_text += 1;
                }
            }
            Value::Array(arr) => {
                for block in arr {
                    let bt = block.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    match bt {
                        "text" => {
                            if !msg_id.is_empty() && !seen_text.insert(msg_id.clone()) {
                                continue;
                            }
                            let t = block.get("text").and_then(|x| x.as_str()).unwrap_or("");
                            let txt = truncate_str(t, MAX_TEXT_BYTES);
                            p.messages.push(json!({
                                "kind": "assistant_text", "text": txt, "line": line1, "ts": &timestamp,
                            }));
                            p.counts.assistant_text += 1;
                        }
                        "thinking" => {
                            let t = block
                                .get("thinking")
                                .and_then(|x| x.as_str())
                                .unwrap_or("");
                            let key = (msg_id.clone(), t.chars().take(60).collect::<String>());
                            if !msg_id.is_empty() && !seen_thinking.insert(key) {
                                continue;
                            }
                            let txt = truncate_str(t, MAX_THINKING_BYTES);
                            p.messages.push(json!({
                                "kind": "thinking", "text": txt, "line": line1, "ts": &timestamp,
                            }));
                            p.counts.thinking += 1;
                        }
                        "tool_use" => {
                            let bid = block
                                .get("id")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !bid.is_empty() && !seen_tool_use.insert(bid) {
                                continue;
                            }
                            let mut input =
                                block.get("input").cloned().unwrap_or(Value::Null);
                            truncate_strings_in(&mut input, MAX_TOOL_INPUT_BYTES);
                            p.messages.push(json!({
                                "kind": "tool_use",
                                "name": block.get("name").cloned().unwrap_or(Value::Null),
                                "input": input,
                                "id": block.get("id").cloned().unwrap_or(Value::Null),
                                "line": line1,
                                "ts": &timestamp,
                            }));
                            p.counts.tool_use += 1;
                        }
                        "tool_result" => {
                            let tid = block
                                .get("tool_use_id")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !tid.is_empty() && !seen_tool_result.insert(tid) {
                                continue;
                            }
                            let mut tr_content =
                                block.get("content").cloned().unwrap_or(Value::Null);
                            if let Value::String(s) = &tr_content {
                                tr_content = Value::String(truncate_str(s, MAX_TOOL_RESULT_BYTES));
                            } else {
                                truncate_strings_in(&mut tr_content, MAX_TOOL_RESULT_BYTES);
                            }
                            p.messages.push(json!({
                                "kind": "tool_result",
                                "tool_use_id": block.get("tool_use_id").cloned().unwrap_or(Value::Null),
                                "content": tr_content,
                                "line": line1,
                                "ts": &timestamp,
                            }));
                            p.counts.tool_result += 1;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    if p.first_msg.is_empty() {
        for m in &p.messages {
            if m.get("kind").and_then(|x| x.as_str()) == Some("user_text") {
                if let Some(t) = m.get("text").and_then(|x| x.as_str()) {
                    p.first_msg = t.chars().take(120).collect();
                    break;
                }
            }
        }
    }

    p
}

fn main() {
    let root = projects_root();
    if !root.exists() {
        eprintln!("not found: {}", root.display());
        std::process::exit(1);
    }
    let start = std::time::Instant::now();

    println!("scanning : {}", root.display());
    let files = list_jsonl(&root);
    let total = files.len();
    let mut sessions: Vec<Value> = Vec::with_capacity(total);
    let mut transcripts: HashMap<String, Vec<Value>> = HashMap::with_capacity(total);

    for (idx, (path, id, proj_name)) in files.into_iter().enumerate() {
        let fs_meta = path.metadata().ok();
        let modified = fs_meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let size = fs_meta.as_ref().map(|m| m.len()).unwrap_or(0);

        let parsed = parse_full(&path);

        let cwd = if parsed.cwd.is_empty() {
            decode_project(&proj_name)
        } else {
            parsed.cwd
        };

        sessions.push(json!({
            "id": &id,
            "project": proj_name,
            "cwd": cwd,
            "title": parsed.title,
            "first_msg": parsed.first_msg,
            "modified": modified,
            "size": size,
            "msg_count": parsed.messages.len(),
            "first_ts": parsed.first_ts,
            "last_ts": parsed.last_ts,
            "counts": {
                "user_text": parsed.counts.user_text,
                "assistant_text": parsed.counts.assistant_text,
                "thinking": parsed.counts.thinking,
                "tool_use": parsed.counts.tool_use,
                "tool_result": parsed.counts.tool_result,
            },
        }));
        transcripts.insert(id, parsed.messages);

        if (idx + 1) % 20 == 0 || idx + 1 == total {
            println!("parsed   : {}/{}", idx + 1, total);
        }
    }

    sessions.sort_by(|a, b| {
        b.get("modified")
            .and_then(|x| x.as_i64())
            .unwrap_or(0)
            .cmp(&a.get("modified").and_then(|x| x.as_i64()).unwrap_or(0))
    });

    let payload_obj = json!({
        "sessions": sessions,
        "transcripts": transcripts,
    });
    let payload_json = serde_json::to_string(&payload_obj).expect("serialize failed");

    // Sidecar data file. Inlining 60MB+ into a single <script> turns out to be
    // unreliable across browsers (Brave silently aborts script execution on
    // very large inline scripts under file://). Loading via <script src=...>
    // sidesteps that and keeps the HTML tiny.
    let out_html = output_html_path();
    let out_data = out_html.with_file_name("cclog-data.js");
    if let Some(parent) = out_html.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let data_js = format!("window.CCLOG_DATA = {};\n", payload_json);
    fs::write(&out_data, data_js.as_bytes()).expect("write data failed");

    let html = INDEX_HTML.replacen(DATA_PLACEHOLDER, "", 1);
    fs::write(&out_html, &html).expect("write html failed");

    println!(
        "wrote    : {} ({:.1}MB html, {:.1}MB data, {} sessions, {:.2}s)",
        out_html.display(),
        html.len() as f64 / 1_048_576.0,
        data_js.len() as f64 / 1_048_576.0,
        transcripts.len(),
        start.elapsed().as_secs_f64()
    );

    if std::env::args().any(|a| a == "--no-open") {
        return;
    }

    open_in_browser(&out_html);
}

fn open_in_browser(path: &Path) {
    let p = path.to_string_lossy().into_owned();
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("cmd").args(["/C", "start", "", &p]).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(&p).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = Command::new("xdg-open").arg(&p).spawn();
    }
}
