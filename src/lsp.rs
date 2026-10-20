//! A diagnostics-only language server for ShyHDL, over stdio.
//!
//! An editor already speaks Rust to `rust-analyzer`, and ShyHDL source *is*
//! Rust syntax. Every DSL construct the analyzer does not know (`uint<N>`,
//! `always!`, `#[In]`, ...) therefore comes back as an error, even though the
//! file is valid ShyHDL. This server answers the same documents with the
//! frontend's own diagnostics, so the editor can show what the ShyHDL compiler
//! would say.
//!
//! # Scope
//!
//! Diagnostics only. Completion, hover, go-to-definition and document symbols
//! are not implemented, and `initialize` advertises nothing but full text
//! synchronisation.
//!
//! # How a file is checked
//!
//! A file may declare several `#[hardware_instance]` structs, while the
//! frontend compiles one top module at a time. The server asks
//! [`concrete_instances`] which modules can be a top with no arguments, compiles
//! once per candidate, and unions the diagnostics. Identical diagnostics are
//! reported once, and `SH-F023` (a module unreachable from the top) is dropped:
//! it is a property of one chosen top, not of the file, and repeating it per
//! candidate would mark every candidate unreachable.
//!
//! # Limitations
//!
//! * A `mod name;` cannot be resolved for an unsaved buffer: [`compile_all`]
//!   compiles the text it is given and never reads another file. A file that
//!   loads a child module is reported as such, not silently compiled with items
//!   missing.
//! * A file whose only hardware structs require const parameters
//!   (`Adder<const N: usize>`) has no concrete top to probe, so it produces no
//!   diagnostics of its own. Its errors are found through the modules that
//!   instantiate it, which is how the CLI compiles it too.
//! * A [`SourceSpan`] carries one position, not a range, so every diagnostic is
//!   a zero-width range at that position.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

use crate::frontend::{codes, compile_all, concrete_instances, Diagnostic, Severity};
use crate::metadata::SourceSpan;

/// What `initialize` advertises: full text synchronisation, nothing else.
///
/// `textDocumentSync` is `TextDocumentSyncKind.Full` (1): every change carries
/// the whole document, which is the only mode this server implements.
const INITIALIZE_RESULT: &str = r#"{"capabilities":{"textDocumentSync":1}}"#;

/// LSP `DiagnosticSeverity` values.
const SEVERITY_ERROR: u8 = 1;
const SEVERITY_WARNING: u8 = 2;

/// JSON-RPC error code for a body that is not JSON at all.
const PARSE_ERROR: i32 = -32700;
/// JSON-RPC error code for a well-formed request this server does not serve.
const METHOD_NOT_FOUND: i32 = -32601;
/// JSON-RPC error code for a request that arrives after `shutdown`.
const INVALID_REQUEST: i32 = -32600;

/// Serve LSP over this process's stdin and stdout until the client exits.
///
/// The returned code is the LSP convention, not the CLI's own table: `0` when a
/// `shutdown` request preceded `exit`, `1` otherwise (a client that disappears
/// without shutting down is a failure of the session, not a success).
pub fn serve_stdio() -> io::Result<u8> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    serve(&mut reader, &mut writer)
}

/// Serve LSP on an arbitrary transport, which is what the tests use.
pub fn serve(reader: &mut impl BufRead, writer: &mut impl Write) -> io::Result<u8> {
    let mut server = Server {
        writer,
        documents: HashMap::new(),
        shutdown: false,
    };
    loop {
        let Some(body) = read_message(reader)? else {
            // The client closed the pipe without `exit`.
            return Ok(if server.shutdown { 0 } else { 1 });
        };
        if server.handle(&body)? == Step::Exit {
            return Ok(if server.shutdown { 0 } else { 1 });
        }
    }
}

/// Whether the server keeps serving after a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Continue,
    Exit,
}

/// One open document's text, keyed by its URI.
struct Server<'w, W: Write> {
    writer: &'w mut W,
    documents: HashMap<String, String>,
    shutdown: bool,
}

impl<W: Write> Server<'_, W> {
    /// Handle one framed JSON-RPC message.
    fn handle(&mut self, body: &[u8]) -> io::Result<Step> {
        let message: Value = match serde_json::from_slice(body) {
            Ok(message) => message,
            Err(error) => {
                // A body that is not JSON has no id to answer, so the error
                // carries a null id, as JSON-RPC requires.
                self.respond_error(Value::Null, PARSE_ERROR, format!("invalid JSON: {error}"))?;
                return Ok(Step::Continue);
            }
        };
        let Some(object) = message.as_object() else {
            self.respond_error(
                Value::Null,
                INVALID_REQUEST,
                "a JSON-RPC message must be an object".to_string(),
            )?;
            return Ok(Step::Continue);
        };
        let method = object
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let id = object.get("id").filter(|id| !id.is_null()).cloned();
        let empty = serde_json::Map::new();
        let params = object
            .get("params")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        match (method, id) {
            // A request: it has an id and expects exactly one response.
            (Some(method), Some(id)) => self.handle_request(&method, id),
            // A notification: no id, so it must never be answered.
            (Some(method), None) => self.handle_notification(&method, params),
            // No method: a response to a request this server never sends.
            // There is nothing to answer, and answering would be a protocol
            // error of our own.
            (None, _) => Ok(Step::Continue),
        }
    }

    fn handle_request(&mut self, method: &str, id: Value) -> io::Result<Step> {
        if self.shutdown {
            // The only request allowed after `shutdown` is another `shutdown`,
            // which the specification also allows to answer.
            if method == "shutdown" {
                self.respond(id, Value::Null)?;
            } else {
                self.respond_error(
                    id,
                    INVALID_REQUEST,
                    format!("`{method}` arrived after shutdown"),
                )?;
            }
            return Ok(Step::Continue);
        }
        match method {
            "initialize" => {
                let result: Value = serde_json::from_str(INITIALIZE_RESULT)
                    .expect("the initialize result is a JSON literal");
                self.respond(id, result)?;
                Ok(Step::Continue)
            }
            "shutdown" => {
                self.shutdown = true;
                self.respond(id, Value::Null)?;
                Ok(Step::Continue)
            }
            other => {
                self.respond_error(
                    id,
                    METHOD_NOT_FOUND,
                    format!("`{other}` is not implemented by this diagnostics-only server"),
                )?;
                Ok(Step::Continue)
            }
        }
    }

    fn handle_notification(
        &mut self,
        method: &str,
        params: &serde_json::Map<String, Value>,
    ) -> io::Result<Step> {
        // Every arm that opens or changes a document publishes and then keeps
        // serving; `exit` is the only notification that ends the session.
        match method {
            // The client acknowledges `initialize`; no reply, no state.
            "initialized" => return Ok(Step::Continue),
            "exit" => return Ok(Step::Exit),
            // After `shutdown` the client may only send `exit`, which is
            // handled above. Anything else is dropped rather than answered.
            _ if self.shutdown => return Ok(Step::Continue),
            _ => {}
        }
        let Some(document) = params.get("textDocument") else {
            return Ok(Step::Continue);
        };
        let Some(uri) = document.get("uri").and_then(Value::as_str) else {
            return Ok(Step::Continue);
        };
        match method {
            "textDocument/didOpen" => {
                let Some(text) = document.get("text").and_then(Value::as_str) else {
                    return Ok(Step::Continue);
                };
                self.documents.insert(uri.to_string(), text.to_string());
                self.publish(uri)?;
            }
            "textDocument/didChange" => {
                // Full synchronisation: the last change carries the whole
                // document. An empty change list would leave the text stale, so
                // it is ignored rather than treated as an empty document.
                let text = params
                    .get("contentChanges")
                    .and_then(Value::as_array)
                    .and_then(|changes| changes.iter().rev().find_map(|change| change.get("text")))
                    .and_then(Value::as_str);
                if let Some(text) = text {
                    self.documents.insert(uri.to_string(), text.to_string());
                }
                self.publish(uri)?;
            }
            "textDocument/didSave" => {
                // A save may carry the text; when it does, it is newer than
                // what the last change left behind.
                if let Some(text) = params.get("text").and_then(Value::as_str) {
                    self.documents.insert(uri.to_string(), text.to_string());
                }
                self.publish(uri)?;
            }
            "textDocument/didClose" => {
                self.documents.remove(uri);
                // A closed document keeps showing its last diagnostics unless
                // they are explicitly cleared.
                self.notify(
                    "textDocument/publishDiagnostics",
                    json!({ "uri": uri, "diagnostics": [] }),
                )?;
            }
            // An unknown notification is ignored: replying to it would violate
            // JSON-RPC, and failing the session over it would be worse.
            _ => {}
        }
        Ok(Step::Continue)
    }

    /// Check one open document and publish its diagnostics under its own URI.
    fn publish(&mut self, uri: &str) -> io::Result<()> {
        let Some(source) = self.documents.get(uri).cloned() else {
            return Ok(());
        };
        // The frontend names the source by path in its spans; that name must not
        // leak into the published URI, which is the document's own.
        let diagnostics = diagnostics_for(&source, uri);
        let items: Vec<Value> = diagnostics.iter().map(lsp_diagnostic).collect();
        self.notify(
            "textDocument/publishDiagnostics",
            json!({ "uri": uri, "diagnostics": items }),
        )
    }

    fn notify(&mut self, method: &str, params: Value) -> io::Result<()> {
        write_message(
            self.writer,
            &json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        )
    }

    fn respond(&mut self, id: Value, result: Value) -> io::Result<()> {
        write_message(
            self.writer,
            &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        )
    }

    fn respond_error(&mut self, id: Value, code: i32, message: String) -> io::Result<()> {
        write_message(
            self.writer,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": code, "message": message },
            }),
        )
    }
}

/// Every diagnostic a document should show, for one source text.
///
/// The source is compiled once per concrete top module candidate and the
/// results are unioned. A file that does not parse reports its syntax errors
/// only: there is no design to lower. A file with no concrete candidate (only
/// parameterised modules, or no hardware struct at all) has nothing this
/// single-source check can say, so it reports nothing rather than a guess.
pub fn diagnostics_for(source: &str, source_name: &str) -> Vec<Diagnostic> {
    let candidates = match concrete_instances(source, source_name) {
        Ok(candidates) => candidates,
        Err(errors) => return errors.into_vec(),
    };
    let mut seen: HashSet<(bool, &'static str, Option<SourceSpan>, String)> = HashSet::new();
    let mut diagnostics = Vec::new();
    for top in &candidates {
        let found = match compile_all(source, source_name, top) {
            Ok(compiled) => compiled.warnings().to_vec(),
            Err(errors) => errors.into_vec(),
        };
        for diagnostic in found {
            // Reachability is relative to the top that was compiled: for top
            // `A`, `B` is unreachable, and the other way round. Unioning those
            // would call every candidate unreachable, so the warning is dropped
            // rather than shown once per candidate.
            if diagnostic.code == codes::UNREACHABLE {
                continue;
            }
            // The severity is folded to "is an error" because the frontend has
            // exactly two severities; this keeps the key hashable without
            // changing the shared `Severity` type.
            let key = (
                diagnostic.is_error(),
                diagnostic.code,
                diagnostic.span.clone(),
                diagnostic.message.clone(),
            );
            if seen.insert(key) {
                diagnostics.push(diagnostic);
            }
        }
    }
    diagnostics
}

/// One frontend diagnostic as an LSP `Diagnostic` object.
fn lsp_diagnostic(diagnostic: &Diagnostic) -> Value {
    // Notes carry the actionable hint ("widen explicitly with..."). LSP has
    // `relatedInformation` for that, but a note may also have no location, so
    // the hint is appended to the message, which every client shows.
    let mut message = diagnostic.message.clone();
    for note in &diagnostic.notes {
        message.push_str("\nnote: ");
        if let Some(span) = &note.span {
            message.push_str(&format!("{span}: "));
        }
        message.push_str(&note.message);
    }
    let start = diagnostic
        .span
        .as_ref()
        .map(Position::from_span)
        // A diagnostic without a location cannot be placed. LSP has no way to
        // say "nowhere", so it is anchored at the start of the document instead
        // of being dropped.
        .unwrap_or(Position {
            line: 0,
            character: 0,
        });
    json!({
        "range": { "start": start.to_json(), "end": start.to_json() },
        "severity": match diagnostic.severity {
            Severity::Error => SEVERITY_ERROR,
            Severity::Warning => SEVERITY_WARNING,
        },
        "code": diagnostic.code,
        "source": "shyhdl",
        "message": message,
    })
}

/// A zero-based position in a document, as LSP counts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Position {
    line: u32,
    character: u32,
}

impl Position {
    /// Convert a 1-based [`SourceSpan`] into a zero-based LSP position.
    ///
    /// The span carries a single point, not a range, so the diagnostic range is
    /// zero-width: start and end are the same position. That marks the exact
    /// line and column the frontend reported without inventing an extent.
    ///
    /// `character` is the column minus one. `SourceSpan` counts columns in
    /// characters while LSP counts UTF-16 code units; the two agree for ASCII
    /// and for every BMP character, and differ only after an astral character
    /// (an emoji) on the same line.
    fn from_span(span: &SourceSpan) -> Self {
        let to_u32 = |value: usize| u32::try_from(value).unwrap_or(u32::MAX);
        Self {
            line: to_u32(span.line.saturating_sub(1)),
            character: to_u32(span.column.saturating_sub(1)),
        }
    }

    fn to_json(self) -> Value {
        json!({ "line": self.line, "character": self.character })
    }
}

/// Read one `Content-Length`-framed message body.
///
/// `Ok(None)` means the stream ended cleanly between messages. A stream that
/// ends in the middle of a frame, or a frame with no `Content-Length`, is an
/// error: a truncated message cannot be answered and must not be guessed at.
fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return match content_length {
                None => Ok(None),
                Some(_) => Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the stream ended between the headers and the message body",
                )),
            };
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed LSP header `{header}`: expected `Name: value`"),
            ));
        };
        // `Content-Type` is allowed and carries nothing this server needs; only
        // a repeated or malformed `Content-Length` is a problem.
        if name.trim().eq_ignore_ascii_case("content-length") {
            let length = value.trim().parse::<usize>().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("malformed Content-Length `{}`", value.trim()),
                )
            })?;
            if content_length.replace(length).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the message repeats its Content-Length header",
                ));
            }
        }
    }
    let Some(length) = content_length else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the message has no Content-Length header",
        ));
    };
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Write one JSON-RPC message with its `Content-Length` frame, and flush it.
fn write_message(writer: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}
