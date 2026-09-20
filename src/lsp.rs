//! A minimal Language Server Protocol client.
//!
//! No async runtime: the server is a child process, one thread reads its
//! stdout and forwards parsed messages over a channel, and the main loop
//! drains the channel between keystrokes. The editor never blocks on the
//! server — a slow or wedged server just means results arrive later.
//!
//! Edits go over incrementally when the server supports it (almost all do):
//! `Document` logs every transaction as LSP change events, so a keystroke in
//! a big file costs a few bytes on the pipe instead of the whole buffer.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

use serde_json::{json, Value};

pub struct Diagnostic {
    pub line: usize,
    /// Start column, in UTF-16 code units (the LSP's metric).
    pub col: usize,
    /// 1 error, 2 warning, 3 info, 4 hint.
    pub severity: u8,
    pub message: String,
    /// The diagnostic as the server sent it: code actions want it back.
    pub raw: Value,
}

impl Diagnostic {
    /// What the server attached beyond the headline: the labels on other
    /// spans and, from a compiler, its `help:` suggestions.
    pub fn notes(&self) -> impl Iterator<Item = &str> {
        self.raw["relatedInformation"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r["message"].as_str())
            // rust-analyzer's back-pointer from a help to the error it is for.
            .filter(|m| *m != "original diagnostic")
    }

    /// rust-analyzer publishes each compiler `help:` a second time, as a hint
    /// of its own pointing back at the error it belongs to.
    pub fn is_echo(&self) -> bool {
        self.raw["relatedInformation"]
            .as_array()
            .is_some_and(|r| r.iter().any(|r| r["message"] == "original diagnostic"))
    }

    /// How much a line wants this one shown, least first: the worst
    /// severity, and among equals the one that comes with a suggestion.
    pub fn rank(&self) -> (u8, bool) {
        (self.severity, self.notes().next().is_none())
    }

    /// Everything there is to read. rust-analyzer passes the compiler's own
    /// rendering along — the snippet, the carets, the suggested rewrite —
    /// and nothing we could assemble says it better; elsewhere, the whole
    /// message and its notes.
    pub fn detail(&self) -> String {
        if let Some(rendered) = self.raw["data"]["rendered"].as_str() {
            return rendered.trim_end().to_string();
        }
        let mut out = self.raw["message"]
            .as_str()
            .unwrap_or(&self.message)
            .trim_end()
            .to_string();
        for note in self.notes() {
            out.push_str("\n➜ ");
            out.push_str(note);
        }
        out
    }
}

/// A place in a file: path, line, UTF-16 column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
}

/// One entry of a document or workspace symbol list.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: &'static str,
    /// Nesting depth in a document outline, 0 for top level.
    pub depth: usize,
    /// The enclosing symbol's name, where the server said.
    pub container: String,
    pub location: Location,
}

pub enum Event {
    /// A definition to jump to: path, line, UTF-16 column.
    Definition(PathBuf, usize, usize),
    Hover(String),
    References(Vec<Location>),
    /// A WorkspaceEdit to apply — from a rename, a code action, or the
    /// server's own `workspace/applyEdit` request.
    ApplyEdit(Value),
    /// Code actions (or bare Commands) available at the requested range.
    CodeActions(Vec<Value>),
    /// A resolved code action, ready to apply and run.
    CodeActionResolved(Value),
    /// The signature being typed, with the active parameter's char range in
    /// it; `None` when the server has nothing to say.
    Signature(Option<(String, Option<(usize, usize)>)>),
    /// A symbol list; true for workspace-wide results.
    Symbols(Vec<Symbol>, bool),
    /// TextEdits formatting the document the request was made for.
    Formatting(Vec<Value>),
    Diagnostics(PathBuf, Vec<Diagnostic>),
    /// Completion candidates as (label, insert text, docs), and whether they
    /// answer the ambient request made while an identifier was being typed
    /// rather than one the user asked for.
    Completions(Vec<(String, String, String)>, bool),
    /// `completionItem/resolve` came back: (label, signature + docs).
    CompletionResolved(String, String),
    Status(String),
}

pub struct Client {
    /// The command line this server was spawned from — its identity, so
    /// documents are routed only to their own language's server.
    command: String,
    child: Child,
    /// Outgoing messages, handed to a writer thread. A pipe holds ~64 KB, and
    /// `didChange` sends the whole buffer — writing from the main loop meant a
    /// server that stopped reading for a moment froze the editor with it.
    tx_out: Sender<String>,
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
    /// Characters the server said should pop the completion menu, from its
    /// initialize result: `.` almost everywhere, `<` for Oxigen's `x <int>`.
    triggers: Vec<char>,
    /// Characters that should ask for signature help: `(` and `,` usually.
    signature_triggers: Vec<char>,
    /// `textDocumentSync.change`: 1 full text, 2 incremental.
    sync_kind: u64,
    /// The server can format documents itself.
    formatting: bool,
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
        let mut stdin = child.stdin.take()?;
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

        let (tx_out, rx_out) = channel::<String>();
        std::thread::spawn(move || {
            while let Ok(body) = rx_out.recv() {
                if write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).is_err()
                    || stdin.flush().is_err()
                {
                    break; // server gone; `poll` notices via the reader thread
                }
            }
        });

        let mut client = Client {
            command: command.to_string(),
            child,
            tx_out,
            rx,
            next_id: 0,
            pending: HashMap::new(),
            ready: false,
            queued: Vec::new(),
            synced: HashMap::new(),
            completion_items: HashMap::new(),
            triggers: Vec::new(),
            signature_triggers: Vec::new(),
            sync_kind: 1,
            formatting: false,
            dead: false,
        };
        client.send_request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": uri_from_path(root),
                "capabilities": {
                    "workspace": {
                        "applyEdit": true,
                        "workspaceEdit": { "documentChanges": true },
                        "symbol": {},
                        "executeCommand": {}
                    },
                    "textDocument": {
                        "synchronization": { "didSave": true },
                        "publishDiagnostics": {},
                        "hover": { "contentFormat": ["plaintext", "markdown"] },
                        "completion": { "completionItem": {
                            "documentationFormat": ["plaintext", "markdown"],
                            "resolveSupport": { "properties": ["documentation", "detail"] }
                        }},
                        "references": {},
                        "rename": {},
                        "formatting": {},
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                        "signatureHelp": { "signatureInformation": {
                            "parameterInformation": { "labelOffsetSupport": true }
                        }},
                        "codeAction": {
                            "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [
                                "", "quickfix", "refactor", "refactor.extract",
                                "refactor.inline", "refactor.rewrite", "source",
                                "source.organizeImports"
                            ]}},
                            "resolveSupport": { "properties": ["edit"] }
                        }
                    }
                }
            }),
            "initialize",
        );
        Some(client)
    }

    // ---- outgoing ----------------------------------------------------------

    fn write(&mut self, msg: &Value) {
        let _ = self.tx_out.send(msg.to_string());
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

    /// Only the changed ranges, when the server takes incremental sync.
    pub fn did_change_incremental(
        &mut self,
        path: &Path,
        changes: Vec<crate::document::LspChange>,
        revision: u64,
    ) {
        let version = {
            let entry = self.synced.entry(path.to_path_buf()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 = revision;
            entry.0
        };
        let events: Vec<Value> = changes
            .into_iter()
            .map(|c| {
                json!({
                    "range": {
                        "start": {"line": c.start.0, "character": c.start.1},
                        "end": {"line": c.end.0, "character": c.end.1}
                    },
                    "text": c.text
                })
            })
            .collect();
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri_from_path(path), "version": version},
                "contentChanges": events
            }),
        );
    }

    /// Whether `did_change_incremental` is understood here.
    pub fn incremental(&self) -> bool {
        self.sync_kind == 2
    }

    pub fn did_save(&mut self, path: &Path) {
        if self.synced.contains_key(path) {
            self.notify(
                "textDocument/didSave",
                json!({"textDocument": {"uri": uri_from_path(path)}}),
            );
        }
    }

    pub fn did_close(&mut self, path: &Path) {
        if self.synced.remove(path).is_some() {
            self.notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": uri_from_path(path)}}),
            );
        }
    }

    /// Any request; the answer comes back as an `Event` chosen by `tag`.
    pub fn request(&mut self, method: &str, params: Value, tag: &'static str) {
        self.send_request(method, params, tag);
    }

    /// Whether typing `c` should ask for signature help.
    pub fn triggers_signature(&self, c: char) -> bool {
        self.signature_triggers.contains(&c)
    }

    pub fn can_format(&self) -> bool {
        self.formatting
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
        // Close what we opened and say goodbye properly; servers that keep
        // caches (rust-analyzer's, say) flush them on `shutdown`.
        for path in self.synced.keys().cloned().collect::<Vec<_>>() {
            self.did_close(&path);
        }
        if self.ready {
            self.send_request("shutdown", Value::Null, "shutdown");
        }
        let body = json!({"jsonrpc": "2.0", "method": "exit"});
        self.write(&body);
        // A moment for the writer thread to deliver the goodbyes before the
        // kill below makes them moot.
        std::thread::sleep(std::time::Duration::from_millis(20));
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

    /// Whether typing `c` should ask this server for completions, per the
    /// trigger characters it advertised.
    pub fn triggers_completion(&self, c: char) -> bool {
        self.triggers.contains(&c)
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
                        let name = self.command.split_whitespace().next().unwrap_or("server");
                        events.push(Event::Status(format!("{name} exited")));
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
                if m == "workspace/applyEdit" {
                    events.push(Event::ApplyEdit(msg["params"]["edit"].clone()));
                    self.write(&json!({"jsonrpc": "2.0", "id": id, "result": {"applied": true}}));
                    return;
                }
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
                if let Some(err) = msg.get("error") {
                    // Ambient requests fail quietly; ones the user asked for say why.
                    if !matches!(
                        tag,
                        "completion_typed" | "resolve" | "signature" | "shutdown"
                    ) {
                        let text = err["message"].as_str().unwrap_or("request failed");
                        events.push(Event::Status(format!("{tag}: {text}")));
                    }
                    return;
                }
                let result = &msg["result"];
                match tag {
                    "initialize" => {
                        self.ready = true;
                        let caps = &result["capabilities"];
                        let chars = |v: &Value| -> Vec<char> {
                            v.as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|c| c.as_str()?.chars().next())
                                        .collect()
                                })
                                .unwrap_or_default()
                        };
                        self.triggers = chars(&caps["completionProvider"]["triggerCharacters"]);
                        self.signature_triggers =
                            chars(&caps["signatureHelpProvider"]["triggerCharacters"]);
                        self.signature_triggers
                            .extend(chars(&caps["signatureHelpProvider"]["retriggerCharacters"]));
                        self.sync_kind = match &caps["textDocumentSync"] {
                            Value::Number(n) => n.as_u64().unwrap_or(1),
                            v => v["change"].as_u64().unwrap_or(1),
                        };
                        self.formatting = match &caps["documentFormattingProvider"] {
                            Value::Bool(b) => *b,
                            Value::Object(_) => true,
                            _ => false,
                        };
                        self.write(
                            &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
                        );
                        for queued in std::mem::take(&mut self.queued) {
                            self.write(&queued);
                        }
                    }
                    "definition" => match parse_location(result) {
                        Some((path, line, col)) => {
                            events.push(Event::Definition(path, line, col));
                        }
                        None => events.push(Event::Status("no definition found".into())),
                    },
                    "hover" => match hover_text(result) {
                        Some(text) => events.push(Event::Hover(text)),
                        None => events.push(Event::Status("no hover info".into())),
                    },
                    "completion" | "completion_typed" => {
                        self.completion_items = raw_items(result);
                        events.push(Event::Completions(
                            parse_completions(result),
                            tag == "completion_typed",
                        ))
                    }
                    "resolve" => {
                        let item = result;
                        if let Some(label) = item["label"].as_str() {
                            events.push(Event::CompletionResolved(
                                label.trim().to_string(),
                                item_info(item),
                            ));
                        }
                    }
                    "references" => events.push(Event::References(parse_locations(result))),
                    "rename" => {
                        if result.is_null() {
                            events.push(Event::Status("nothing to rename here".into()));
                        } else {
                            events.push(Event::ApplyEdit(result.clone()));
                        }
                    }
                    "code_action" => events.push(Event::CodeActions(
                        result.as_array().cloned().unwrap_or_default(),
                    )),
                    "code_action_resolve" => events.push(Event::CodeActionResolved(result.clone())),
                    "signature" => events.push(Event::Signature(parse_signature(result))),
                    "symbols" => events.push(Event::Symbols(parse_symbols(result), false)),
                    "workspace_symbols" => events.push(Event::Symbols(parse_symbols(result), true)),
                    "formatting" => events.push(Event::Formatting(
                        result.as_array().cloned().unwrap_or_default(),
                    )),
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
        col: v["range"]["start"]["character"].as_u64().unwrap_or(0) as usize,
        severity: v["severity"].as_u64().unwrap_or(2) as u8,
        message: v["message"].as_str()?.lines().next()?.to_string(),
        raw: v.clone(),
    })
}

/// Every location in a Location / Location[] / LocationLink[] result.
fn parse_locations(result: &Value) -> Vec<Location> {
    let one = |loc: &Value| -> Option<Location> {
        let (uri, range) = match loc.get("uri") {
            Some(u) => (u, loc.get("range")?),
            None => (
                loc.get("targetUri")?,
                loc.get("targetSelectionRange")
                    .or_else(|| loc.get("targetRange"))?,
            ),
        };
        Some(Location {
            path: path_from_uri(uri.as_str()?)?,
            line: range["start"]["line"].as_u64()? as usize,
            col: range["start"]["character"].as_u64()? as usize,
        })
    };
    match result {
        Value::Array(items) => items.iter().filter_map(one).collect(),
        Value::Null => Vec::new(),
        single => one(single).into_iter().collect(),
    }
}

/// The active signature's label and the char range of its active parameter.
fn parse_signature(result: &Value) -> Option<(String, Option<(usize, usize)>)> {
    let sigs = result["signatures"].as_array()?;
    let active = result["activeSignature"].as_u64().unwrap_or(0) as usize;
    let sig = sigs.get(active).or_else(|| sigs.first())?;
    let label = sig["label"].as_str()?.to_string();
    let param_idx = sig["activeParameter"]
        .as_u64()
        .or_else(|| result["activeParameter"].as_u64());
    let range = param_idx
        .and_then(|i| sig["parameters"].as_array()?.get(i as usize).cloned())
        .and_then(|p| match &p["label"] {
            // A substring of the label: find it.
            Value::String(s) => {
                let byte = label.find(s.as_str())?;
                let start = label[..byte].chars().count();
                Some((start, start + s.chars().count()))
            }
            // [start, end) in UTF-16 units of the label.
            Value::Array(pair) => {
                let (a, b) = (
                    pair.first()?.as_u64()? as usize,
                    pair.get(1)?.as_u64()? as usize,
                );
                let to_char = |units: usize| {
                    let mut seen = 0;
                    label
                        .chars()
                        .take_while(|c| {
                            let keep = seen < units;
                            seen += c.len_utf16();
                            keep
                        })
                        .count()
                };
                Some((to_char(a), to_char(b)))
            }
            _ => None,
        });
    Some((label, range))
}

/// DocumentSymbol[] (a tree, flattened with depths) or SymbolInformation[].
fn parse_symbols(result: &Value) -> Vec<Symbol> {
    fn walk(
        items: &[Value],
        depth: usize,
        container: &str,
        path: Option<&Path>,
        out: &mut Vec<Symbol>,
    ) {
        for item in items {
            let Some(name) = item["name"].as_str() else {
                continue;
            };
            let kind = symbol_kind(item["kind"].as_u64().unwrap_or(0));
            // DocumentSymbol: a range in the requested file. SymbolInformation:
            // a full location with its own uri.
            let location =
                if let Some(range) = item.get("selectionRange").or_else(|| item.get("range")) {
                    path.map(|p| Location {
                        path: p.to_path_buf(),
                        line: range["start"]["line"].as_u64().unwrap_or(0) as usize,
                        col: range["start"]["character"].as_u64().unwrap_or(0) as usize,
                    })
                } else {
                    parse_locations(&item["location"]).into_iter().next()
                };
            let Some(location) = location else {
                continue;
            };
            let own_container = item["containerName"].as_str().unwrap_or(container);
            out.push(Symbol {
                name: name.to_string(),
                kind,
                depth,
                container: own_container.to_string(),
                location,
            });
            if let Some(children) = item["children"].as_array() {
                walk(children, depth + 1, name, path, out);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(items) = result.as_array() {
        // DocumentSymbols carry no uri; the editor fills the path in.
        walk(items, 0, "", Some(Path::new("")), &mut out);
    }
    out
}

fn symbol_kind(n: u64) -> &'static str {
    match n {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type parameter",
        _ => "symbol",
    }
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
        // MarkedString[]: the signature is usually first and the prose after
        // it, so all of them together are the answer, not just the first.
        let parts: Vec<String> = contents
            .as_array()?
            .iter()
            .filter_map(|part| {
                part.as_str()
                    .map(str::to_string)
                    .or_else(|| Some(part.get("value")?.as_str()?.to_string()))
            })
            .collect();
        if parts.is_empty() {
            return None;
        }
        parts.join("\n\n")
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

pub fn uri_from_path(path: &Path) -> String {
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

pub fn path_from_uri(uri: &str) -> Option<PathBuf> {
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

    /// What rust-analyzer really sends for rustc's E0106: the suggestions
    /// ride in `relatedInformation`, the compiler's rendering in `data`.
    #[test]
    fn a_compiler_diagnostic_keeps_its_suggestions() {
        let d = parse_diagnostic(&json!({
            "range": {"start": {"line": 0, "character": 24}},
            "severity": 1,
            "message": "missing lifetime specifier\nexpected named lifetime parameter",
            "relatedInformation": [
                {"message": "consider using the `'static` lifetime: `'static `"},
                {"message": "instead, return an owned value: `String`"}
            ],
            "data": {"rendered": "error[E0106]: missing lifetime specifier\nhelp: consider\n\n"}
        }))
        .unwrap();
        assert_eq!(d.message, "missing lifetime specifier");
        assert_eq!(d.notes().count(), 2);
        assert!(d.detail().ends_with("help: consider"));
        assert!(!d.is_echo());

        // The help republished as a hint: no note of its own, and no
        // rendering, so the detail is assembled from the message.
        let echo = parse_diagnostic(&json!({
            "range": {"start": {"line": 0, "character": 25}},
            "severity": 4,
            "message": "consider using the `'static` lifetime",
            "relatedInformation": [{"message": "original diagnostic"}]
        }))
        .unwrap();
        assert!(echo.is_echo());
        assert_eq!(echo.notes().count(), 0);
        assert_eq!(echo.detail(), "consider using the `'static` lifetime");
        // The error with a suggestion outranks a bare one and any hint.
        assert!(d.rank() < echo.rank());
    }
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

    /// The Oxigen round trip: handshake, the `<` trigger the type list hangs
    /// off, and completions parsed back out of the response.
    #[test]
    #[ignore] // needs oxigen-lsp on PATH; run with `cargo test -- --ignored`
    fn oxigen_completions_arrive_from_its_server() {
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join("crow-oxigen-test");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("probe.oxi");
        let src = "fun helper(a <int>) { a }\nx <\n";
        std::fs::write(&file, src).unwrap();

        let Some(mut client) = Client::spawn(&dir, "oxigen-lsp") else {
            return; // no server installed — nothing to verify
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline && !client.ready {
            let _ = client.poll();
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(client.ready, "initialize handshake did not complete");
        assert!(
            client.triggers_completion('<'),
            "the server advertises `<` as a completion trigger"
        );
        client.did_open(&file, src.to_string(), 0);
        client.request_position("completion", "textDocument/completion", &file, 1, 3);

        let mut items = None;
        while Instant::now() < deadline && items.is_none() {
            for event in client.poll() {
                if let Event::Completions(list, _) = event {
                    items = Some(list);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        client.shutdown();
        let items = items.expect("no completions arrived");
        assert!(
            items.iter().any(|(label, _, _)| label == "int"),
            "the type list should be offered after `<`: {items:?}"
        );
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
    fn signature_help_finds_the_active_parameter() {
        let by_label = json!({"signatures": [{"label": "fn f(a: i32, b: u8)",
            "parameters": [{"label": "a: i32"}, {"label": "b: u8"}]}], "activeParameter": 1});
        let (label, range) = parse_signature(&by_label).unwrap();
        assert_eq!(&label[range.unwrap().0..range.unwrap().1], "b: u8");
        let by_offset = json!({"signatures": [{"label": "f(x, y)",
            "parameters": [{"label": [2, 3]}, {"label": [5, 6]}], "activeParameter": 0}]});
        assert_eq!(parse_signature(&by_offset).unwrap().1, Some((2, 3)));
        assert!(parse_signature(&Value::Null).is_none());
    }

    #[test]
    fn symbols_flatten_with_depth_and_containers() {
        let tree = json!([{"name": "Foo", "kind": 23,
            "range": {"start": {"line": 0, "character": 0}},
            "selectionRange": {"start": {"line": 0, "character": 7}},
            "children": [{"name": "bar", "kind": 6,
                "selectionRange": {"start": {"line": 2, "character": 4}}}]}]);
        let syms = parse_symbols(&tree);
        assert_eq!(syms.len(), 2);
        assert_eq!(
            (syms[1].name.as_str(), syms[1].depth, syms[1].kind),
            ("bar", 1, "method")
        );
        assert_eq!(syms[1].container, "Foo");
        assert_eq!(syms[0].location.col, 7);
        let flat = json!([{"name": "main", "kind": 12, "containerName": "crate",
            "location": {"uri": "file:///a.rs", "range": {"start": {"line": 3, "character": 0}}}}]);
        let syms = parse_symbols(&flat);
        assert_eq!(syms[0].location.path, PathBuf::from("/a.rs"));
    }

    #[test]
    fn locations_parse_as_a_list() {
        let locs = json!([{"uri": "file:///a.rs", "range": {"start": {"line": 1, "character": 2}}},
            {"uri": "file:///b.rs", "range": {"start": {"line": 3, "character": 4}}}]);
        assert_eq!(parse_locations(&locs).len(), 2);
        assert!(parse_locations(&Value::Null).is_empty());
    }

    #[test]
    fn hover_skips_code_fences() {
        let h = json!({"contents": {"kind": "markdown", "value": "```rust\nfn foo()\n```"}});
        assert_eq!(hover_text(&h).unwrap(), "fn foo()");
    }
}
