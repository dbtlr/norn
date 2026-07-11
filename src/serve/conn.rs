//! Per-connection handling: control-frame preamble, then MCP.
//!
//! Every accepted connection starts with exactly one newline-delimited
//! [`ControlFrame`] first line, read under a hard deadline and byte cap. The
//! first frame decides the connection's fate:
//!
//! - `ping` → answer one `pong` and close. This path is the O(1) liveness probe
//!   the routing client gates on (ADR 0005): it touches NO vault and takes NO
//!   map lock, so a busy daemon still answers instantly regardless of query load.
//! - `hello` → resolve the named vault's warm [`McpServer`], answer `ready`, then
//!   hand the REST of the stream to rmcp to serve MCP. The buffered read half is
//!   reused so any bytes the client pipelined after the `hello` line (e.g. the
//!   MCP `initialize`) are not lost.
//! - anything else (garbage, or a daemon→client frame sent as a first line) →
//!   one `error` frame and close.
//!
//! A connection error is logged as a single stderr line and never crashes the
//! daemon (each connection runs in its own task).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use rmcp::ServiceExt;

use crate::serve::contexts::Contexts;
use crate::service::{ControlFrame, CONTROL_PROTOCOL, MAX_CONTROL_FRAME_BYTES};

/// Hard deadline for the first control line. A live client sends it
/// immediately; anything slower is a stuck or hostile peer we drop.
const FIRST_LINE_TIMEOUT: Duration = Duration::from_secs(5);

/// Handle one accepted connection end to end. `start` is the daemon's start
/// instant, used to report `uptime_secs` in a pong.
pub(crate) async fn handle_connection(
    stream: tokio::net::UnixStream,
    contexts: Arc<Contexts>,
    start: Instant,
) -> anyhow::Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut write_half = write_half;

    // Read the first control line under a deadline + byte cap. Any failure
    // (timeout, oversize, EOF-before-newline, I/O) drops the connection.
    let line = match tokio::time::timeout(
        FIRST_LINE_TIMEOUT,
        read_first_line_capped(&mut reader, MAX_CONTROL_FRAME_BYTES),
    )
    .await
    {
        Ok(Ok(Some(line))) => line,
        // EOF before a full line, oversize/I-O error, or timeout: drop quietly.
        Ok(Ok(None)) | Ok(Err(_)) | Err(_) => return Ok(()),
    };

    let frame: ControlFrame = match serde_json::from_slice(&line) {
        Ok(f) => f,
        Err(_) => {
            write_error(&mut write_half, "malformed control frame").await;
            return Ok(());
        }
    };

    match frame {
        // Ping: answer promptly, touch nothing slow. A protocol mismatch still
        // gets our pong — the client decides whether to route (its gate checks
        // the pong's protocol).
        ControlFrame::Ping { .. } => {
            let pong = ControlFrame::Pong {
                protocol: CONTROL_PROTOCOL,
                version: env!("CARGO_PKG_VERSION").to_string(),
                // NRN-247: the source-content fingerprint the routing gate
                // requires an exact match on. A rebuild of the same version
                // mints a new id, so a stale daemon fails the gate.
                build: Some(env!("NORN_BUILD_ID").to_string()),
                pid: Some(std::process::id()),
                uptime_secs: Some(start.elapsed().as_secs()),
            };
            write_frame(&mut write_half, &pong).await?;
        }

        ControlFrame::Hello {
            protocol,
            vault_root,
        } => {
            if protocol != CONTROL_PROTOCOL {
                write_error(
                    &mut write_half,
                    &format!(
                        "control protocol mismatch: client {protocol}, server {CONTROL_PROTOCOL}"
                    ),
                )
                .await;
                return Ok(());
            }
            match contexts.resolve(&vault_root).await {
                Ok(server) => {
                    let ready = ControlFrame::Ready {
                        protocol: CONTROL_PROTOCOL,
                        version: env!("CARGO_PKG_VERSION").to_string(),
                    };
                    write_frame(&mut write_half, &ready).await?;
                    // Serve MCP over the remainder of the stream. `reader` (the
                    // buffered read half) retains any bytes pipelined after the
                    // hello line, so hand THAT to rmcp, not a fresh read half.
                    let service = server.serve((reader, write_half)).await?;
                    service.waiting().await?;
                    // Client disconnected; the warm context stays in the map.
                }
                Err(e) => {
                    eprintln!("norn serve: hello for {vault_root}: {e}");
                    write_error(&mut write_half, &e.to_string()).await;
                }
            }
        }

        // A daemon→client frame (or anything else) as the first line is a
        // protocol error.
        ControlFrame::Pong { .. } | ControlFrame::Ready { .. } | ControlFrame::Error { .. } => {
            write_error(&mut write_half, "unexpected control frame as first line").await;
        }
    }
    Ok(())
}

/// Read a single newline-terminated line from `reader`, bounded by `cap` bytes.
///
/// Returns `Ok(Some(line))` (newline stripped) on a complete line, `Ok(None)` on
/// EOF before any newline, and `Err` if the line exceeds `cap` without a newline.
/// Crucially, it consumes only up to and including the newline from the
/// `BufReader`, leaving any pipelined bytes buffered for the subsequent MCP
/// stream.
///
/// Each iteration only consumes/appends up to `cap - buf.len() + 1` bytes of
/// whatever `fill_buf` makes available, so `buf` can overshoot `cap` by at
/// most one byte before the cap trips — never by a full extra `fill_buf`
/// chunk. Any unconsumed remainder stays buffered in `reader` for the next
/// `fill_buf` call (or for the subsequent MCP stream), so no bytes are lost.
async fn read_first_line_capped<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            // Clean EOF. A partial (newline-less) buffer is discarded as "no
            // frame" — the caller drops the connection either way.
            return Ok(None);
        }
        // Bound how much of `available` we look at / take this iteration —
        // `buf.len() <= cap` is the loop invariant (checked below), so this
        // budget is always >= 1.
        let budget = cap - buf.len() + 1;
        let window = &available[..available.len().min(budget)];
        if let Some(pos) = window.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&window[..pos]);
            reader.consume(pos + 1);
            return Ok(Some(buf));
        }
        let taken = window.len();
        buf.extend_from_slice(window);
        reader.consume(taken);
        if buf.len() > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "control frame exceeded the byte cap without a newline",
            ));
        }
    }
}

/// Serialize `frame` as one newline-delimited JSON line, write it, and flush.
async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &ControlFrame) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    w.write_all(&bytes).await?;
    w.flush().await?;
    Ok(())
}

/// Best-effort write of an `error` control frame. Never fights a dead peer: a
/// write failure here is ignored (the connection is being dropped anyway).
async fn write_error<W: AsyncWrite + Unpin>(w: &mut W, message: &str) {
    let frame = ControlFrame::Error {
        protocol: CONTROL_PROTOCOL,
        message: message.to_string(),
    };
    let _ = write_frame(w, &frame).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded_vault() -> (tempfile::TempDir, camino::Utf8PathBuf) {
        let tmp = tempfile::Builder::new()
            .prefix("norn-serve-conn-")
            .tempdir()
            .unwrap();
        let root = camino::Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        for (name, kind) in [("alpha", "note"), ("beta", "task")] {
            std::fs::write(
                root.join(format!("{name}.md")),
                format!("---\ntype: {kind}\nstatus: active\n---\n{name} body\n"),
            )
            .unwrap();
        }
        (tmp, root)
    }

    /// Read one newline-terminated line from a tokio reader.
    async fn read_line<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Option<String> {
        let mut br = BufReader::new(r);
        let mut s = String::new();
        let n = br.read_line(&mut s).await.unwrap();
        if n == 0 {
            None
        } else {
            Some(s)
        }
    }

    /// A ping first line yields exactly one pong carrying this build's version
    /// and the process pid, then EOF.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ping_yields_one_pong_then_eof() {
        let (server, mut client) = tokio::net::UnixStream::pair().unwrap();
        let contexts = Arc::new(Contexts::new());
        let start = Instant::now();
        let handle = tokio::spawn(async move {
            handle_connection(server, contexts, start).await.unwrap();
        });

        client
            .write_all(b"{\"norn_control\":\"ping\",\"protocol\":1}\n")
            .await
            .unwrap();

        let line = read_line(&mut client).await.expect("expected a pong line");
        let frame: ControlFrame = serde_json::from_str(line.trim()).unwrap();
        match frame {
            ControlFrame::Pong {
                protocol,
                version,
                build,
                pid,
                ..
            } => {
                assert_eq!(protocol, CONTROL_PROTOCOL);
                assert_eq!(version, env!("CARGO_PKG_VERSION"));
                // NRN-247: the daemon stamps its build fingerprint so the
                // routing gate can require an exact match.
                assert_eq!(build.as_deref(), Some(env!("NORN_BUILD_ID")));
                assert_eq!(pid, Some(std::process::id()));
            }
            other => panic!("expected pong, got {other:?}"),
        }
        // No second frame: connection closes after the pong.
        assert!(
            read_line(&mut client).await.is_none(),
            "expected EOF after pong"
        );
        handle.await.unwrap();
    }

    /// A garbage first line yields one error frame.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn garbage_first_line_yields_error_frame() {
        let (server, mut client) = tokio::net::UnixStream::pair().unwrap();
        let contexts = Arc::new(Contexts::new());
        let handle = tokio::spawn(async move {
            handle_connection(server, contexts, Instant::now())
                .await
                .unwrap();
        });

        client.write_all(b"this is not json\n").await.unwrap();
        let line = read_line(&mut client)
            .await
            .expect("expected an error line");
        let frame: ControlFrame = serde_json::from_str(line.trim()).unwrap();
        assert!(matches!(frame, ControlFrame::Error { .. }), "got {frame:?}");
        handle.await.unwrap();
    }

    /// A hello at a mismatched protocol yields one error frame (no serve).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hello_protocol_mismatch_yields_error_frame() {
        let (server, mut client) = tokio::net::UnixStream::pair().unwrap();
        let contexts = Arc::new(Contexts::new());
        let handle = tokio::spawn(async move {
            handle_connection(server, contexts, Instant::now())
                .await
                .unwrap();
        });

        client
            .write_all(b"{\"norn_control\":\"hello\",\"protocol\":9999,\"vault_root\":\"/tmp\"}\n")
            .await
            .unwrap();
        let line = read_line(&mut client)
            .await
            .expect("expected an error line");
        let frame: ControlFrame = serde_json::from_str(line.trim()).unwrap();
        assert!(matches!(frame, ControlFrame::Error { .. }), "got {frame:?}");
        handle.await.unwrap();
    }

    /// An oversize first line with no newline drops the connection within the
    /// deadline (no frame written).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn oversize_first_line_drops_connection() {
        let (server, mut client) = tokio::net::UnixStream::pair().unwrap();
        let contexts = Arc::new(Contexts::new());
        let handle = tokio::spawn(async move {
            handle_connection(server, contexts, Instant::now())
                .await
                .unwrap();
        });

        // > MAX_CONTROL_FRAME_BYTES with no newline.
        let junk = vec![b'x'; MAX_CONTROL_FRAME_BYTES + 1024];
        let _ = client.write_all(&junk).await;
        let start = Instant::now();
        // The connection is dropped (cap tripped) well before the 5s deadline.
        assert!(read_line(&mut client).await.is_none(), "expected no frame");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "should drop near the cap, not wait the full deadline (elapsed {:?})",
            start.elapsed()
        );
        handle.await.unwrap();
    }

    /// A hello for a real vault yields Ready, then rmcp is wired to the buffered
    /// read half — a raw JSON-RPC initialize + tools/list round-trips over the
    /// SAME stream (catches the lost-buffered-bytes bug).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn hello_then_mcp_initialize_and_tools_list() {
        let (_tmp, root) = seeded_vault();
        let (server, client) = tokio::net::UnixStream::pair().unwrap();
        let contexts = Arc::new(Contexts::new());
        let handle = tokio::spawn(async move {
            handle_connection(server, contexts, Instant::now())
                .await
                .unwrap();
        });

        let (read_half, mut write_half) = client.into_split();
        let mut reader = BufReader::new(read_half);

        // hello
        let hello = format!(
            "{{\"norn_control\":\"hello\",\"protocol\":1,\"vault_root\":{}}}\n",
            serde_json::to_string(root.as_str()).unwrap()
        );
        write_half.write_all(hello.as_bytes()).await.unwrap();

        // ready
        let mut ready_line = String::new();
        reader.read_line(&mut ready_line).await.unwrap();
        let ready: ControlFrame = serde_json::from_str(ready_line.trim()).unwrap();
        assert!(matches!(ready, ControlFrame::Ready { .. }), "got {ready:?}");

        // Raw JSON-RPC over the same stream.
        let init = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "serve-test", "version": "0.0.1"}
            }
        });
        let list = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
        });
        let mut msg = serde_json::to_vec(&init).unwrap();
        msg.push(b'\n');
        msg.extend_from_slice(&serde_json::to_vec(&list).unwrap());
        msg.push(b'\n');
        write_half.write_all(&msg).await.unwrap();

        // Collect responses until we see id=2 (tools/list).
        let mut init_ok = false;
        let mut tools: Option<serde_json::Value> = None;
        for _ in 0..10 {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            if n == 0 {
                break;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            if v["id"] == 1 {
                assert!(
                    v["result"]["capabilities"].get("tools").is_some(),
                    "initialize must advertise tools capability: {v}"
                );
                init_ok = true;
            }
            if v["id"] == 2 {
                tools = Some(v["result"]["tools"].clone());
                break;
            }
        }
        assert!(init_ok, "did not see a valid initialize response");
        let tools = tools.expect("did not see a tools/list response");
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(
            names.contains(&"vault.get"),
            "tools/list must include vault.get, got {names:?}"
        );

        // Close the client so the daemon-side serve ends and the task joins.
        drop(write_half);
        drop(reader);
        handle.await.unwrap();
    }

    /// Two hellos for the same vault both get Ready and the map holds ONE entry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_vault_double_hello_one_entry() {
        let (_tmp, root) = seeded_vault();
        let contexts = Arc::new(Contexts::new());

        async fn one_hello(contexts: Arc<Contexts>, root: String) {
            let (server, client) = tokio::net::UnixStream::pair().unwrap();
            let h = tokio::spawn(async move {
                handle_connection(server, contexts, Instant::now())
                    .await
                    .unwrap();
            });
            let (read_half, mut write_half) = client.into_split();
            let mut reader = BufReader::new(read_half);
            let hello = format!(
                "{{\"norn_control\":\"hello\",\"protocol\":1,\"vault_root\":{}}}\n",
                serde_json::to_string(&root).unwrap()
            );
            write_half.write_all(hello.as_bytes()).await.unwrap();
            let mut ready_line = String::new();
            reader.read_line(&mut ready_line).await.unwrap();
            let ready: ControlFrame = serde_json::from_str(ready_line.trim()).unwrap();
            assert!(matches!(ready, ControlFrame::Ready { .. }), "got {ready:?}");
            // By the time Ready is received, resolve() has inserted the map
            // entry. Close without completing MCP init; the daemon-side serve
            // ends with an abrupt-close error we don't care about here.
            drop(write_half);
            drop(reader);
            let _ = h.await;
        }

        one_hello(Arc::clone(&contexts), root.to_string()).await;
        one_hello(Arc::clone(&contexts), root.to_string()).await;

        assert_eq!(contexts.len().await, 1, "same vault must map to one entry");
    }
}
