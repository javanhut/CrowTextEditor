//! A minimal Language Server Protocol client.
//!
//! No async runtime: the server is a child process, one thread reads its
//! stdout and forwards parsed messages over a channel, and the main loop
//! drains the channel between keystrokes. The editor never blocks on the
//! server — a slow or wedged server just means results arrive later.
//!
//! ponytail: full-text sync on every edit and one hardcoded server
//! (rust-analyzer); incremental sync and a per-language server table when
//! they itch.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, TryRecvError};

use serde_json::{json, Value};

pub struct Diagnostic {
    pub line: usize,
    /// 1 error, 2 warning, 3 info, 4 hint.
    pub severity: u8,
    pub message: String,
}

pub enum Event {
    /// A definition to jump to: path, line, UTF-16 column.
    Definition(PathBuf, usize, usize),
    Hover(String),
    Diagnostics(PathBuf, Vec<Diagnostic>),
    /// Completion candidates as (label, insert text).
    Completions(Vec<(String, String, String)>),
    /// `completionItem/resolve` came back: (label, signature + docs).
    CompletionResolved(String, String),
    Status(String),
}

pub struct Client {
    /// The command line this server was spawned from — its identity, so
    /// documents are routed only to their own language's server.
    command: String,
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next_id: i64,
    pending: HashMap<i64, &'static str>,
    ready: bool,
    /// Messages waiting for the initialize handshake to finish.
    queued: Vec<Value>,
    /// Per file: the LSP document version and the editor revision last synced.
    pub synced: HashMap<PathBuf, (i64, u64)>,
    /// The raw items of the last completion response, by label — what
    /// `completionItem/resolve` needs sent back to fetch the docs.
    completion_items: HashMap<String, Value>,
    dead: bool,
}

impl Client {
    /// Spawn a server from a command line like `"pyright-langserver --stdio"`.
    pub fn spawn(root: &Path, command: &str) -> Option<Client> {
        let mut parts = command.split_whitespace();
        let mut child = Command::new(parts.next()?)
            .args(parts)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;

        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(msg) = read_message(&mut reader) {
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });

        let mut client = Client {
            command: command.to_string(),
            child,
            stdin,
            rx,
            next_id: 0,
            pending: HashMap::new(),
            ready: false,
            queued: Vec::new(),
            synced: HashMap::new(),
            completion_items: HashMap::new(),
            dead: false,
        };
        client.send_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": uri_from_path(root),
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": {},
                        "hover": { "contentFormat": ["plaintext", "markdown"] },
                        "completion": { "completionItem": {
                            "documentationFormat": ["plaintext", "markdown"],
                            "resolveSupport": { "properties": ["documentation", "detail"] }
                        }}
                    }
                }
            }),
            "initialize",
        );
        Some(client)
    }

    // ---- outgoing ----------------------------------------------------------

    fn write(&mut self, msg: &Value) {
        let body = msg.to_string();
        let _ = write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body);
        let _ = self.stdin.flush();
    }

    /// Send now if the handshake is done, otherwise queue.
    fn send(&mut self, msg: Value) {
        if self.ready {
            self.write(&msg);
        } else {
            self.queued.push(msg);
        }
    }

    fn send_request(&mut self, method: &str, params: Value, tag: &'static str) {
        self.next_id += 1;
        self.pending.insert(self.next_id, tag);
        let msg = json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params});
        if tag == "initialize" {
            self.write(&msg);
        } else {
            self.send(msg);
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub fn did_open(&mut self, path: &Path, text: String, revision: u64) {
        self.synced.insert(path.to_path_buf(), (1, revision));
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri_from_path(path),
                "languageId": language_id(path),
                "version": 1,
                "text": text
            }}),
        );
    }

    pub fn did_change(&mut self, path: &Path, text: String, revision: u64) {
        let version = {
            let entry = self.synced.entry(path.to_path_buf()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 = revision;
            entry.0
        };
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri_from_path(path), "version": version},
                "contentChanges": [{"text": text}]
            }),
        );
    }

    /// A request at a cursor position; `tag` is "definition" or "hover".
    pub fn request_position(
        &mut self,
        tag: &'static str,
        method: &str,
        path: &Path,
        line: usize,
        utf16_col: usize,
    ) {
        self.send_request(
            method,
            json!({
                "textDocument": {"uri": uri_from_path(path)},
                "position": {"line": line, "character": utf16_col}
            }),
            tag,
        );
    }

    /// Ask the server to fill in docs for a completion item from the last
    /// response; the answer arrives as `Event::CompletionResolved`.
    pub fn resolve_completion(&mut self, label: &str) {
        if let Some(item) = self.completion_items.get(label).cloned() {
            self.send_request("completionItem/resolve", item, "resolve");
        }
    }

    pub fn shutdown(mut self) {
        let body = json!({"jsonrpc": "2.0", "method": "exit"});
        self.write(&body);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    // ---- incoming ----------------------------------------------------------

    pub fn is_dead(&self) -> bool {
        self.dead
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(msg) => self.handle(msg, &mut events),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !self.dead {
                        self.dead = true;
                        events.push(Event::Status("rust-analyzer exited".into()));
                    }
                    break;
                }
            }
        }
        events
    }

    fn handle(&mut self, msg: Value, events: &mut Vec<Event>) {
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned();

        match (method, id) {
            // A request from the server: answer with an empty default so the
            // server never stalls waiting on us.
            (Some(m), Some(id)) => {
                let result = if m == "workspace/configuration" {
                    // An empty object per item, not null: "no settings, use
                    // your defaults". Taplo treats a null here as a failed
                    // config fetch and excludes every document.
                    let n = msg["params"]["items"].as_array().map_or(0, Vec::len);
                    Value::Array(vec![json!({}); n])
                } else {
                    Value::Null
                };
                self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
            }
            // A notification from the server.
            (Some("textDocument/publishDiagnostics"), None) => {
                let params = &msg["params"];
                if let Some(path) = params["uri"].as_str().and_then(path_from_uri) {
                    let diags = params["diagnostics"]
                        .as_array()
                        .map(|a| a.iter().filter_map(parse_diagnostic).collect())
                        .unwrap_or_default();
                    events.push(Event::Diagnostics(path, diags));
                }
            }
            (Some(_), None) => {} // progress, logs — ignore
            // A response to one of our requests.
            (None, Some(id)) => {
                let Some(tag) = id.as_i64().and_then(|i| self.pending.remove(&i)) else {
                    return;
                };
                match tag {
                    "initialize" => {
                        self.ready = true;
                        self.write(
                            &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
                        );
                        for queued in std::mem::take(&mut self.queued) {
                            self.write(&queued);
                        }
                    }
                    "definition" => match parse_location(&msg["result"]) {
                        Some((path, line, col)) => {
                            events.push(Event::Definition(path, line, col));
                        }
                        None => events.push(Event::Status("no definition found".into())),
                    },
                    "hover" => match hover_text(&msg["result"]) {
                        Some(text) => events.push(Event::Hover(text)),
                        None => events.push(Event::Status("no hover info".into())),
                    },
                    "completion" => {
                        self.completion_items = raw_items(&msg["result"]);
                        events.push(Event::Completions(parse_completions(&msg["result"])))
                    }
                    "resolve" => {
                        let item = &msg["result"];
                        if let Some(label) = item["label"].as_str() {
                            events.push(Event::CompletionResolved(
                                label.trim().to_string(),
                                item_info(item),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            (None, None) => {}
        }
    }
}

fn parse_diagnostic(v: &Value) -> Option<Diagnostic> {
    Some(Diagnostic {
        line: v["range"]["start"]["line"].as_u64()? as usize,
        severity: v["severity"].as_u64().unwrap_or(2) as u8,
        message: v["message"].as_str()?.lines().next()?.to_string(),
    })
}

/// Accepts Location, Location[], and LocationLink[].
fn parse_location(result: &Value) -> Option<(PathBuf, usize, usize)> {
    let loc = if result.is_array() {
        result.get(0)?
    } else {
        result
    };
    let (uri, range) = match loc.get("uri") {
        Some(u) => (u, loc.get("range")?),
        None => (
            loc.get("targetUri")?,
            loc.get("targetSelectionRange")
                .or_else(|| loc.get("targetRange"))?,
        ),
    };
    Some((
        path_from_uri(uri.as_str()?)?,
        range["start"]["line"].as_u64()? as usize,
        range["start"]["character"].as_u64()? as usize,
    ))
}

/// CompletionItem[] or CompletionList; text preference is
/// textEdit.newText > insertText > label, with snippet placeholders stripped.
fn parse_completions(result: &Value) -> Vec<(String, String, String)> {
    let items = result
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| result.as_array());
    let Some(items) = items else {
        return Vec::new();
    };
    items
        .iter()
        .take(100)
        .filter_map(|item| {
            let label = item["label"].as_str()?.trim().to_string();
            let text = item["textEdit"]["newText"]
                .as_str()
                .or_else(|| item["insertText"].as_str())
                .map(strip_snippet)
                .unwrap_or_else(|| label.clone());
            Some((label, text, item_info(item)))
        })
        .collect()
}

/// The raw completion items by label, kept for `completionItem/resolve`.
fn raw_items(result: &Value) -> HashMap<String, Value> {
    let items = result
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| result.as_array());
    items
        .into_iter()
        .flatten()
        .take(100)
        .filter_map(|item| Some((item["label"].as_str()?.trim().to_string(), item.clone())))
        .collect()
}

/// Signature (`detail`) and documentation of a completion item, for the
/// docs side panel. Empty when the server sent neither.
fn item_info(item: &Value) -> String {
    let detail = item["detail"].as_str().unwrap_or("").trim();
    let doc = item["documentation"]
        .as_str()
        .or_else(|| item["documentation"]["value"].as_str())
        .unwrap_or("");
    let doc = strip_fences(doc);
    match (detail.is_empty(), doc.is_empty()) {
        (false, false) => format!("{detail}\n\n{doc}"),
        (false, true) => detail.to_string(),
        (true, _) => doc,
    }
}

/// Drop `$0` / `${1:placeholder}` snippet syntax from an insert text.
fn strip_snippet(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('{') => {
                // ${n:placeholder} — keep the placeholder text, drop the rest.
                let mut inner = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    inner.push(c);
                }
                if let Some((_, placeholder)) = inner.split_once(':') {
                    out.push_str(placeholder);
                }
            }
            Some(c) if c.is_ascii_digit() => {
                while chars.peek().is_some_and(char::is_ascii_digit) {
                    chars.next();
                }
            }
            _ => out.push('$'),
        }
    }
    out
}

/// The full hover text out of the various shapes hover contents can take:
/// signature, description, and examples, with the markdown fences dropped.
fn hover_text(result: &Value) -> Option<String> {
    let contents = result.get("contents")?;
    let raw = if let Some(s) = contents.as_str() {
        s.to_string()
    } else if let Some(v) = contents.get("value").and_then(Value::as_str) {
        v.to_string()
    } else {
        let first = contents.as_array()?.first()?;
        first
            .as_str()
            .map(str::to_string)
            .or_else(|| first.get("value")?.as_str().map(str::to_string))?
    };
    let text = strip_fences(&raw);
    (!text.is_empty()).then_some(text)
}

/// Markdown without the ``` fence lines and horizontal rules; code inside
/// the fences — the examples — stays.
fn strip_fences(markdown: &str) -> String {
    let lines: Vec<&str> = markdown
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim_start().starts_with("```") && l.trim() != "---")
        .collect();
    lines.join("\n").trim().to_string()
}

// ---- wire format -----------------------------------------------------------

/// One `Content-Length`-framed JSON-RPC message, or `None` on EOF.
fn read_message(r: &mut impl BufRead) -> Option<Value> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse().ok();
        }
    }
    let mut buf = vec![0u8; len?];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

/// LSP languageId from a file extension; the extension itself is a decent
/// fallback for anything not listed.
pub fn language_id(path: &Path) -> &str {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "rs" => "rust",
        "py" => "python",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" => "javascript",
        "c" | "h" => "c",
        "cc" | "cpp" | "hpp" => "cpp",
        "rb" => "ruby",
        "kt" => "kotlin",
        "hs" => "haskell",
        "ex" | "exs" => "elixir",
        "md" => "markdown",
        "sh" => "shellscript",
        "oxi" => "oxigen",
        other => other,
    }
}

fn uri_from_path(path: &Path) -> String {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut uri = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                uri.push(b as char)
            }
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let mut it = rest.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
            bytes.push(hex(it.next()?)? * 16 + hex(it.next()?)?);
        } else {
            bytes.push(b);
        }
    }
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frames_parse() {
        let body = r#"{"jsonrpc":"2.0","method":"x"}"#;
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let msg = read_message(&mut Cursor::new(framed)).unwrap();
        assert_eq!(msg["method"], "x");
        assert!(read_message(&mut Cursor::new("")).is_none());
    }

    #[test]
    fn uri_roundtrips_spaces() {
        let path = PathBuf::from("/tmp/a dir/file.rs");
        let uri = uri_from_path(&path);
        assert!(uri.contains("%20"));
        assert_eq!(path_from_uri(&uri).unwrap(), path);
    }

    #[test]
    fn locations_parse_in_all_shapes() {
        let loc = json!({"uri": "file:///a.rs", "range": {"start": {"line": 3, "character": 7}}});
        assert_eq!(
            parse_location(&loc).unwrap(),
            (PathBuf::from("/a.rs"), 3, 7)
        );
        let link = json!([{"targetUri": "file:///b.rs",
            "targetSelectionRange": {"start": {"line": 1, "character": 2}}}]);
        assert_eq!(
            parse_location(&link).unwrap(),
            (PathBuf::from("/b.rs"), 1, 2)
        );
    }

    #[test]
    #[ignore] // needs rust-analyzer on PATH; run with `cargo test -- --ignored`
    fn handshake_with_a_real_server() {
        let Some(mut client) = Client::spawn(Path::new("."), "rust-analyzer") else {
            return; // no server installed — nothing to verify
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while std::time::Instant::now() < deadline && !client.ready {
            let _ = client.poll();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(client.ready, "initialize handshake did not complete");
        client.shutdown();
    }

    #[test]
    #[ignore] // needs rust-analyzer on PATH; run with `cargo test -- --ignored`
    fn definition_resolves_in_a_scratch_file() {
        use std::time::{Duration, Instant};

        // rust-analyzer only indexes real Cargo projects; make a minimal one.
        let dir = std::env::temp_dir().join("crow-lsp-test");
        let _ = std::fs::create_dir_all(dir.join("src"));
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"scratch\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let file = dir.join("src/main.rs");
        let src = "fn foo() {}\nfn main() { foo(); }\n";
        std::fs::write(&file, src).unwrap();

        let Some(mut client) = Client::spawn(&dir, "rust-analyzer") else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !client.ready {
            let _ = client.poll();
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(client.ready);
        client.did_open(&file, src.to_string(), 0);

        // The server may still be indexing; retry until it answers.
        let mut found = None;
        let mut last_ask = Instant::now() - Duration::from_secs(9);
        while Instant::now() < deadline && found.is_none() {
            if last_ask.elapsed() > Duration::from_secs(2) {
                last_ask = Instant::now();
                client.request_position("definition", "textDocument/definition", &file, 1, 13);
            }
            for event in client.poll() {
                if let Event::Definition(path, line, _) = event {
                    found = Some((path, line));
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        client.shutdown();
        let (path, line) = found.expect("no definition arrived");
        assert_eq!(line, 0, "foo() is defined on line 0");
        assert!(path.ends_with("main.rs"));
    }

    #[test]
    fn hover_skips_code_fences() {
        let h = json!({"contents": {"kind": "markdown", "value": "```rust\nfn foo()\n```"}});
        assert_eq!(hover_text(&h).unwrap(), "fn foo()");
    }
}
