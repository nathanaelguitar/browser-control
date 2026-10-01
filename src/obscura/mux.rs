//! CDP multiplexer: many downstream clients over one upstream connection.
//!
//! Obscura scopes targets to the CDP connection that created them, while
//! browser-control (and Playwright) expect Chrome's model where every
//! connection sees every page. The mux holds exactly one upstream
//! connection for the lifetime of the browser and emulates the per-client
//! parts of Chrome's target domain on top of it:
//!
//! * Request ids are rewritten per client and responses routed back.
//! * Flat sessions (`sessionId`) belong to the client whose
//!   `Target.attachToTarget` / `Target.attachToBrowserTarget` created them;
//!   their events go only to that client and other clients cannot send on
//!   them. Commands on a client's *browser* session (Playwright drives the
//!   Target domain through one) get the same browser-level treatment as
//!   sessionless commands, and synthesized events are delivered on it.
//! * `Target.setAutoAttach` is never forwarded upstream. The mux attaches a
//!   fresh session per auto-attaching client and synthesizes
//!   `Target.attachedToTarget`, for existing pages and for every new page.
//!   A `Target.createTarget` reply is held until those synthetic attaches
//!   have been delivered, matching Chrome's ordering (Playwright relies on
//!   it).
//! * `Target.setDiscoverTargets` is enabled once upstream; clients that ask
//!   for discovery get synthesized `targetCreated` events for existing pages.
//! * `Target.getTargetInfo` sent on a session without a `targetId` is
//!   pinned to that session's target (Obscura answers with the browser
//!   target otherwise).
//! * `Browser.close` from a client is acknowledged but not forwarded, so a
//!   disconnecting Playwright sidecar cannot take the browser down.
//! * A dismissing dialog shim ([`super::DIALOG_SHIM_JS`]) is installed in
//!   every page on a mux-owned session.
//!
//! When a client disconnects, its sessions are detached upstream; pages
//! stay open (as in Chrome).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// Writer-task command that closes the upstream socket (never valid JSON,
/// so it cannot collide with a CDP frame).
const CLOSE_SENTINEL: &str = "\u{0}close";

/// Bound on mux-internal upstream calls (auto-attach, shim install).
const INTERNAL_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest CDP frame accepted in either direction (full-page screenshots
/// and PDFs are base64 in a single message).
pub const MAX_MESSAGE_BYTES: usize = 256 << 20;

/// WebSocket config shared by the upstream and downstream sockets.
pub fn ws_config() -> WebSocketConfig {
    WebSocketConfig {
        max_message_size: Some(MAX_MESSAGE_BYTES),
        max_frame_size: Some(MAX_MESSAGE_BYTES),
        ..Default::default()
    }
}

type ClientId = u64;

/// Where a browser-level feature was enabled: the root connection
/// (`None`) or one of the client's browser sessions (`Some(sessionId)`).
type Channel = Option<String>;

struct Client {
    tx: mpsc::UnboundedSender<String>,
    auto_attach: Option<Channel>,
    discover: Option<Channel>,
    /// Targets this client already received a synthetic auto-attach for.
    attached: HashSet<String>,
}

struct Session {
    /// `None` for mux-internal sessions (dialog shim).
    client: Option<ClientId>,
    target: String,
    /// A `Target.attachToBrowserTarget` session.
    browser: bool,
    /// Channel that `Target.detachedFromTarget` for this session goes to.
    parent: Channel,
}

enum Pending {
    Client {
        client: ClientId,
        id: Value,
        method: String,
        target: Option<String>,
        session: Channel,
    },
    Internal(oneshot::Sender<Value>),
}

/// An event addressed to `channel` (adds the top-level `sessionId`).
fn event_on(channel: &Channel, method: &str, params: Value) -> Value {
    let mut ev = json!({ "method": method, "params": params });
    if let Some(s) = channel {
        ev["sessionId"] = json!(s);
    }
    ev
}

#[derive(Default)]
struct State {
    next_up_id: u64,
    next_client: ClientId,
    pending: HashMap<u64, Pending>,
    sessions: HashMap<String, Session>,
    clients: HashMap<ClientId, Client>,
    /// Known targets in creation order: (targetId, targetInfo).
    targets: Vec<(String, Value)>,
    shimmed: HashSet<String>,
    target_locks: HashMap<String, Arc<AsyncMutex<()>>>,
    closed: bool,
}

impl State {
    fn target_info(&self, id: &str) -> Option<&Value> {
        self.targets.iter().find(|(t, _)| t == id).map(|(_, v)| v)
    }

    fn upsert_target(&mut self, info: &Value) {
        let Some(id) = info.get("targetId").and_then(Value::as_str) else {
            return;
        };
        if let Some(slot) = self.targets.iter_mut().find(|(t, _)| t == id) {
            slot.1 = info.clone();
        } else {
            self.targets.push((id.to_string(), info.clone()));
        }
    }

    fn page_target_ids(&self) -> Vec<String> {
        self.targets
            .iter()
            .filter(|(_, info)| is_page(info))
            .map(|(id, _)| id.clone())
            .collect()
    }
}

fn is_page(info: &Value) -> bool {
    info.get("type").and_then(Value::as_str).unwrap_or("page") == "page"
}

/// The multiplexer. Cheap to share via `Arc`.
pub struct Mux {
    up_tx: mpsc::UnboundedSender<String>,
    state: StdMutex<State>,
    shim: Option<&'static str>,
}

impl Mux {
    /// Connect to `upstream_ws` and start routing. The returned receiver
    /// fires once the upstream connection is gone (the browser exited).
    pub async fn connect(
        upstream_ws: &str,
        shim: Option<&'static str>,
    ) -> Result<(Arc<Mux>, oneshot::Receiver<()>)> {
        let (ws, _) = tokio::time::timeout(
            crate::transport::CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async_with_config(upstream_ws, Some(ws_config()), false),
        )
        .await
        .map_err(|_| anyhow!("connecting to obscura at {upstream_ws} timed out"))?
        .with_context(|| format!("connecting to obscura at {upstream_ws}"))?;
        Self::from_stream(ws, shim).await
    }

    /// Build a mux over an already-connected upstream WebSocket.
    pub async fn from_stream<S>(
        ws: WebSocketStream<S>,
        shim: Option<&'static str>,
    ) -> Result<(Arc<Mux>, oneshot::Receiver<()>)>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sink, mut stream) = ws.split();
        let (up_tx, mut up_rx) = mpsc::unbounded_channel::<String>();
        let mux = Arc::new(Mux {
            up_tx,
            state: StdMutex::new(State::default()),
            shim,
        });

        tokio::spawn(async move {
            while let Some(text) = up_rx.recv().await {
                if text == CLOSE_SENTINEL {
                    break;
                }
                if sink.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let (done_tx, done_rx) = oneshot::channel();
        let reader_mux = Arc::clone(&mux);
        tokio::spawn(async move {
            while let Some(frame) = stream.next().await {
                match frame {
                    Ok(Message::Text(text)) => reader_mux.on_upstream(&text),
                    Ok(Message::Binary(bytes)) => {
                        if let Ok(text) = std::str::from_utf8(&bytes) {
                            reader_mux.on_upstream(text);
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            reader_mux.shutdown();
            let _ = done_tx.send(());
        });

        mux.call(
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
            None,
        )
        .await
        .context("enabling target discovery on obscura")?;
        let targets = mux
            .call("Target.getTargets", json!({}), None)
            .await
            .context("listing obscura targets")?;
        let pages: Vec<String> = {
            let mut st = mux.lock();
            if let Some(infos) = targets.get("targetInfos").and_then(Value::as_array) {
                for info in infos {
                    st.upsert_target(info);
                }
            }
            st.page_target_ids()
        };
        for id in pages {
            mux.ensure_target_ready(&id).await;
        }
        Ok((mux, done_rx))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Disconnect every client and close the upstream connection. Obscura
    /// waits up to 3 s for live connections before persisting cookies on
    /// SIGTERM, so the supervisor calls this before stopping it.
    pub fn close(&self) {
        self.shutdown();
        let _ = self.up_tx.send(CLOSE_SENTINEL.to_string());
    }

    fn shutdown(&self) {
        let mut st = self.lock();
        st.closed = true;
        // Dropping the senders ends every client writer, which closes the
        // downstream sockets; dropping pending internal senders fails any
        // in-flight `call`.
        st.clients.clear();
        st.pending.clear();
    }

    /// Issue a mux-internal upstream command and return its `result`.
    pub async fn call(&self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.lock();
            if st.closed {
                bail!("obscura connection closed");
            }
            st.next_up_id += 1;
            let id = st.next_up_id;
            st.pending.insert(id, Pending::Internal(tx));
            let mut msg = json!({ "id": id, "method": method, "params": params });
            if let Some(s) = session {
                msg["sessionId"] = json!(s);
            }
            self.up_tx
                .send(msg.to_string())
                .map_err(|_| anyhow!("obscura connection closed"))?;
        }
        let reply = tokio::time::timeout(INTERNAL_CALL_TIMEOUT, rx)
            .await
            .map_err(|_| {
                anyhow!("obscura did not answer {method} within {INTERNAL_CALL_TIMEOUT:?}")
            })?
            .map_err(|_| anyhow!("obscura connection closed during {method}"))?;
        if let Some(err) = reply.get("error") {
            bail!("obscura {method} failed: {err}");
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    fn send_to(&self, client: ClientId, msg: &Value) {
        let tx = self.lock().clients.get(&client).map(|c| c.tx.clone());
        if let Some(tx) = tx {
            let _ = tx.send(msg.to_string());
        }
    }

    fn reply(&self, client: ClientId, id: &Value, session: Option<&str>, result: Value) {
        let mut msg = json!({ "id": id, "result": result });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.send_to(client, &msg);
    }

    fn reply_error(&self, client: ClientId, id: &Value, session: Option<&str>, message: &str) {
        let mut msg = json!({ "id": id, "error": { "code": -32001, "message": message } });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.send_to(client, &msg);
    }

    // -- upstream -> clients -------------------------------------------------

    fn on_upstream(self: &Arc<Self>, text: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(text) else {
            return;
        };
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            self.on_upstream_reply(id, msg);
            return;
        }
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let top_session = msg.get("sessionId").and_then(Value::as_str);
        if method == "Target.detachedFromTarget" {
            let sid = params
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let (removed, top_owner) = {
                let mut st = self.lock();
                let removed = st.sessions.remove(sid);
                let top_owner = top_session
                    .and_then(|t| st.sessions.get(t))
                    .and_then(|s| s.client);
                (removed, top_owner)
            };
            match (top_session, top_owner) {
                (Some(_), Some(owner)) => self.send_to(owner, &msg),
                (Some(_), None) => {}
                (None, _) => {
                    if let Some(Session {
                        client: Some(owner),
                        parent,
                        ..
                    }) = removed
                    {
                        self.send_to(owner, &event_on(&parent, method, params));
                    }
                }
            }
            return;
        }
        if let Some(sid) = top_session {
            let owner = self.lock().sessions.get(sid).and_then(|s| s.client);
            if let Some(owner) = owner {
                self.send_to(owner, &msg);
            }
            return;
        }
        match method {
            "Target.targetCreated" => {
                let info = params.get("targetInfo").cloned().unwrap_or(Value::Null);
                let recipients = {
                    let mut st = self.lock();
                    st.upsert_target(&info);
                    st.clients
                        .iter()
                        .filter_map(|(id, c)| c.discover.clone().map(|ch| (*id, ch)))
                        .collect::<Vec<_>>()
                };
                for (c, ch) in recipients {
                    self.send_to(c, &event_on(&ch, method, params.clone()));
                }
                if is_page(&info) {
                    if let Some(id) = info.get("targetId").and_then(Value::as_str) {
                        let mux = Arc::clone(self);
                        let id = id.to_string();
                        tokio::spawn(async move { mux.ensure_target_ready(&id).await });
                    }
                }
            }
            "Target.targetInfoChanged" => {
                let info = params.get("targetInfo").cloned().unwrap_or(Value::Null);
                let recipients = {
                    let mut st = self.lock();
                    st.upsert_target(&info);
                    st.clients
                        .iter()
                        .filter_map(|(id, c)| {
                            c.discover
                                .clone()
                                .or_else(|| c.auto_attach.clone())
                                .map(|ch| (*id, ch))
                        })
                        .collect::<Vec<_>>()
                };
                for (c, ch) in recipients {
                    self.send_to(c, &event_on(&ch, method, params.clone()));
                }
            }
            "Target.targetDestroyed" => {
                let target = params
                    .get("targetId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let (discoverers, detached) = {
                    let mut st = self.lock();
                    st.targets.retain(|(t, _)| *t != target);
                    st.shimmed.remove(&target);
                    st.target_locks.remove(&target);
                    for c in st.clients.values_mut() {
                        c.attached.remove(&target);
                    }
                    let gone: Vec<String> = st
                        .sessions
                        .iter()
                        .filter(|(_, s)| s.target == target)
                        .map(|(sid, _)| sid.clone())
                        .collect();
                    let mut detached = Vec::new();
                    for sid in gone {
                        if let Some(s) = st.sessions.remove(&sid) {
                            if let Some(c) = s.client {
                                detached.push((c, sid, s.parent));
                            }
                        }
                    }
                    let discoverers = st
                        .clients
                        .iter()
                        .filter_map(|(id, c)| c.discover.clone().map(|ch| (*id, ch)))
                        .collect::<Vec<_>>();
                    (discoverers, detached)
                };
                for (c, sid, parent) in detached {
                    self.send_to(
                        c,
                        &event_on(
                            &parent,
                            "Target.detachedFromTarget",
                            json!({ "sessionId": sid, "targetId": target }),
                        ),
                    );
                }
                for (c, ch) in discoverers {
                    self.send_to(c, &event_on(&ch, method, params.clone()));
                }
            }
            // Upstream never has auto-attach enabled; ignore stray events.
            "Target.attachedToTarget" => {}
            _ => {
                let all: Vec<ClientId> = self.lock().clients.keys().copied().collect();
                for c in all {
                    self.send_to(c, &msg);
                }
            }
        }
    }

    fn on_upstream_reply(self: &Arc<Self>, id: u64, msg: Value) {
        let pending = self.lock().pending.remove(&id);
        match pending {
            None => {}
            Some(Pending::Internal(tx)) => {
                let _ = tx.send(msg);
            }
            Some(Pending::Client {
                client,
                id: client_id,
                method,
                target,
                session,
            }) => {
                let mut out = msg;
                out["id"] = client_id;
                let result = out.get("result").cloned().unwrap_or(Value::Null);
                if method == "Target.attachToTarget" || method == "Target.attachToBrowserTarget" {
                    if let Some(sid) = result.get("sessionId").and_then(Value::as_str) {
                        let browser = method == "Target.attachToBrowserTarget";
                        self.lock().sessions.insert(
                            sid.to_string(),
                            Session {
                                client: Some(client),
                                target: if browser {
                                    "browser".to_string()
                                } else {
                                    target.unwrap_or_default()
                                },
                                browser,
                                parent: session,
                            },
                        );
                    }
                } else if method == "Target.createTarget" {
                    if let Some(tid) = result.get("targetId").and_then(Value::as_str) {
                        // Chrome delivers auto-attach events before the
                        // createTarget reply; Playwright depends on that.
                        let mux = Arc::clone(self);
                        let tid = tid.to_string();
                        tokio::spawn(async move {
                            mux.ensure_target_ready(&tid).await;
                            mux.send_to(client, &out);
                        });
                        return;
                    }
                }
                self.send_to(client, &out);
            }
        }
    }

    /// Install the dialog shim and deliver auto-attach to every
    /// auto-attaching client that has not seen `target` yet. Serialized per
    /// target, so a concurrent caller returns only after the work is done.
    async fn ensure_target_ready(&self, target: &str) {
        let lock = {
            let mut st = self.lock();
            if st.closed {
                return;
            }
            Arc::clone(st.target_locks.entry(target.to_string()).or_default())
        };
        let _guard = lock.lock().await;

        let needs_info = self.lock().target_info(target).is_none();
        if needs_info {
            if let Ok(r) = self
                .call("Target.getTargetInfo", json!({ "targetId": target }), None)
                .await
            {
                if let Some(info) = r.get("targetInfo") {
                    self.lock().upsert_target(info);
                }
            }
        }
        let info = self.lock().target_info(target).cloned().unwrap_or_else(|| {
            json!({
                "targetId": target, "type": "page", "title": "", "url": "about:blank",
                "attached": true, "canAccessOpener": false, "browserContextId": "default",
            })
        });
        if !is_page(&info) {
            return;
        }

        if let Some(shim) = self.shim {
            let first = self.lock().shimmed.insert(target.to_string());
            if first {
                if let Err(e) = self.install_shim(target, shim).await {
                    tracing::warn!(target = "obscura", %e, page = target, "dialog shim not installed");
                }
            }
        }

        let clients: Vec<(ClientId, Channel)> = {
            let mut st = self.lock();
            st.clients
                .iter_mut()
                .filter(|(_, c)| c.auto_attach.is_some() && !c.attached.contains(target))
                .map(|(id, c)| {
                    c.attached.insert(target.to_string());
                    (*id, c.auto_attach.clone().flatten())
                })
                .collect()
        };
        for (client, channel) in clients {
            if let Err(e) = self.attach_for(client, &channel, target, &info).await {
                tracing::warn!(target = "obscura", %e, page = target, "auto-attach failed");
            }
        }
    }

    async fn install_shim(&self, target: &str, shim: &str) -> Result<()> {
        let r = self
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target, "flatten": true }),
                None,
            )
            .await?;
        let sid = r
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("attachToTarget returned no sessionId"))?
            .to_string();
        self.lock().sessions.insert(
            sid.clone(),
            Session {
                client: None,
                target: target.to_string(),
                browser: false,
                parent: None,
            },
        );
        self.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": shim }),
            Some(&sid),
        )
        .await?;
        self.call(
            "Runtime.evaluate",
            json!({ "expression": shim, "returnByValue": true }),
            Some(&sid),
        )
        .await?;
        Ok(())
    }

    async fn attach_for(
        &self,
        client: ClientId,
        channel: &Channel,
        target: &str,
        info: &Value,
    ) -> Result<()> {
        let r = self
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target, "flatten": true }),
                None,
            )
            .await?;
        let sid = r
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("attachToTarget returned no sessionId"))?
            .to_string();
        let still_here = {
            let mut st = self.lock();
            let here = st.clients.contains_key(&client);
            if here {
                st.sessions.insert(
                    sid.clone(),
                    Session {
                        client: Some(client),
                        target: target.to_string(),
                        browser: false,
                        parent: channel.clone(),
                    },
                );
            }
            here
        };
        if !still_here {
            let _ = self
                .call("Target.detachFromTarget", json!({ "sessionId": sid }), None)
                .await;
            return Ok(());
        }
        let mut info = info.clone();
        info["attached"] = json!(true);
        self.send_to(
            client,
            &event_on(
                channel,
                "Target.attachedToTarget",
                json!({ "sessionId": sid, "targetInfo": info, "waitingForDebugger": false }),
            ),
        );
        Ok(())
    }

    // -- clients -> upstream -------------------------------------------------

    /// Serve one downstream CDP client until it disconnects.
    pub async fn serve_client<S>(self: Arc<Self>, ws: WebSocketStream<S>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let client = {
            let mut st = self.lock();
            if st.closed {
                return;
            }
            st.next_client += 1;
            let id = st.next_client;
            st.clients.insert(
                id,
                Client {
                    tx,
                    auto_attach: None,
                    discover: None,
                    attached: HashSet::new(),
                },
            );
            id
        };
        let writer = tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if sink.send(Message::Text(text)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(Message::Text(text)) => self.on_client(client, &text).await,
                Ok(Message::Binary(bytes)) => {
                    if let Ok(text) = std::str::from_utf8(&bytes) {
                        self.on_client(client, text).await;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        self.remove_client(client);
        let _ = writer.await;
    }

    fn remove_client(&self, client: ClientId) {
        let sessions: Vec<String> = {
            let mut st = self.lock();
            st.clients.remove(&client);
            let owned: Vec<String> = st
                .sessions
                .iter()
                .filter(|(_, s)| s.client == Some(client))
                .map(|(sid, _)| sid.clone())
                .collect();
            for sid in &owned {
                st.sessions.remove(sid);
            }
            st.pending
                .retain(|_, p| !matches!(p, Pending::Client { client: c, .. } if *c == client));
            owned
        };
        // Detach upstream so Obscura can release per-session state. Pages
        // stay open, as they would in Chrome.
        for sid in sessions {
            let mut st = self.lock();
            st.next_up_id += 1;
            let id = st.next_up_id;
            let msg = json!({ "id": id, "method": "Target.detachFromTarget", "params": { "sessionId": sid } });
            let _ = self.up_tx.send(msg.to_string());
        }
    }

    async fn on_client(&self, client: ClientId, text: &str) {
        let Ok(mut msg) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let session = msg
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

        // Sessions are private to the client that created them.
        let mut target = params
            .get("targetId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut browser_level = session.is_none();
        if let Some(sid) = session.as_deref() {
            let owned = {
                let st = self.lock();
                st.sessions
                    .get(sid)
                    .filter(|s| s.client == Some(client))
                    .map(|s| (s.target.clone(), s.browser))
            };
            let Some((owned_target, is_browser)) = owned else {
                self.reply_error(
                    client,
                    &id,
                    Some(sid),
                    &format!("Session with given id not found: {sid}"),
                );
                return;
            };
            browser_level = is_browser;
            if !is_browser && method == "Target.getTargetInfo" && target.is_none() {
                msg["params"] = json!({ "targetId": owned_target });
                target = Some(owned_target);
            }
        }

        if browser_level {
            let channel: Channel = session.clone();
            match method.as_str() {
                "Target.setAutoAttach" => {
                    let on = params
                        .get("autoAttach")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let pages = {
                        let mut st = self.lock();
                        if let Some(c) = st.clients.get_mut(&client) {
                            c.auto_attach = on.then(|| channel.clone());
                        }
                        st.page_target_ids()
                    };
                    if on {
                        for page in pages {
                            self.ensure_target_ready(&page).await;
                        }
                    }
                    self.reply(client, &id, channel.as_deref(), json!({}));
                    return;
                }
                "Target.setDiscoverTargets" => {
                    let on = params
                        .get("discover")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let infos: Vec<Value> = {
                        let mut st = self.lock();
                        if let Some(c) = st.clients.get_mut(&client) {
                            c.discover = on.then(|| channel.clone());
                        }
                        st.targets.iter().map(|(_, info)| info.clone()).collect()
                    };
                    if on {
                        for info in infos {
                            self.send_to(
                                client,
                                &event_on(
                                    &channel,
                                    "Target.targetCreated",
                                    json!({ "targetInfo": info }),
                                ),
                            );
                        }
                    }
                    self.reply(client, &id, channel.as_deref(), json!({}));
                    return;
                }
                "Browser.close" | "Browser.crash" => {
                    self.reply(client, &id, channel.as_deref(), json!({}));
                    return;
                }
                _ => {}
            }
        }

        let mut st = self.lock();
        if st.closed {
            drop(st);
            self.reply_error(client, &id, session.as_deref(), "obscura connection closed");
            return;
        }
        st.next_up_id += 1;
        let up_id = st.next_up_id;
        st.pending.insert(
            up_id,
            Pending::Client {
                client,
                id,
                method,
                target,
                session,
            },
        );
        msg["id"] = json!(up_id);
        let _ = self.up_tx.send(msg.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_tungstenite::MaybeTlsStream;

    /// Minimal fake of Obscura's connection-scoped target model: one
    /// upstream connection, pages `page-N`, sessions `page-N-session-M`.
    async fn spawn_fake_obscura() -> (String, Arc<StdMutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(StdMutex::new(Vec::<Value>::new()));
        let log2 = Arc::clone(&log);
        tokio::spawn(async move {
            // Only the first connection matters (the mux's upstream).
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut rx) = ws.split();
            let pages = AtomicU64::new(0);
            let sessions = AtomicU64::new(0);
            let mut discover = false;
            while let Some(Ok(Message::Text(text))) = rx.next().await {
                let msg: Value = serde_json::from_str(&text).unwrap();
                log2.lock().unwrap().push(msg.clone());
                let id = msg["id"].clone();
                let method = msg["method"].as_str().unwrap_or("").to_string();
                let mut out = Vec::new();
                let result = match method.as_str() {
                    "Target.setDiscoverTargets" => {
                        discover = true;
                        json!({})
                    }
                    "Target.getTargets" => json!({ "targetInfos": [] }),
                    "Target.createTarget" => {
                        let n = pages.fetch_add(1, Ordering::SeqCst) + 1;
                        let tid = format!("page-{n}");
                        if discover {
                            out.push(json!({ "method": "Target.targetCreated", "params": { "targetInfo": {
                                "targetId": tid, "type": "page", "url": "about:blank", "title": "", "attached": false, "browserContextId": "default" } } }));
                        }
                        json!({ "targetId": tid })
                    }
                    "Target.attachToTarget" => {
                        let n = sessions.fetch_add(1, Ordering::SeqCst) + 1;
                        json!({ "sessionId": format!("{}-session-{n}", msg["params"]["targetId"].as_str().unwrap()) })
                    }
                    "Target.attachToBrowserTarget" => {
                        let n = sessions.fetch_add(1, Ordering::SeqCst) + 1;
                        json!({ "sessionId": format!("browser-{n}") })
                    }
                    "Target.getTargetInfo" => json!({ "targetInfo": {
                        "targetId": msg["params"]["targetId"].as_str().unwrap_or("browser"), "type": "page" } }),
                    _ => json!({}),
                };
                let mut reply = json!({ "id": id, "result": result });
                if let Some(s) = msg.get("sessionId") {
                    reply["sessionId"] = s.clone();
                }
                // Events first, then the reply (Obscura's order).
                for ev in out {
                    sink.send(Message::Text(ev.to_string())).await.unwrap();
                }
                sink.send(Message::Text(reply.to_string())).await.unwrap();
                if method == "Runtime.evaluate" {
                    if let Some(s) = msg.get("sessionId") {
                        let ev = json!({ "method": "Runtime.consoleAPICalled", "sessionId": s, "params": {} });
                        sink.send(Message::Text(ev.to_string())).await.unwrap();
                    }
                }
            }
        });
        (format!("ws://{addr}/devtools/browser"), log)
    }

    /// Spin up a mux over the fake plus a downstream listener; return a
    /// connector for downstream clients.
    async fn mux_fixture(shim: Option<&'static str>) -> (String, Arc<StdMutex<Vec<Value>>>) {
        let (up, log) = spawn_fake_obscura().await;
        let (mux, _done) = Mux::connect(&up, shim).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                tokio::spawn(Arc::clone(&mux).serve_client(ws));
            }
        });
        (format!("ws://{addr}/devtools/browser"), log)
    }

    struct TestClient {
        ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
        next: u64,
        backlog: Vec<Value>,
    }

    impl TestClient {
        async fn connect(url: &str) -> Self {
            let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
            Self {
                ws,
                next: 0,
                backlog: Vec::new(),
            }
        }

        async fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> Value {
            self.next += 1;
            let id = self.next;
            let mut msg = json!({ "id": id, "method": method, "params": params });
            if let Some(s) = session {
                msg["sessionId"] = json!(s);
            }
            self.ws.send(Message::Text(msg.to_string())).await.unwrap();
            loop {
                let frame = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                    .await
                    .expect("reply timeout")
                    .unwrap()
                    .unwrap();
                let v: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    return v;
                }
                self.backlog.push(v);
            }
        }

        fn events(&self, method: &str) -> Vec<&Value> {
            self.backlog
                .iter()
                .filter(|v| v.get("method").and_then(Value::as_str) == Some(method))
                .collect()
        }
    }

    #[tokio::test]
    async fn pages_created_by_one_client_are_visible_to_auto_attaching_client() {
        let (url, _log) = mux_fixture(None).await;
        let mut pw = TestClient::connect(&url).await;
        let r = pw
            .send(
                "Target.setAutoAttach",
                json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
                None,
            )
            .await;
        assert_eq!(r["result"], json!({}));

        let mut native = TestClient::connect(&url).await;
        let created = native
            .send("Target.createTarget", json!({ "url": "about:blank" }), None)
            .await;
        assert_eq!(created["result"]["targetId"], "page-1");

        // The auto-attach event reached the other client; its own request
        // (any) flushes it into the backlog.
        let _ = pw.send("Browser.getVersion", json!({}), None).await;
        let attached = pw.events("Target.attachedToTarget");
        assert_eq!(attached.len(), 1, "backlog: {:?}", pw.backlog);
        assert_eq!(attached[0]["params"]["targetInfo"]["targetId"], "page-1");
        assert_eq!(attached[0]["params"]["waitingForDebugger"], false);
        // The creator did not auto-attach, so it got no session.
        assert!(native.events("Target.attachedToTarget").is_empty());
    }

    #[tokio::test]
    async fn create_target_reply_follows_creators_own_auto_attach() {
        let (url, _log) = mux_fixture(None).await;
        let mut pw = TestClient::connect(&url).await;
        pw.send(
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "flatten": true }),
            None,
        )
        .await;
        let created = pw
            .send("Target.createTarget", json!({ "url": "about:blank" }), None)
            .await;
        let tid = created["result"]["targetId"].as_str().unwrap().to_string();
        // Already in the backlog before the reply arrived.
        let attached = pw.events("Target.attachedToTarget");
        assert_eq!(attached.len(), 1);
        assert_eq!(attached[0]["params"]["targetInfo"]["targetId"], tid);
    }

    #[tokio::test]
    async fn sessions_are_owned_by_their_client() {
        let (url, _log) = mux_fixture(None).await;
        let mut a = TestClient::connect(&url).await;
        let mut b = TestClient::connect(&url).await;
        let tid = a.send("Target.createTarget", json!({}), None).await["result"]["targetId"]
            .as_str()
            .unwrap()
            .to_string();
        let sid = a
            .send(
                "Target.attachToTarget",
                json!({ "targetId": tid, "flatten": true }),
                None,
            )
            .await["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        let ok = a
            .send("Runtime.evaluate", json!({ "expression": "1" }), Some(&sid))
            .await;
        assert!(ok.get("error").is_none(), "{ok}");
        assert_eq!(ok["sessionId"], json!(sid));
        let denied = b
            .send("Runtime.evaluate", json!({ "expression": "1" }), Some(&sid))
            .await;
        assert!(denied.get("error").is_some(), "{denied}");
        // Session events go to the owner only.
        let _ = b.send("Browser.getVersion", json!({}), None).await;
        assert!(b.events("Runtime.consoleAPICalled").is_empty());
        let _ = a.send("Browser.getVersion", json!({}), None).await;
        assert_eq!(a.events("Runtime.consoleAPICalled").len(), 1);
    }

    #[tokio::test]
    async fn get_target_info_on_session_is_pinned_to_its_target() {
        let (url, log) = mux_fixture(None).await;
        let mut a = TestClient::connect(&url).await;
        let tid = a.send("Target.createTarget", json!({}), None).await["result"]["targetId"]
            .as_str()
            .unwrap()
            .to_string();
        let sid = a
            .send(
                "Target.attachToTarget",
                json!({ "targetId": tid, "flatten": true }),
                None,
            )
            .await["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        let info = a.send("Target.getTargetInfo", json!({}), Some(&sid)).await;
        assert_eq!(info["result"]["targetInfo"]["targetId"], json!(tid));
        let forwarded = log
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["method"] == "Target.getTargetInfo" && m.get("sessionId").is_some())
            .cloned()
            .unwrap();
        assert_eq!(forwarded["params"]["targetId"], json!(tid));
    }

    #[tokio::test]
    async fn browser_close_and_auto_attach_are_not_forwarded() {
        let (url, log) = mux_fixture(None).await;
        let mut a = TestClient::connect(&url).await;
        a.send("Target.setAutoAttach", json!({ "autoAttach": true }), None)
            .await;
        let r = a.send("Browser.close", json!({}), None).await;
        assert_eq!(r["result"], json!({}));
        let methods: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .map(|m| m["method"].as_str().unwrap().to_string())
            .collect();
        assert!(!methods.iter().any(|m| m == "Browser.close"), "{methods:?}");
        assert!(
            !methods.iter().any(|m| m == "Target.setAutoAttach"),
            "{methods:?}"
        );
    }

    #[tokio::test]
    async fn shim_is_installed_on_new_pages() {
        let (url, log) = mux_fixture(Some("window.__shim = 1")).await;
        let mut a = TestClient::connect(&url).await;
        a.send("Target.createTarget", json!({}), None).await;
        let methods: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .map(|m| m["method"].as_str().unwrap().to_string())
            .collect();
        assert!(
            methods
                .iter()
                .any(|m| m == "Page.addScriptToEvaluateOnNewDocument"),
            "{methods:?}"
        );
    }

    #[tokio::test]
    async fn browser_session_gets_browser_level_auto_attach() {
        // Playwright's connectOverCDP drives the Target domain through a
        // browser session from Target.attachToBrowserTarget.
        let (url, log) = mux_fixture(None).await;
        let mut pw = TestClient::connect(&url).await;
        let bs = pw
            .send("Target.attachToBrowserTarget", json!({}), None)
            .await["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        let r = pw
            .send(
                "Target.setAutoAttach",
                json!({ "autoAttach": true, "flatten": true }),
                Some(&bs),
            )
            .await;
        assert_eq!(r["sessionId"], json!(bs));
        let mut native = TestClient::connect(&url).await;
        native.send("Target.createTarget", json!({}), None).await;
        let _ = pw.send("Browser.getVersion", json!({}), Some(&bs)).await;
        let attached = pw.events("Target.attachedToTarget");
        assert_eq!(attached.len(), 1, "backlog: {:?}", pw.backlog);
        assert_eq!(attached[0]["sessionId"], json!(bs));
        let methods: Vec<String> = log
            .lock()
            .unwrap()
            .iter()
            .map(|m| m["method"].as_str().unwrap().to_string())
            .collect();
        assert!(
            !methods.iter().any(|m| m == "Target.setAutoAttach"),
            "{methods:?}"
        );
    }

    #[tokio::test]
    async fn discovery_replays_existing_targets() {
        let (url, _log) = mux_fixture(None).await;
        let mut a = TestClient::connect(&url).await;
        a.send("Target.createTarget", json!({}), None).await;
        let mut b = TestClient::connect(&url).await;
        b.send(
            "Target.setDiscoverTargets",
            json!({ "discover": true }),
            None,
        )
        .await;
        assert_eq!(b.events("Target.targetCreated").len(), 1);
    }
}
