//! One connection's worth of state, as a page holds it.
//!
//! The page owns a socket and nothing else. This owns everything that
//! decides: the engine client, the framing, the provisioning plan and the
//! reassembly of chunked replies.
//!
//! # What the page does, and all it does
//!
//! 1. Open a socket to [`Session::url`].
//! 2. Send whatever [`Session::outbound`] hands it; say how much went with
//!    [`Session::sent`].
//! 3. Feed every message that arrives to [`Session::on_inbound`].
//! 4. Call [`Session::tick`] on a timer.
//!
//! No branch in that list. Every decision — what to provision next, whether an
//! ack answers the step in flight, when a write has waited too long, whether a
//! reply is usable at all — is in Rust, where it is tested against a transport
//! that reorders, duplicates and drops, and against a node.

use craftworks_sdk::{CachedStore, DbError, SystemEnv};
use wasm_bindgen::prelude::*;
use wire::provision::{Provisioner, Step};
use wire::{AckKind, DelegateKey, Incoming, Reassembler};

/// The artefacts a page provisions with, as bytes it fetched.
///
/// **The DEVELOPMENT path.** The long-term shape is both fetched from the
/// network by hash (§19, sdk#5); shipping them beside the wasm is how a page
/// can do it today.
struct Artefacts {
    delegate: Vec<u8>,
    block: Vec<u8>,
    register: Vec<u8>,
}

/// Everything one page-to-node connection needs.
#[wasm_bindgen]
pub struct Session {
    /// The database AND the store it reads through. One object, because a
    /// page has one connection and one tree: `Db` owns its store, and the
    /// pump reaches it through `store_mut()` rather than through a second
    /// handle that could drift out of step with it.
    db: craftworks_sdk::Db<CachedStore, SystemEnv>,
    frames: Reassembler,
    plan: Provisioner,
    /// Frames waiting to go out. WIRE frames, already enveloped — the page
    /// sends bytes and never learns what a `ClientRequest` is.
    out: Vec<Vec<u8>>,
    /// The delegate this session talks to, once its code is on hand.
    ///
    /// DERIVED from the code, never carried beside it: a key that named a
    /// different build would address requests to a delegate this session
    /// never registered.
    delegate: Option<DelegateKey>,
    artefacts: Option<Artefacts>,
    /// A counter, so each chunked request gets its own stream and two
    /// concurrent ones cannot be reassembled into each other.
    stream: u32,
    port: u16,
    /// Steps completed, for the page to show. Drained by `take_progress`.
    progress: Vec<(Step, bool)>,
    /// Messages this build could not use, by reason.
    unusable: Vec<String>,
    /// How many of the plan's completed steps have been reported.
    reported: usize,
    /// Ranges asked for and not yet answered. The bookkeeping lives in the
    /// SDK, not here, so it can be tested on a machine rather than only in a
    /// tab — which is what the first version of this recovery could not be.
    loads: craftworks_sdk::Loads,
    /// The head's contract instance id, once `Identity` has named it.
    ///
    /// Zero until then, and zero for ever on a delegate that is not
    /// provisioned — there is no head to subscribe to.
    head_id: [u8; 32],
    /// The head's id as the node NAMES it, so a notification can be matched.
    head_named: String,
    /// Head notifications for a contract this session is not watching.
    ///
    /// COUNTED, not described. The node chooses what it sends, and a page
    /// that reacted to any of them would reload on somebody else's contract.
    /// A count is also the thing that says whether it is happening at all.
    foreign_notifications: usize,
    /// Whether this connection has ASKED the node to watch the head.
    ///
    /// Reset on reconnect, not remembered: the node's copy of a subscription
    /// outlives the engine's context and can be evicted at its cap without
    /// anyone being told (F39), so a new connection asks again. Re-asking is
    /// idempotent at the node, so it costs nothing when nothing was lost.
    subscribed: bool,
    /// Whether the node ACCEPTED it.
    ///
    /// Distinct from having asked, and the distinction is the whole point: a
    /// page that believed it was being notified while it was actually
    /// polling is the failure `LiveMode` exists to make impossible.
    watching: bool,
    /// Domains this page has bound, so a head move can name what is stale.
    bound: std::collections::BTreeSet<String>,
    /// When the subscribe request went out, so an unanswered one does not sit
    /// at "asked" for ever.
    asked_at_ms: u64,
    /// The node refused to watch the head, in its own words. Display only.
    watch_refused: String,
    /// The head moved. Set by a notification, drained by the page.
    ///
    /// A HINT and never an authority: a fabricated one costs a reload, and a
    /// reload can change nothing that the data does not verify. A SUPPRESSED
    /// one costs nothing at all, because the tick re-reads the root anyway —
    /// which is what makes being told an accelerator rather than the
    /// mechanism.
    head_moved: bool,
    /// The root the engine last reported standing on.
    ///
    /// A page is recorded against the root it was read at, and a page
    /// recorded against the wrong root would make a stale range look current.
    /// Zero until `Identity` has answered, and a load that completes before
    /// then is still recorded against zero — which is what a brand-new engine
    /// actually stands on.
    head_root: [u8; 32],
}

#[wasm_bindgen]
impl Session {
    /// A session against a node on THIS machine.
    ///
    /// Loopback only, and `wire` refuses anything else: provisioning installs
    /// code and hands over a signing key, and the acks it rests on are that
    /// node's own word about its own store.
    #[wasm_bindgen(constructor)]
    pub fn new(port: u16) -> Result<Session, JsError> {
        let mut device = [0u8; 4];
        let _ = getrandom::getrandom(&mut device);
        // Built here so the URL's `encodingProtocol=native` cannot be lost by
        // a page assembling its own: the node falls back to a different
        // encoding without it and says nothing that names the cause.
        wire::ws_url("127.0.0.1", port).map_err(|e| JsError::new(&e))?;
        Ok(Session {
            db: craftworks_sdk::Db::new(
                CachedStore::new(Box::new(crate::js_now_ms)),
                SystemEnv,
                device,
            ),
            frames: Reassembler::new(),
            plan: Provisioner::new(),
            out: Vec::new(),
            delegate: None,
            artefacts: None,
            stream: 1,
            port,
            progress: Vec::new(),
            unusable: Vec::new(),
            reported: 0,
            loads: craftworks_sdk::Loads::new(),
            head_root: [0u8; 32],
            head_id: [0u8; 32],
            head_named: String::new(),
            subscribed: false,
            foreign_notifications: 0,
            watching: false,
            head_moved: false,
            bound: std::collections::BTreeSet::new(),
            asked_at_ms: 0,
            watch_refused: String::new(),
        })
    }

    /// The websocket URL to open. Built in Rust; see the constructor.
    pub fn url(&self) -> String {
        wire::ws_url("127.0.0.1", self.port).unwrap_or_default()
    }

    /// Frames waiting to be sent, oldest first — WITHOUT giving them up.
    ///
    /// A socket's send can fail, so the page looks, sends what it can, and
    /// says how many with `sent`. Draining here would lose whatever could not
    /// go — and a write made before the socket opens is the ordinary start of
    /// every session.
    pub fn outbound(&mut self) -> Vec<js_sys::Uint8Array> {
        self.advance();
        self.watch_head();
        self.envelope_engine_requests();
        self.out
            .iter()
            .map(|b| js_sys::Uint8Array::from(&b[..]))
            .collect()
    }

    /// The first `n` of `outbound()` went out.
    pub fn sent(&mut self, n: usize) {
        self.out.drain(..n.min(self.out.len()));
    }

    /// A message arrived from the node.
    ///
    /// Everything it can mean is decided here: an engine reply, a head that
    /// moved, an ack for the step being provisioned, a refusal, a chunk of
    /// something larger, or something this build cannot use — which is
    /// counted rather than ignored.
    pub fn on_inbound(&mut self, bytes: &[u8]) {
        match wire::unframe(&mut self.frames, bytes) {
            Incoming::EngineBytes(msgs) => {
                for m in msgs {
                    // Read, not intercepted: the store still gets every byte.
                    // `Identity` is the only reply provisioning rests on, and
                    // it is the delegate's own report about its own secret
                    // store rather than an acknowledgement that a message
                    // arrived.
                    match protocol::decode_reply(&m) {
                        Ok(protocol::Reply::Identity {
                            head_writable,
                            head_root,
                            head_id,
                            ..
                        }) => {
                            // Which contract the head IS. Without it a tab
                            // that made no write can only poll: an
                            // engine-originated push returns to whoever
                            // invoked the delegate (F40).
                            self.head_id = head_id;
                            self.head_named = if head_id == [0u8; 32] {
                                String::new()
                            } else {
                                wire::contract_id(head_id).to_string()
                            };
                            // The root pages are recorded against. A page
                            // filed under the wrong root would make a stale
                            // range look current.
                            self.head_root = head_root;
                            self.plan.on_identity(head_writable);
                            self.note_progress();
                        }
                        // Another writer got there first. The ordinary
                        // outcome of two tabs opened together, not an error:
                        // the plan goes on to the confirming Ask and simply
                        // stops claiming an install it did not make.
                        Ok(protocol::Reply::AlreadyInstalled) => {
                            self.plan.on_already_installed();
                        }
                        Ok(protocol::Reply::Page {
                            req_id,
                            entries,
                            cursor,
                            ..
                        }) => self.on_page(req_id, entries, cursor),
                        // A read the engine could not answer. The range is
                        // NOT recorded as loaded: an empty page here would
                        // say "this range is empty", which is a wrong answer
                        // wearing the shape of a right one.
                        Ok(protocol::Reply::Unavailable { req_id, .. }) => {
                            self.loads.on_unavailable(req_id)
                        }
                        _ => {}
                    }
                    self.db.store_mut().on_inbound(&m);
                }
            }
            Incoming::Ack(kind) => self.on_ack(kind),
            Incoming::Refused(why) => {
                // The node's wording, kept for display. Nothing branches on
                // it: the node chose it, and a reason that steers control flow
                // is an input from a stranger.
                self.plan.on_refused(&why.said);
            }
            Incoming::HeadChanged { key } => {
                // ONLY for the head this session asked to watch.
                //
                // A notification names a contract, and the node chooses what
                // it sends. Acting on any of them would let one unasked-for
                // message make a page reload for ever. This is still a HINT
                // even when it matches — what it triggers is the reload a
                // tick would do anyway — but a hint about somebody else's
                // contract is not even that.
                // Compared as the node NAMES it. The engine reports a head
                // as 32 bytes and the client API names contracts as strings;
                // rendering ours the same way is the only comparison that is
                // about the same thing.
                if self.subscribed && !self.head_named.is_empty() && key == self.head_named {
                    self.head_moved = true;
                } else {
                    self.foreign_notifications += 1;
                }
            }
            Incoming::Unusable(why) => self.unusable.push(format!("{why:?}")),
            Incoming::Partial => {}
        }
    }

    fn on_ack(&mut self, kind: AckKind) {
        // The subscribe ack is matched by the KEY IT NAMES, never by
        // position. Both acks arrive on one connection with no correlation
        // id, so pairing by order would let a delegate registration confirm
        // a subscription that was never accepted — which is harness#38's
        // shape, and it has already been made once in this file.
        if let AckKind::Subscribed(key) = &kind {
            if *key == self.head_named && !self.head_named.is_empty() {
                self.watching = true;
            } else {
                self.foreign_notifications += 1;
            }
            return;
        }
        self.plan.on_ack(&kind);
        self.note_progress();
    }

    /// Record any steps the plan completed since this was last asked.
    ///
    /// Counted from the plan's own list rather than from the call that might
    /// have completed one: `on_identity` can finish two steps at once (it
    /// proves the delegate is registered AND reports the install), and a
    /// caller that pushed "the last one" would show one of them.
    fn note_progress(&mut self) {
        let done = self.plan.result().steps.len();
        while self.reported < done {
            let (step, _) = self.plan.result().steps[self.reported];
            self.progress.push((step, true));
            self.reported += 1;
        }
    }

    /// A read's answer, with a `NotLoaded` turned into a real request and a
    /// ticket to wait on.
    fn answer<T: serde::Serialize>(&mut self, r: Result<T, DbError>) -> Result<String, JsValue> {
        match r {
            Ok(v) => {
                // The chain this read was walking is finished; the next read
                // starts its own.
                self.loads.read_succeeded();
                serde_json::to_string(&v).map_err(|e| db_err(&DbError::Refused(e.to_string())))
            }
            Err(e) => Err(self.park(e)),
        }
    }

    /// Queue the load a `NotLoaded` needs, and return the error with its
    /// ticket on it.
    ///
    /// **This is the whole of the recovery, and it used to be a comment.**
    /// The first version of the wrapper answered a `NotLoaded` by awaiting a
    /// resolved promise and asking again — which asks again in a microtask,
    /// before any websocket message can possibly have arrived, and after
    /// having requested nothing at all. Every read outside a preload manifest
    /// rejected, always, and the test passed because its fake scripted the
    /// second call to succeed.
    fn park(&mut self, e: DbError) -> JsValue {
        let Some((lo, hi)) = e.needs() else {
            return db_err(&e);
        };
        let (lo, hi) = (lo.to_vec(), hi.to_vec());

        let Some((req_id, send)) = self.loads.want(&lo, &hi, crate::js_now_ms()) else {
            // This span was loaded already and the read still cannot be
            // answered. Loading it again would answer exactly as it did the
            // first time, so the caller is told instead of sent round.
            return db_err(&e);
        };
        if send {
            // A FULL PAGE, by the shared constant. `0` reads like "no
            // limit" and is not one: the shell clamps it to ONE entry, so a
            // range of N rows would load in N round trips of a single row.
            // Paging is still followed to the end, because the copy records
            // `[lo, hi)` as loaded and that is only true once the range is
            // exhausted — this decides how many trips that takes.
            self.db
                .store_mut()
                .client
                .send(&craftworks_sdk::Loads::range_request(
                    req_id, &lo, &hi, None,
                ));
        }
        db_err_waiting(&e, Some(req_id))
    }

    /// A page of a load arrived.
    fn on_page(&mut self, req_id: u64, entries: Vec<(Vec<u8>, Vec<u8>)>, cursor: Option<Vec<u8>>) {
        match self.loads.on_page(req_id, entries, cursor) {
            craftworks_sdk::loads::Page::More { lo, hi, after } => {
                // Not exhausted. Ask for the rest under the SAME ticket, so
                // the read parked on it waits for the whole range rather than
                // being woken by a part of it.
                self.db
                    .store_mut()
                    .client
                    .send(&craftworks_sdk::Loads::range_request(
                        req_id,
                        &lo,
                        &hi,
                        Some(after),
                    ));
            }
            craftworks_sdk::loads::Page::Complete { lo, hi, rows } => {
                let root = self.head_root;
                self.db.store_mut().on_page(&lo, &hi, rows, root);
            }
            craftworks_sdk::loads::Page::Nothing => {}
        }
    }

    /// Loads that ended since this was last asked, as JSON.
    ///
    /// The page resolves its parked reads from THIS, called when a message
    /// arrives. Never a timer: a timer either spins or answers late, and
    /// neither of those is a fact about the data.
    pub fn take_loads(&mut self) -> String {
        let out: Vec<serde_json::Value> = self
            .loads
            .take_ended()
            .into_iter()
            .map(|(id, how)| {
                let ok = how == craftworks_sdk::Ended::Loaded;
                serde_json::json!({
                    "id": id,
                    "ok": ok,
                    "code": if ok { "LOADED" } else { "UNAVAILABLE" },
                })
            })
            .collect();
        serde_json::to_string(&out).unwrap_or_else(|_| "[]".into())
    }

    /// How many ranges are being loaded right now. For a page to show, and
    /// for a test to assert that concurrent reads of one range issue ONE
    /// request rather than one each.
    pub fn loads_in_flight(&self) -> usize {
        self.loads.in_flight()
    }

    /// Ask the node to tell us when the head moves.
    ///
    /// Once per connection, and only once there IS a head: an unprovisioned
    /// delegate has no Register to name.
    fn watch_head(&mut self) {
        if self.subscribed || self.head_id == [0u8; 32] || !self.plan.provisioned() {
            return;
        }
        let id = wire::contract_id(self.head_id);
        let stream = self.next_stream();
        match wire::frame_subscribe(id, stream) {
            Ok(frames) => {
                self.out.extend(frames);
                self.subscribed = true;
                self.asked_at_ms = crate::js_now_ms();
            }
            Err(e) => self
                .unusable
                .push(format!("could not ask to watch the head: {e}")),
        }
    }

    /// Which bound domains are stale, because the head moved. Drains.
    ///
    /// **Rust decides which, not the page.** A page that reloaded
    /// "everything" on every notification would turn one write anywhere into
    /// a full refetch of every screen; one that guessed would miss the domain
    /// that changed. The session knows which domains have been bound.
    ///
    /// The head moving is a HINT. A missed notification costs nothing — the
    /// tick re-reads the root regardless — and a spurious one costs a reload.
    /// What it is NOT is a root: nothing here goes into the copy.
    pub fn take_stale(&mut self) -> String {
        if !std::mem::take(&mut self.head_moved) {
            return "[]".into();
        }
        let stale: Vec<&String> = self.bound.iter().collect();
        serde_json::to_string(&stale).unwrap_or_else(|_| "[]".into())
    }

    /// A domain this page is showing, so a head move can name it.
    ///
    /// Recorded by the session rather than tracked in JS, because deciding
    /// what to reload is a decision.
    pub fn bind(&mut self, domain: &str) {
        self.bound.insert(domain.to_string());
    }

    pub fn unbind(&mut self, domain: &str) {
        self.bound.remove(domain);
    }

    /// How this session actually finds out that the head moved.
    ///
    /// REPORTED, never assumed. `HeadSubscribed` only after the node has
    /// ACCEPTED the subscription — asking is not being answered — and
    /// `Polled` says, in words, why it is not: the LIVE switch in a builder
    /// shows which one a component really has, so a binding that silently
    /// fell back to polling cannot look like one that did not.
    pub fn live_mode(&self) -> String {
        let waited = crate::js_now_ms().saturating_sub(self.asked_at_ms);
        let tick = " The tick keeps the data right meanwhile.";
        let (mode, why) = if self.watching {
            ("HeadSubscribed", String::new())
        } else if !self.watch_refused.is_empty() {
            let said = &self.watch_refused;
            (
                "Polled",
                format!("the node refused to watch the head: {said}.{tick}"),
            )
        } else if !self.plan.provisioned() {
            (
                "Polled",
                "this node is not provisioned, so there is no head to watch".to_string(),
            )
        } else if self.head_id == [0u8; 32] {
            (
                "Polled",
                "the engine has not named a head contract yet".to_string(),
            )
        } else if self.subscribed && waited > WATCH_ANSWER_MS {
            // ASKED AND NEVER ANSWERED. Without this it sits at "asked" for
            // ever and a page shows a subscription it does not have.
            (
                "Polled",
                format!("asked to watch the head and the node never answered.{tick}"),
            )
        } else if self.subscribed {
            (
                "Polled",
                "the node has not accepted the subscription yet".to_string(),
            )
        } else {
            (
                "Polled",
                "no subscription has been asked for on this connection".to_string(),
            )
        };
        serde_json::json!({
            "mode": mode,
            "why": why,
            "foreignNotifications": self.foreign_notifications,
        })
        .to_string()
    }

    /// Take the next provisioning step, if there is one and nothing is in
    /// flight. The frames go on the outbound queue; nothing is sent here.
    fn advance(&mut self) {
        let Some(step) = self.plan.next_step() else {
            return;
        };
        if self.artefacts.is_none() {
            // Nothing to provision WITH. Not an error: a page that only
            // reads an already-provisioned node never calls `provision`.
            return;
        }
        let now = crate::js_now_ms();
        let stream = self.next_stream();

        let framed = match step {
            Step::Delegate => {
                let code = self
                    .artefacts
                    .as_ref()
                    .expect("checked above")
                    .delegate
                    .clone();
                let (container, key) = wire::delegate_from_code(&code);
                let named = key.to_string();
                self.delegate = Some(key);
                wire::frame_register_delegate(container, stream).map(|f| (f, named))
            }
            Step::Ask => self
                .engine_frames(
                    protocol::encode_request(1, &protocol::Request::Identity),
                    stream,
                )
                .map(|f| (f, String::new())),
            Step::Install => {
                // A TEST key, minted here and then FORGOTTEN. The page keeps
                // no copy: on every later open it asks the delegate, which is
                // what makes "close the tab, reopen, the data is there" true
                // without a browser holding a key at all. Real keys are
                // sdk#14 — a passkey-derived device key that never leaves
                // keycraft — and nothing here may be reused as that path.
                let mut seed = [0u8; 32];
                if getrandom::getrandom(&mut seed).is_err() {
                    // No randomness is a refusal, never a fixed key: a
                    // predictable signing key is one anybody can forge a head
                    // with.
                    self.unusable.push("no randomness to mint a key".into());
                    return;
                }
                let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
                let vk = sk.verifying_key().to_bytes();
                let art = self.artefacts.as_ref().expect("checked above");
                let (block_code, register_code) = (art.block.clone(), art.register.clone());
                let req = protocol::Request::Install {
                    block_code,
                    register_code,
                    register_params: wire::register_params(&vk, wire::HEAD_NAME),
                    signing_key: protocol::TestKey(sk.to_bytes().to_vec()),
                };
                self.engine_frames(protocol::encode_request(1, &req), stream)
                    .map(|f| (f, String::new()))
            }
        };

        match framed {
            Ok((frames, named)) => {
                self.out.extend(frames);
                self.plan.sent(step, &named, now);
            }
            // Framing failed on THIS build's own bytes, so re-sending would
            // fail the same way. Recorded, and the step is not marked sent.
            Err(e) => self.unusable.push(format!("could not frame {step:?}: {e}")),
        }
    }

    /// Envelope whatever the engine has queued, once the delegate is known.
    ///
    /// Until then the engine KEEPS them. An earlier version drained the
    /// engine here and dropped what it could not frame — the same lose-the-
    /// queue defect already fixed once in the page's socket pump, where a
    /// closed socket silently discarded every request behind the first.
    fn envelope_engine_requests(&mut self) {
        let Some(key) = self.delegate.clone() else {
            return;
        };
        for bytes in self.db.store_mut().take_outbound() {
            let stream = self.next_stream();
            match wire::frame_engine_request(&key, bytes, stream) {
                Ok(frames) => self.out.extend(frames),
                Err(e) => self
                    .unusable
                    .push(format!("could not frame a request: {e}")),
            }
        }
    }

    fn engine_frames(&mut self, payload: Vec<u8>, stream: u32) -> Result<Vec<Vec<u8>>, String> {
        let key = self
            .delegate
            .clone()
            .ok_or_else(|| "no delegate to address".to_string())?;
        wire::frame_engine_request(&key, payload, stream)
    }

    fn next_stream(&mut self) -> u32 {
        self.stream = self.stream.wrapping_add(1).max(1);
        self.stream
    }

    /// Steps that completed since this was last asked, as JSON.
    pub fn take_progress(&mut self) -> String {
        let done: Vec<String> = self
            .progress
            .drain(..)
            .map(|(s, _)| format!("{s:?}"))
            .collect();
        serde_json::to_string(&done).unwrap_or_else(|_| "[]".into())
    }

    /// Messages this build could not use, by reason.
    pub fn unusable(&self) -> String {
        serde_json::to_string(&self.unusable).unwrap_or_else(|_| "[]".into())
    }

    /// The artefacts to provision with, as the page fetched them.
    ///
    /// Handing them in is what starts provisioning. A page that only reads an
    /// already-provisioned node never calls this and never sends a byte of
    /// contract code.
    pub fn provision(&mut self, delegate: Vec<u8>, block: Vec<u8>, register: Vec<u8>) {
        self.artefacts = Some(Artefacts {
            delegate,
            block,
            register,
        });
    }

    /// Has provisioning finished — because the DELEGATE said so?
    ///
    /// Not "every step was sent": an unprovisioned delegate answers
    /// `Identity` exactly like a healthy empty one while dropping every head
    /// it is given, so the only honest answer comes from asking it.
    pub fn provisioned(&self) -> bool {
        self.plan.provisioned()
    }

    /// Everything was sent, accepted, and the delegate still cannot write a
    /// head. A different fact from stalled and from refused, and the page
    /// says so rather than spinning.
    pub fn exhausted(&self) -> bool {
        self.plan.exhausted()
    }

    /// The step that stalled, if one has, as a string — or empty.
    ///
    /// A page shows this. Without it a step nobody answers leaves the screen
    /// saying nothing at all, which is indistinguishable from slow.
    pub fn stalled(&self) -> String {
        self.plan
            .stalled()
            .map(|s| format!("{s:?}"))
            .unwrap_or_default()
    }

    /// Why provisioning stopped, in the node's own words — or empty.
    pub fn refused(&self) -> String {
        self.plan
            .refused()
            .map(|(s, w)| format!("{s:?}: {w}"))
            .unwrap_or_default()
    }

    /// The socket dropped and a new one opened.
    ///
    /// **Reassembly is reset here and nowhere else.** A chunked reply that was
    /// half-received when the socket went is never completed: the stream ids
    /// restart with the new connection, so the tail of a message from before
    /// the drop would be joined to the head of one from after it and decoded
    /// as a single reply. Dropping the partial loses a message the node will
    /// answer again; keeping it would silently make one up.
    ///
    /// The step in flight is re-issued by `tick`, not here, because a reply
    /// may have been sent before the socket dropped and arrive on the new one.
    pub fn reconnected(&mut self) {
        self.frames.reset();
        // The node's copy of a subscription outlives the engine's context and
        // can be evicted at its cap without anyone being told (F39), so this
        // connection asks again. Idempotent at the node, so it costs nothing
        // when nothing was lost.
        self.subscribed = false;
        self.watching = false;
    }

    // ---- the data surface -------------------------------------------
    //
    // The SAME method names the in-memory `Db` has, so "Publish switches the
    // backend and the app code does not change" is a fact rather than an
    // intention. `tests/surfaces_agree.rs` fails if either side grows a
    // method the other lacks.
    //
    // Errors cross as `{ code, message }` with a code from a FIXED list.
    // An app must be able to tell "read me again" from "you are wrong", and
    // the only alternative to a code is matching on the text of a message —
    // which breaks the first time anybody rewords it, and breaks silently.
    // Nothing downstream branches on the message.

    pub fn define(&mut self, domain: &str, schema: &str) -> Result<(), JsValue> {
        let s: craftworks_sdk::Schema =
            serde_json::from_str(schema).map_err(|e| db_err(&DbError::Refused(e.to_string())))?;
        self.db.define(domain, &s).map_err(|e| db_err(&e))
    }

    pub fn schema(&mut self, domain: &str) -> Result<String, JsValue> {
        let r = self.db.schema(domain);
        self.answer(r)
    }

    pub fn domains(&mut self) -> Result<String, JsValue> {
        let r = self.db.domains();
        self.answer(r)
    }

    pub fn put(&mut self, domain: &str, fields: &str) -> Result<String, JsValue> {
        let f = fields_of(fields)?;
        as_json(self.db.put(domain, &f))
    }

    pub fn update(&mut self, domain: &str, id: &str, patch: &str) -> Result<String, JsValue> {
        let p = fields_of(patch)?;
        let k = rkey_of(id)?;
        as_json(self.db.update(domain, &k, &p))
    }

    pub fn get(&mut self, domain: &str, id: &str) -> Result<String, JsValue> {
        let k = rkey_of(id)?;
        let r = self.db.get(domain, &k);
        self.answer(r)
    }

    pub fn delete(&mut self, domain: &str, id: &str) -> Result<bool, JsValue> {
        let k = rkey_of(id)?;
        self.db.delete(domain, &k).map_err(|e| db_err(&e))
    }

    /// `after` is a record id or the empty string.
    pub fn scan(
        &mut self,
        domain: &str,
        reverse: bool,
        limit: usize,
        after: &str,
    ) -> Result<String, JsValue> {
        let after = if after.is_empty() {
            None
        } else {
            Some(rkey_of(after)?)
        };
        let r = self.db.scan(
            domain,
            craftworks_sdk::Scan {
                reverse,
                limit,
                after,
            },
        );
        self.answer(r)
    }

    /// What this client HOLDS — not what the tree contains.
    ///
    /// The in-memory store can answer `blocks`/`bytes`/`height` because it
    /// IS the tree. This one holds a copy of the ranges the app has bound, on
    /// a node that holds the rest, so those three are **`null` and not 0**.
    /// Zero would render as a real, empty database — the same
    /// not-loaded-versus-empty confusion this whole layer exists to prevent,
    /// arriving through a statistics panel instead of a read.
    ///
    /// The numbers that ARE this client's own are reported beside them.
    pub fn stats(&mut self) -> Result<String, JsValue> {
        let (n_pending, pending_bytes) = self.db.store_mut().copy.pending();
        let held = self.db.store_mut().copy.bytes();
        as_json::<serde_json::Value>(Ok(serde_json::json!({
            // Properties of the TREE, which lives on the node.
            "blocks": serde_json::Value::Null,
            "bytes": serde_json::Value::Null,
            "height": serde_json::Value::Null,
            // Properties of THIS CLIENT, which are the ones it can state.
            "heldBytes": held,
            "pendingWrites": n_pending,
            "pendingBytes": pending_bytes,
        })))
    }

    pub fn count(&mut self, domain: &str) -> Result<usize, JsValue> {
        match self.db.count(domain) {
            Ok(n) => Ok(n),
            Err(e) => Err(self.park(e)),
        }
    }

    /// The tree's root, as `node:<64 hex>`.
    pub fn root(&mut self) -> String {
        match craftworks_sdk::Reads::root(self.db.store_mut()) {
            Ok(root) => craftworks_sdk::BlockId::from_parts(freenet_prolly::kind::TREE_NODE, root)
                .to_string(),
            // EMPTY, never a zero root. A store that cannot state its root
            // has not got one yet; a zero root renders as a real tree that
            // happens to be empty, which is the same NotLoaded-vs-empty
            // confusion one layer up.
            Err(_) => String::new(),
        }
    }

    /// Load the DOMAINS an app names on open, before it asks for them.
    ///
    /// `manifest` is `["tasks", "notes"]` as JSON. Each domain becomes one
    /// range request on the pump; the answers land in the local copy, so a
    /// read that would have been `NOT_LOADED` is answered from memory.
    ///
    /// **Domains, not key ranges.** What a range is, is this crate's
    /// business: a caller that built one would be encoding the key layout,
    /// which is the thing this boundary exists to hide and which could then
    /// never change without breaking every app that had hard-coded it.
    ///
    /// A domain that is NOT DEFINED is refused, not skipped. A manifest
    /// naming a domain that does not exist is a project file that has
    /// drifted, and quietly skipping it makes the page mysteriously slow
    /// instead of telling anybody it is wrong.
    ///
    /// Bounded, because it comes from a file a person edits — and refused
    /// WHOLE rather than truncated, because a silently shortened preload is
    /// a page that is mysteriously slow for a different reason.
    pub fn preload(&mut self, manifest: &str) -> Result<usize, JsValue> {
        const MAX_DOMAINS: usize = 64;
        let domains: Vec<String> = serde_json::from_str(manifest)
            .map_err(|e| db_err(&DbError::Refused(format!("preload manifest: {e}"))))?;
        if domains.len() > MAX_DOMAINS {
            return Err(db_err(&DbError::TooLarge(format!(
                "preload names {} domains and the limit is {MAX_DOMAINS}",
                domains.len()
            ))));
        }
        // CHECKED BEFORE ANYTHING IS SENT, so a manifest with one bad name
        // does not leave half its ranges requested and half refused — the
        // caller would have no way to tell which half.
        for d in &domains {
            match self.db.schema(d) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(db_err(&DbError::NotDefined(format!(
                        "preload names `{d}`, which this project does not define"
                    ))))
                }
                // The schema itself is not loaded yet. Not an error and not a
                // reason to refuse the manifest: reading it is exactly what
                // opening the project is about to do.
                Err(e) if e.code() == "NOT_LOADED" => {}
                Err(e) => return Err(db_err(&e)),
            }
        }
        for (i, d) in domains.iter().enumerate() {
            let (lo, hi) = craftworks_sdk::Db::<CachedStore, SystemEnv>::domain_range(d);
            self.db
                .store_mut()
                .client
                .send(&craftworks_sdk::Loads::range_request(
                    PRELOAD_REQ_BASE + i as u64,
                    &lo,
                    &hi,
                    None,
                ));
        }
        Ok(domains.len())
    }

    /// The call tree of the last operation, in the instrument VOCABULARY.
    ///
    /// **No user content crosses this.** Not a key, not a value, not a domain
    /// name — only the fixed event fields the instrument defines. The same
    /// recording ships in a user's app and is what a support bundle is made
    /// of, so a site that emitted anything derived from a person's data would
    /// put it in every bundle, and no grep would find it.
    pub fn trace(&mut self) -> String {
        // The LAST write this session issued. `next_write_id` is the one the
        // next write will carry, so the last issued is one below it — and
        // before any write has been made there is nothing to trace.
        let next = self.db.store_mut().next_write_id();
        if next <= 1 {
            return "null".into();
        }
        let of = protocol::TraceOf::Write(next - 1);
        let Some(t) = self.db.store_mut().client.trace(of) else {
            return "null".into();
        };
        // Serialised FIELD BY FIELD, never derived.
        //
        // `Trace` is the SDK's own type and a derive would carry whatever is
        // added to it later straight across this boundary. The same recording
        // ships in a user's app and is what a support bundle is made of, so
        // what crosses is enumerated here: a step's NAME from the fixed
        // `protocol::Step` list, its depth, its count, and a coarse offset.
        // No key, no value, no domain name.
        let steps: Vec<serde_json::Value> = t
            .steps
            .iter()
            .map(|s| {
                serde_json::json!({
                    "step": format!("{:?}", s.what),
                    "depth": s.depth,
                    "n": s.n,
                    "atMs": s.at_ms,
                })
            })
            .collect();
        serde_json::json!({
            "steps": steps,
            "totalMs": t.total_ms,
            "truncated": t.truncated,
        })
        .to_string()
    }

    /// Turn tracing on or off. A parameter, not a rebuild.
    pub fn trace_on(&mut self, on: bool) {
        if on {
            self.db
                .store_mut()
                .client
                .trace_on(Box::new(crate::js_now_ms));
        } else {
            self.db.store_mut().client.trace_off();
        }
    }

    /// The client's timer: roll back writes with no verdict, notice stalls.
    pub fn tick(&mut self) -> String {
        let now = crate::js_now_ms();
        let told = self.db.store_mut().copy.time_out(now);
        let stalled = self.plan.tick(now);
        // A load nobody answered ends as UNAVAILABLE rather than waiting for
        // ever. The read parked on it gets a fact; a page can show it.
        self.loads.time_out(now);
        serde_json::json!({
            "rolledBack": told.rolled_back.len(),
            "stalled": stalled.map(|s| format!("{s:?}")),
            "loadsInFlight": self.loads.in_flight(),
        })
        .to_string()
    }
}

/// How long a subscribe request may go unanswered before this session says
/// it is polling.
///
/// Not a claim about the network: a bound on the WAIT, so a page never shows
/// a subscription it does not have. The tick keeps the data right either way,
/// which is why this can be short.
const WATCH_ANSWER_MS: u64 = 10_000;

/// The request ids preload uses, kept away from the app's own.
const PRELOAD_REQ_BASE: u64 = 1 << 32;

/// A `DbError` as JavaScript sees it: a stable `code` and a message that is
/// for a person to read, never for code to branch on.
fn db_err(e: &DbError) -> JsValue {
    let o = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&o, &"code".into(), &e.code().into());
    let _ = js_sys::Reflect::set(&o, &"message".into(), &e.to_string().into());
    let _ = js_sys::Reflect::set(&o, &"transient".into(), &e.is_transient().into());
    o.into()
}

fn as_json<T: serde::Serialize>(r: Result<T, DbError>) -> Result<String, JsValue> {
    let v = r.map_err(|e| db_err(&e))?;
    serde_json::to_string(&v).map_err(|e| db_err(&DbError::Refused(e.to_string())))
}

/// A `DbError` as JavaScript sees it, plus the ticket to wait on.
fn db_err_waiting(e: &DbError, wait: Option<u64>) -> JsValue {
    let o = db_err(e);
    if let Some(w) = wait {
        // The load this read is parked on. The page resolves the promise
        // when the session reports this ticket ended — from the EVENT of the
        // answer arriving, never from a timer.
        let _ = js_sys::Reflect::set(&o, &"wait".into(), &JsValue::from_f64(w as f64));
    }
    o
}

fn fields_of(s: &str) -> Result<serde_json::Map<String, serde_json::Value>, JsValue> {
    serde_json::from_str(s)
        .map_err(|e| db_err(&DbError::Refused(format!("fields must be an object: {e}"))))
}

fn rkey_of(id: &str) -> Result<craftworks_sdk::id::RKey, JsValue> {
    craftworks_sdk::id::from_hex(id)
        .ok_or_else(|| db_err(&DbError::Refused(format!("`{id}` is not a record id"))))
}
