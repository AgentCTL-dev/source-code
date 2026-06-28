//! The management-profile client: NDJSON JSON-RPC over a unix socket.
//!
//! Implements the consumer half of the contract management wire (agentd RFC
//! 0005 §3.6 / RFC 0015): a blocking, thread-per-connection-friendly client that
//! `initialize`s, lists tools, reads `agent://` resources, and calls operator
//! tools. It is the only thing that needs the per-pod socket; the operator and
//! the CLI reach it through the node-agent API (RFC 0009).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use agent_contract_client::{parse_manifest, Manifest};
use serde_json::{json, Value};

/// The MCP protocol version this client speaks (contract baseline, MCP 2025-11-25).
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// The capabilities resource URI. Reference-impl spelling (`agentd://`); the
/// neutral `agent://` becomes canonical at the de-branding GA cutover (contract
/// `management-profile` / README de-branding map).
pub const URI_CAPABILITIES: &str = "agentd://capabilities";
/// The live subagent/inventory tree (operator-facing).
pub const URI_INVENTORY: &str = "agentd://inventory";

/// A management-bridge error.
#[derive(Debug)]
pub enum Error {
    /// Transport/IO failure.
    Io(std::io::Error),
    /// A malformed or unparseable JSON frame / manifest.
    Json(serde_json::Error),
    /// A JSON-RPC error response from the agent.
    Rpc { code: i64, message: String },
    /// A protocol violation (e.g. a closed connection mid-exchange).
    Protocol(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Json(e) => write!(f, "json: {e}"),
            Error::Rpc { code, message } => write!(f, "rpc error {code}: {message}"),
            Error::Protocol(m) => write!(f, "protocol: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

/// A connection to a conformant agent's management profile.
pub struct ManagementClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    /// The agent's reported `serverInfo.name` (after [`Self::initialize`]).
    pub server_name: Option<String>,
    /// The agent's reported `serverInfo.version`.
    pub server_version: Option<String>,
}

impl ManagementClient {
    /// Connect to the management socket at `path` (no handshake yet — call
    /// [`Self::initialize`]).
    pub fn connect<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        let stream = UnixStream::connect(path)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            reader,
            writer: stream,
            next_id: 1,
            server_name: None,
            server_version: None,
        })
    }

    /// The kernel-attested pid of the process serving this socket (`SO_PEERCRED`),
    /// or `None` if the credential is unavailable.
    ///
    /// The node-agent is the **client** here (it `connect`s to the per-pod
    /// socket), so the peer credential is the SERVER's — exactly the process we
    /// want to attest against the requested pod UID (RFC 0015).
    ///
    /// `std::os::unix::net::UnixStream::peer_cred` is still nightly-only
    /// (`peer_credentials_unix_socket`), so we read `SO_PEERCRED` directly with
    /// `getsockopt` (Linux). A non-positive pid (e.g. the kernel could not
    /// attribute the peer) is reported as `None`.
    pub fn peer_pid(&self) -> Option<u32> {
        use std::os::unix::io::AsRawFd;
        let fd = self.writer.as_raw_fd();
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred`/`len` are valid, correctly sized out-params for the
        // SO_PEERCRED getsockopt on a connected AF_UNIX stream; `fd` is owned by
        // `self.writer` and outlives the call.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                std::ptr::addr_of_mut!(cred).cast::<libc::c_void>(),
                &mut len,
            )
        };
        if rc != 0 || cred.pid <= 0 {
            return None;
        }
        Some(cred.pid as u32)
    }

    /// The MCP handshake: `initialize`, then the `initialized` notification.
    /// Records the agent's `serverInfo`.
    pub fn initialize(&mut self) -> Result<(), Error> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "agentctl-node-agent",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )?;
        self.server_name = result
            .pointer("/serverInfo/name")
            .and_then(Value::as_str)
            .map(String::from);
        self.server_version = result
            .pointer("/serverInfo/version")
            .and_then(Value::as_str)
            .map(String::from);
        self.notify("notifications/initialized", json!({}))?;
        Ok(())
    }

    /// `tools/list` → the tool names the agent serves to this (management) peer.
    /// The operator tools (drain/lame-duck/cancel) appear only to a management
    /// origin — read them, never assume them.
    pub fn list_tools(&mut self) -> Result<Vec<String>, Error> {
        let r = self.request("tools/list", json!({}))?;
        Ok(r.get("tools")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.get("name").and_then(Value::as_str).map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// `resources/read` → the `text` body of `contents[0]` for `uri`.
    pub fn read_resource_text(&mut self, uri: &str) -> Result<String, Error> {
        let r = self.request("resources/read", json!({ "uri": uri }))?;
        r.pointer("/contents/0/text")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| Error::Protocol(format!("resources/read {uri}: no contents[0].text")))
    }

    /// Read + parse the capabilities manifest via the typed contract client.
    pub fn read_capabilities(&mut self) -> Result<Manifest, Error> {
        let text = self.read_resource_text(URI_CAPABILITIES)?;
        parse_manifest(&text).map_err(Error::Json)
    }

    /// `tools/call` an operator tool.
    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, Error> {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    /// Convenience: the `drain` operator tool (RFC 0015).
    pub fn drain(&mut self) -> Result<Value, Error> {
        self.call_tool("drain", json!({}))
    }

    /// Issue a raw JSON-RPC `method` with `params` and return its `result`.
    ///
    /// The generic escape hatch the node-agent API uses to forward arbitrary
    /// reference methods (e.g. `a2a.SendMessage`/`a2a.GetTask`/`a2a.CancelTask`)
    /// to a conformant agent without this crate knowing their shapes.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        self.request(method, params)
    }

    /// Stream a JSON-RPC `method`: write the request once, then read the
    /// **multiple same-id response frames** the agent emits, passing each frame's
    /// `result` to `on_frame` until (and including) the terminal frame whose
    /// `result.final == true`.
    ///
    /// This is the consumer half of the A2A streaming wire (RFC 0020): the
    /// reference method is `a2a.SendStreamingMessage` (the gateway translates the
    /// spec slash-form `message/stream` to it), the agent answers with a
    /// `working` status-update → an `artifact-update` → a `completed`/`final`
    /// status-update, and the node-agent re-emits each frame as one SSE event.
    ///
    /// Out-of-band frames (interleaved notifications / other ids) are skipped, as
    /// in [`Self::request`]. A JSON-RPC error frame surfaces as [`Error::Rpc`]; a
    /// connection closed before the final frame is an [`Error::Protocol`]
    /// (inherited from [`Self::read_line`]). Blocking, like the rest of the
    /// client.
    pub fn stream(
        &mut self,
        method: &str,
        params: Value,
        mut on_frame: impl FnMut(Value),
    ) -> Result<(), Error> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_msg(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }))?;
        loop {
            let v: Value = serde_json::from_str(&self.read_line()?)?;
            if v.get("id") != Some(&json!(id)) {
                continue; // notification or an out-of-band id
            }
            if let Some(err) = v.get("error") {
                return Err(Error::Rpc {
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            let result = v.get("result").cloned().unwrap_or(Value::Null);
            // The terminal marker rides on the last status-update.
            let is_final = result
                .get("final")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            on_frame(result);
            if is_final {
                return Ok(());
            }
        }
    }

    // --- wire -------------------------------------------------------------

    fn request(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_msg(&json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }))?;
        // Read frames until the matching response; skip interleaved notifications.
        loop {
            let v: Value = serde_json::from_str(&self.read_line()?)?;
            if v.get("id") != Some(&json!(id)) {
                continue; // notification or an out-of-band id
            }
            if let Some(err) = v.get("error") {
                return Err(Error::Rpc {
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), Error> {
        self.write_msg(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn write_msg(&mut self, v: &Value) -> Result<(), Error> {
        let mut buf = serde_json::to_vec(v)?;
        buf.push(b'\n');
        self.writer.write_all(&buf)?;
        self.writer.flush()?;
        Ok(())
    }

    fn read_line(&mut self) -> Result<String, Error> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(Error::Protocol("connection closed mid-exchange".into()));
        }
        Ok(line)
    }
}
