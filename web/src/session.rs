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
use wire::{AckKind, Incoming};

/// A block's contract id from its cid (the Block code hashed once).
type ContractOf = Box<dyn Fn(&craftworks_sdk::Cid) -> craftworks_sdk::Cid>;

/// Everything one page-to-node connection needs.
#[wasm_bindgen]
pub struct Session {
    /// The database AND the store it reads through. One object, because a
    /// page has one connection and one tree: `Db` owns its store, and the
    /// pump reaches it through `store_mut()` rather than through a second
    /// handle that could drift out of step with it.
    db: craftworks_sdk::Db<CachedStore, SystemEnv>,
    /// Frames waiting to go out. WIRE frames, already enveloped — the page
    /// sends bytes and never learns what a `ClientRequest` is.
    out: Vec<Vec<u8>>,
    /// A counter, so each chunked request gets its own stream and two
    /// concurrent ones cannot be reassembled into each other.
    stream: u32,
    port: u16,
    /// Messages this build could not use, by reason.
    unusable: Vec<String>,
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
    /// What this page has bound, so a head move can name what is stale: watch
    /// keys (`Db::watch_key`) — a domain, or one parent's band of it.
    bound: std::collections::BTreeSet<String>,
    /// Asking what changed, and which answer belongs to which domain.
    ///
    /// In the SDK, not here, so it can be tested against a real engine
    /// rather than through a fake session written in JavaScript — which
    /// compiles this file and runs none of it.
    refresh: craftworks_sdk::Refresh,
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
    /// COLD READS IN THE PAGE (`craftworks_sdk::cold`): a range this node does
    /// not hold, read by this page's own GETs with a short timeout and a
    /// re-fetch, instead of by the engine's cold read and F52's stall.
    cold: craftworks_sdk::cold::ColdReads,
    /// A block's contract id from its cid — the Block contract's code hashed
    /// once (`wire::block::contract_deriver`). `None` until the page hands the
    /// code in with [`Session::set_cold_reads`]: without it no GET can be named.
    cold_contract: Option<ContractOf>,
    /// The builder called [`Session::set_cold_reads`]. Until then cold reads
    /// are ON by default, switched on by [`Session::provision`] with the
    /// Block code it hands in; after it, the builder's choice stands.
    cold_chosen: bool,
    /// The SIGNER delegate's wasm, handed in with [`Session::provision`].
    signer_code: Vec<u8>,
    /// The page's I/O over the client API, from `provision` on.
    page: Option<page_io::PageIo>,
    /// `Identity` sent to the in-page server once the signer is provisioned.
    page_identity_sent: bool,
    /// The signer's provisioning was reported by `take_progress`.
    provision_told: bool,
    /// PUTs of contracts the APP names (`put_contract`, builder#104), and
    /// what the node said about each — matched by the key it names.
    puts: wire::puts::Puts,
    /// A VIEW of somebody's published head (`open_named`, sdk#239): reads
    /// only, and every write refused before it reaches the store.
    read_only: bool,
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
            out: Vec::new(),
            stream: 1,
            port,
            unusable: Vec::new(),
            loads: craftworks_sdk::Loads::new(),
            head_root: [0u8; 32],
            head_id: [0u8; 32],
            head_named: String::new(),
            subscribed: false,
            foreign_notifications: 0,
            watching: false,
            head_moved: false,
            bound: std::collections::BTreeSet::new(),
            refresh: craftworks_sdk::Refresh::new(),
            asked_at_ms: 0,
            watch_refused: String::new(),
            cold: craftworks_sdk::cold::ColdReads::default(),
            cold_contract: None,
            cold_chosen: false,
            puts: wire::puts::Puts::default(),
            read_only: false,
            signer_code: Vec::new(),
            page: None,
            page_identity_sent: false,
            provision_told: false,
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
    ///
    /// Returns whether the frame was THIS session's. One socket carries a
    /// person's own session and every tree they read (`tree`, sdk#239); each
    /// is offered every frame and takes only what it asked for. A frame no
    /// session takes is counted once, with [`Session::unowned`].
    pub fn on_inbound(&mut self, bytes: &[u8]) -> bool {
        // Every node frame is the page executor's (page-io): the engine runs
        // in this page and the node is reached only through it.
        let owned = match self.page.as_mut() {
            Some(p) => p.inbound(bytes, page::Ms(crate::js_now_ms())),
            None => false,
        };
        self.pump_page();
        owned
    }

    /// A node frame NO session on this socket asked for (`on_inbound` said
    /// no, for every one). Counted, never silently dropped: the node chooses
    /// what it sends, and the count says whether it is happening at all.
    pub fn unowned(&mut self) {
        self.foreign_notifications += 1;
    }

    /// One protocol reply for this session's store, from the in-page
    /// `page::Server`.
    fn on_engine_reply(&mut self, m: Vec<u8>) {
        // Read, not intercepted: the store still gets every byte.
        // `Identity` is the only reply provisioning rests on, and
        // it is the delegate's own report about its own secret
        // store rather than an acknowledgement that a message
        // arrived.
        match protocol::decode_reply(&m) {
            Ok(protocol::Reply::Identity {
                head_root,
                head_id,
                head_seq,
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
                if head_root != [0u8; 32] {
                    self.cold.set_root(head_root);
                }
                // The newest head this client has heard of, from
                // anywhere. A load that finishes behind it is not
                // recorded.
                self.loads.note_seq(head_seq);
            }
            Ok(protocol::Reply::Page {
                req_id,
                entries,
                cursor,
                at,
                ..
            }) => self.on_page(req_id, entries, cursor, at),
            // A read the engine could not answer. The range is
            // NOT recorded as loaded: an empty page here would
            // say "this range is empty", which is a wrong answer
            // wearing the shape of a right one.
            // WHAT CHANGED since this client last looked.
            //
            // Every field BOUND, no `..`: a `cursor` dropped in
            // one is how a first page was applied as the whole
            // answer (sdk#140), and a field added later must
            // fail to compile here rather than vanish.
            Ok(protocol::Reply::Delta {
                req_id,
                changes,
                cursor,
                new_root,
                at,
            }) => {
                self.loads.note_seq(at.seq);
                self.on_delta(req_id, changes, cursor, new_root)
            }
            // The delta could not be computed. The interval is
            // forgotten and re-requested in full, through the
            // ordinary load path so it is bounded and ticketed
            // like any other — NOT applied as if it were a delta,
            // which would record a range as current on the
            // strength of an answer that said it could not say.
            Ok(protocol::Reply::FullReloadRequired { req_id, .. }) => {
                self.on_full_reload(req_id)
            }
            Ok(protocol::Reply::Unavailable { req_id, .. }) => {
                self.loads.on_unavailable(req_id)
            }
            _ => {}
        }
        self.db.store_mut().on_inbound(&m);
    }

    /// A read's answer, with a `NotLoaded` turned into a real request and a
    /// ticket to wait on.
    fn answer<T: serde::Serialize>(&mut self, r: Result<T, DbError>) -> Result<String, JsValue> {
        match self.decide(r) {
            craftworks_sdk::Outcome::Done(v) => {
                serde_json::to_string(&v).map_err(|e| db_err(&DbError::Refused(e.to_string())))
            }
            craftworks_sdk::Outcome::Wait(e, t) => Err(db_err_waiting(&e, Some(t))),
            craftworks_sdk::Outcome::Told(e) => Err(db_err(&e)),
        }
    }

    /// THE DECISION, which lives in the SDK so something native can run it.
    ///
    /// This type is `#[wasm_bindgen]` in a `cdylib`: nothing native can build
    /// one, so a test that reached this method could only be a fake session
    /// written in JavaScript — which compiles this file and runs none of it.
    ///
    /// Measured: with the recovery inline here, the sdk#89 defect could be
    /// put back — `define` returning a ticketless `NotLoaded` — and the whole
    /// suite stayed green. `craftworks_sdk::parking` carries it now, and
    /// `tests/cold_write_native.rs` drives it against a real `Shell`.
    fn decide<T>(&mut self, r: Result<T, DbError>) -> craftworks_sdk::Outcome<T> {
        let now = crate::js_now_ms();
        // Cold reads take a new load only when the page can name the GETs.
        let cold = self.cold_contract.as_ref().map(|_| &mut self.cold);
        let o = craftworks_sdk::decide_with(&mut self.loads, self.db.store_mut(), r, now, cold);
        self.pump_cold();
        o
    }

    /// A WRITE THAT HAD TO READ BEFORE IT COULD APPLY.
    ///
    /// Every write in this file reads first: `define` reads the existing
    /// schema to check the new one against it, and `put`, `update` and
    /// `delete` all go through `need_schema`. `update` and `delete` read the
    /// record as well. So all four can fail with `NotLoaded`, and that
    /// failure means **"I could not read what I needed in order to apply
    /// this"** — never "this write is invalid".
    ///
    /// The difference is the whole bug. Unparked, `define` returned a
    /// TICKETLESS `NotLoaded` that nothing could retry, and a published app
    /// was left with a form on screen, a button saying Published, and every
    /// put refused with `domain has no schema; define it first` — for ever,
    /// because the next mount ran the same cold define. Measured against a
    /// real node: 120 of 120 writes refused (sdk#89).
    ///
    /// # Retrying cannot double-apply
    ///
    /// In `Db`, every one of these reads happens BEFORE the single
    /// `self.write(...)` that mutates, and `write` either applies the whole
    /// edit or fails having applied none of it. A `NotLoaded` from any of
    /// them therefore leaves the tree untouched, so asking again once the
    /// range is loaded repeats the attempt rather than the effect.
    ///
    /// This is `answer`'s sibling and deliberately not `answer` itself: a
    /// write's return type is its own (`()`, `bool`, a `Record`), and
    /// serializing it to a string here would change four wasm signatures to
    /// share one helper. `count` takes it for the same reason — it is a read,
    /// but a read that answers a `usize`.
    fn decided<T>(&mut self, r: Result<T, DbError>) -> Result<T, JsValue> {
        match self.decide(r) {
            craftworks_sdk::Outcome::Done(v) => Ok(v),
            craftworks_sdk::Outcome::Wait(e, t) => Err(db_err_waiting(&e, Some(t))),
            craftworks_sdk::Outcome::Told(e) => Err(db_err(&e)),
        }
    }

    /// A page of a load arrived.
    fn on_page(
        &mut self,
        req_id: u64,
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        cursor: Option<Vec<u8>>,
        at: protocol::At,
    ) {
        match self.loads.on_page(req_id, entries, cursor, at) {
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
            craftworks_sdk::loads::Page::Complete { lo, hi, rows, at } => {
                let root = self.head_root;
                self.db.store_mut().on_page(&lo, &hi, rows, root);
                // A whole domain, loaded at one root: the next question about
                // it can be a real delta FROM that root (sdk#142).
                if let Some(d) = craftworks_sdk::Db::<CachedStore, SystemEnv>::watch_key_of_range(&lo, &hi) {
                    self.refresh.on_loaded(&d, at.root);
                }
            }
            // The tree moved under this load, or it finished behind what
            // this client already knows. Ask again from the top: what was
            // gathered is half from one tree and half from another.
            craftworks_sdk::loads::Page::Restart { lo, hi } => {
                self.db
                    .store_mut()
                    .client
                    .send(&craftworks_sdk::Loads::range_request(
                        req_id, &lo, &hi, None,
                    ));
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
                let code = match how {
                    craftworks_sdk::Ended::Loaded => "LOADED",
                    craftworks_sdk::Ended::Unavailable => "UNAVAILABLE",
                    craftworks_sdk::Ended::NotAnswering => "NOT_ANSWERING",
                };
                serde_json::json!({ "id": id, "ok": ok, "code": code })
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
        if std::mem::take(&mut self.head_moved) {
            // The head moved, so every bound domain is worth ASKING about.
            // Asking is not the same as having changed: what a binding
            // re-runs on is the ANSWER, which arrives as a `Delta`.
            let domains: Vec<String> = self.bound.iter().cloned().collect();
            for d in domains {
                self.refresh(&d);
            }
        }
        let changed = self.refresh.take_changed();
        serde_json::to_string(&changed).unwrap_or_else(|_| "[]".into())
    }

    /// Ask what changed in a domain since this client last saw it.
    ///
    /// **This is the refresh path, and for a while there was none.** A
    /// binding's `reload` read the LOCAL COPY, whose root moves only when a
    /// delta or a page arrives — and nothing sent `ChangesSince`, so the root
    /// never moved, `reload` always answered "nothing changed", and a tab
    /// that made no write could never see another's. The engine had the
    /// mechanism, `CachedStore::on_delta` had the mechanism, and no line
    /// joined them.
    ///
    /// The client sends the root IT last saw. That is what makes a missed
    /// notification recoverable: however many were dropped, the gap closes in
    /// one call.
    fn refresh(&mut self, domain: &str) {
        // A watch KEY: a domain, or one parent's band (sdk#137) — so a live
        // band asks what changed IN ITS BAND, and a change under a sibling
        // parent is not a change to it.
        let Some((lo, hi)) = craftworks_sdk::Db::<CachedStore, SystemEnv>::watch_range(domain) else {
            return;
        };
        // From the session's ONE request counter, shared with every load
        // (sdk#166).
        let id = self.loads.take_id();
        if let Some(req) = self.refresh.ask(id, domain, &lo, &hi) {
            self.db.store_mut().client.send(&req);
        }
    }

    /// Local movement: rolled-back writes, in the domains that are bound.
    ///
    /// A component re-renders on its OWN write through the same drain as on
    /// somebody else's. Without this a put changes `base + pending` but not
    /// the root, so a bound component would not re-render on its own write
    /// nor when it went PENDING -> CLEAN.
    fn note_local(&mut self, told: &craftworks_sdk::Told) {
        let bound: Vec<(String, Vec<u8>, Vec<u8>)> = self
            .bound
            .iter()
            .filter_map(|d| {
                let (lo, hi) = craftworks_sdk::Db::<CachedStore, SystemEnv>::watch_range(d)?;
                Some((d.clone(), lo, hi))
            })
            .collect();
        // EVERY watch containing the key, not the first: a domain binding and
        // a band binding of the same domain both moved (sdk#137).
        for (d, lo, hi) in &bound {
            let keys = told
                .rolled_back_keys
                .iter()
                .chain(told.moved_under_pending.iter());
            self.refresh
                .note_local(keys, |k| (k >= &lo[..] && k < &hi[..]).then(|| d.clone()));
        }
    }

    /// A delta arrived: apply it through the copy — or, if `Refresh` says it
    /// is only a first page, reload the domain INSTEAD. Never after: the copy
    /// records the root in `apply_delta` too, so applying first would leave
    /// it claiming a root it is not at (sdk#140).
    fn on_delta(
        &mut self,
        req_id: u64,
        changes: Vec<(Vec<u8>, Option<Vec<u8>>)>,
        cursor: Option<Vec<u8>>,
        new_root: [u8; 32],
    ) {
        match self.refresh.on_delta(req_id, changes, cursor, new_root) {
            craftworks_sdk::Answer::Delta {
                changes, new_root, ..
            } => {
                self.db.store_mut().on_delta(changes, new_root);
            }
            craftworks_sdk::Answer::NotOurs => self.foreign_notifications += 1,
            craftworks_sdk::Answer::Reload { domain } => self.reload(&domain),
        }
    }

    /// The engine could not compute a delta: load the range again.
    fn on_full_reload(&mut self, req_id: u64) {
        let craftworks_sdk::Answer::Reload { domain } = self.refresh.on_full_reload(req_id) else {
            self.foreign_notifications += 1;
            return;
        };
        self.reload(&domain);
    }

    /// Forget a domain's range and load it again.
    fn reload(&mut self, domain: &str) {
        let (lo, hi) = craftworks_sdk::Db::<CachedStore, SystemEnv>::domain_range(domain);
        // FORGOTTEN, then re-requested through `Loads` -- ticketed and bounded
        // like any other load. The copy must not keep answering from a range
        // the engine has just said it cannot reconcile.
        self.db.store_mut().copy.forget(&lo, &hi);
        if let Some((id, send)) = self.loads.want(&lo, &hi, crate::js_now_ms()) {
            if send {
                self.db
                    .store_mut()
                    .client
                    .send(&craftworks_sdk::Loads::range_request(id, &lo, &hi, None));
            }
        }
    }

    pub fn refresh_domain(&mut self, domain: &str) {
        self.refresh(domain);
    }

    /// What this page is showing — a watch key from [`Session::watch_key`] —
    /// so a head move can name it.
    ///
    /// Recorded by the session rather than tracked in JS, because deciding
    /// what to reload is a decision.
    pub fn bind(&mut self, domain: &str) {
        self.bound.insert(domain.to_string());
    }

    /// The watch key for a binding of `domain`, over one `parent`'s band when
    /// `parent` is not empty (sdk#137). JavaScript holds it as an opaque name;
    /// what it MEANS is decided here.
    pub fn watch_key(&self, domain: &str, parent: &str) -> Result<String, JsValue> {
        if parent.is_empty() {
            return Ok(craftworks_sdk::Db::<CachedStore, SystemEnv>::watch_key(domain, None));
        }
        let p = rkey_of(parent)?;
        Ok(craftworks_sdk::Db::<CachedStore, SystemEnv>::watch_key(domain, Some(&p)))
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
        } else if !self.provisioned() {
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

    /// Hand the store's protocol frames to the in-page server. HELD (not
    /// drained) until the page exists: an earlier version drained and dropped
    /// what it could not deliver — the lose-the-queue defect already fixed
    /// once in the page's socket pump.
    fn envelope_engine_requests(&mut self) {
        if self.page.is_some() {
            for bytes in self.db.store_mut().take_outbound() {
                self.page.as_mut().expect("checked").client(&bytes);
            }
            self.pump_page();
        }
    }

    /// Cold reads on or off with the Block code — the one place both the
    /// default (at provision) and the builder's choice go through.
    fn switch_cold(&mut self, on: bool, block_code: Vec<u8>) {
        if on && !self.cold.on {
            // A fresh reader: the RTO and the window start where RFC 6298 and
            // slow start say, at the head this session already knows.
            self.cold = craftworks_sdk::cold::ColdReads::switched_on();
            if self.head_root != [0u8; 32] {
                self.cold.set_root(self.head_root);
            }
        }
        self.cold.on = on;
        self.cold_contract = if on && !block_code.is_empty() {
            Some(Box::new(wire::block::contract_deriver(&block_code)))
        } else {
            None
        };
    }

    /// PAGE MODE: what `page-io` produced, carried out — its node frames go
    /// out, its protocol replies reach this session exactly as a delegate's
    /// did, and once the signer is provisioned the in-page engine is started
    /// with `Identity`.
    /// The signer answered that it holds NO key: this person's first page on
    /// this node. Mint one and provision it — the only place a key is minted.
    fn mint_if_needed(&mut self) {
        let Some(p) = self.page.as_mut() else { return };
        if !p.needs_key() {
            return;
        }
        let mut seed = [0u8; 32];
        if getrandom::getrandom(&mut seed).is_err() {
            self.unusable.push("no randomness to mint a key".into());
            return;
        }
        let sk = ed25519_dalek::SigningKey::from_bytes(&seed);
        let params = wire::register_params(&sk.verifying_key().to_bytes(), wire::HEAD_NAME);
        p.provision_with(sk.to_bytes().to_vec(), params);
    }

    fn pump_page(&mut self) {
        self.mint_if_needed();
        let Some(p) = self.page.as_mut() else { return };
        let frames = p.take_frames();
        let replies = p.take_replies();
        let ready = p.provisioned() && !self.page_identity_sent;
        let others = p.take_others();
        self.out.extend(frames);
        for m in replies {
            self.on_engine_reply(m);
        }
        // The node's answers about contracts that are not the page's own: the
        // app's PUTs, by the key each names.
        for answer in others {
            let ours = match &answer {
                Incoming::Ack(AckKind::Put(key)) => self.puts.acked(key),
                Incoming::PutFailed { key, said } => self.puts.refused(key, said),
                _ => false,
            };
            if !ours {
                self.unusable.push(format!("a PUT answer for a contract this session never put: {answer:?}"));
            }
        }
        if ready {
            self.page_identity_sent = true;
            match protocol::encode_request(1, &protocol::Request::Identity) {
                Ok(frame) => {
                    if let Some(p) = self.page.as_mut() {
                        p.client(&frame);
                    }
                    self.pump_page();
                }
                Err(e) => self.unusable.push(format!("the identity request cannot be encoded: {e:?}")),
            }
        }
    }

    /// COLD READS: what the cold reader decided, carried out.
    ///
    /// * loads it handed back (the root is this node's own, F55; or the tree
    ///   is not something a fetch fixes) go to the engine exactly as `decide`
    ///   sends them;
    /// * each GET goes out as a FRESH client GET of the block's contract —
    ///   on this connection. ASSUMPTION (the live run tells): a re-GET on the
    ///   same connection is not pinned behind the stalled one; client GETs
    ///   are not deduplicated (F55, read), each is its own transaction;
    /// * each finished load completes through the SAME path an engine page
    ///   does (`on_page`), at the root it read.
    fn pump_cold(&mut self) {
        for (id, lo, hi) in self.cold.take_returned() {
            self.db
                .store_mut()
                .client
                .send(&craftworks_sdk::Loads::range_request(id, &lo, &hi, None));
        }
        let gets = self.cold.take_gets();
        if let Some(contract) = self.cold_contract.as_ref() {
            let ids: Vec<craftworks_sdk::Cid> = gets.iter().map(|g| contract(&g.block)).collect();
            for id in ids {
                let stream = self.next_stream();
                match wire::frame_get(wire::contract_id(id), false, stream) {
                    Ok(frames) => self.out.extend(frames),
                    Err(e) => self.unusable.push(format!("a cold GET could not be framed: {e}")),
                }
            }
        }
        for id in self.cold.take_not_answering() {
            self.loads.on_not_answering(id);
        }
        for (id, rows, root) in self.cold.take_done() {
            let at = protocol::At { seq: self.loads.known_seq(), root };
            self.on_page(id, rows, None, at);
        }
    }


    fn next_stream(&mut self) -> u32 {
        self.stream = self.stream.wrapping_add(1).max(1);
        self.stream
    }

    /// Provisioning steps that completed since this was last asked, as JSON.
    /// The page path has one: the signer is provisioned ([`Session::provisioned`]),
    /// reported ONCE.
    pub fn take_progress(&mut self) -> String {
        if self.provisioned() && !std::mem::replace(&mut self.provision_told, true) {
            return r#"["Signer"]"#.into();
        }
        "[]".into()
    }

    /// The Session's OWN cold reads — the F52 mitigation for the engine
    /// DELEGATE, whose cold read could stall ≈ 60 s — are OFF on the page
    /// path: the in-page engine fetches every block itself through page-io,
    /// each node call on its RTO (#227). Turning them ON is refused by name
    /// rather than half-done (their answers would never reach this session,
    /// page-io owning every node frame); their removal is sdk#258.
    pub fn set_cold_reads(&mut self, on: bool, block_code: Vec<u8>) {
        let _ = block_code;
        if on {
            self.unusable.push("set_cold_reads(true): the in-page engine reads cold itself; the Session's own cold reads are off (sdk#258)".into());
        }
    }

    /// Milliseconds until the cold reader's earliest fetch reaches its RTO,
    /// or -1 when none is in flight. The page arms a one-shot timer for it
    /// and calls [`Session::cold_tick`] then — so a late fetch is seen at its
    /// own timeout even when no answer arrives and the 1 s tick is far off.
    pub fn cold_due_ms(&self) -> i32 {
        let now = crate::js_now_ms();
        // The page executor's next timer too (page mode): its every node call
        // retries on its RTO, and the page's 1 s tick is too coarse for it.
        let page = self.page.as_ref().and_then(|p| p.next_due()).map(|d| d.0.saturating_sub(now));
        match [self.cold.next_due_ms(now), page].into_iter().flatten().min() {
            Some(ms) => ms.min(i32::MAX as u64) as i32,
            None => -1,
        }
    }

    /// The cold reader's clock alone, at the moment [`Session::cold_due_ms`]
    /// named: its late fetches re-sent, its give-ups reported.
    pub fn cold_tick(&mut self) {
        let now = crate::js_now_ms();
        self.cold.tick(now);
        self.pump_cold();
        if let Some(p) = self.page.as_mut() {
            p.tick(page::Ms(now));
        }
        self.pump_page();
    }

    /// Every cold GET's timeout, re-fetch and answer since this was last
    /// asked, as JSON — what a live run reports, and what a support bundle
    /// carries.
    pub fn take_cold_log(&mut self) -> String {
        serde_json::to_string(&std::mem::take(&mut self.cold.log)).unwrap_or_else(|_| "[]".into())
    }

    /// Messages this build could not use, by reason.
    pub fn unusable(&self) -> String {
        // AND page-io's, in page mode: what the page's own I/O could not use
        // (a refused provisioning, a frame it could not make, a mint it was
        // stopped from) is this session's to report. Kept apart, it was
        // invisible — core dev's M254 minted on every pump and nothing showed.
        let mut all = self.unusable.clone();
        if let Some(p) = self.page.as_ref() {
            all.extend(p.unusable().iter().cloned());
        }
        serde_json::to_string(&all).unwrap_or_else(|_| "[]".into())
    }

    /// Provision this session on the node: the SIGNER delegate's wasm, and
    /// the Block and Register contracts' code, as the page fetched them.
    ///
    /// The engine runs IN THE PAGE (`page::Server`) and the signer signs: the
    /// signer is registered, asked which Register it signs for, and given a
    /// key only when it holds none (`provision_page`). Every node operation
    /// goes through `page-io` — there is no other path to the node. A page
    /// that only reads somebody's head calls [`Session::open_named`] instead.
    pub fn provision(&mut self, signer: Vec<u8>, block: Vec<u8>, register: Vec<u8>) {
        // A VIEW installs nothing on the node it reads from (sdk#239).
        if self.read_only {
            self.unusable.push(format!("{READ_ONLY}: provisioning refused"));
            return;
        }
        self.signer_code = signer;
        self.provision_page(block, register);
    }

    /// The provisioning: a TEST key minted here and FORGOTTEN (as the
    /// delegate path's; real keys are sdk#14), the head Register named by it,
    /// and the SIGNER registered and provisioned — all through `page-io`. The
    /// page's own cold reads are OFF here: the in-page engine fetches blocks
    /// itself, through `page-io`, on the RTO estimator and the window.
    fn provision_page(&mut self, block: Vec<u8>, register: Vec<u8>) {
        if self.page.is_some() {
            return;
        }
        // NOT MINTED HERE. The page ASKS the signer which Register it signs
        // for, and opens that one; only a signer holding no key gets a new one
        // (`mint_if_needed`, from `pump_page`). Minting on every load made a
        // reload — and every second tab — a new identity (sdk#234).
        let (container, signer) = wire::delegate_from_code(&self.signer_code);
        let art = page_io::Artefacts {
            block_code: block,
            register_code: register,
            // Named by the signer's answer, or by the minted key's.
            register_params: Vec::new(),
            signer,
        };
        let server = page::server::Server::new(
            page::Page::unstarted(engine::Params::default(), page::PutPath::Page),
            page::server::SignerFacts::default(),
        );
        let mut io = page_io::PageIo::new(server, art);
        io.begin(container);
        self.cold_chosen = true;
        self.switch_cold(false, Vec::new());
        self.page = Some(io);
        self.pump_page();
        self.envelope_engine_requests();
    }

    /// Has provisioning finished — because the SIGNER said so: it answered
    /// Provisioned to a fresh key, or NAMED the Register it signs for (a
    /// reload, a second tab: nothing minted). `PageIo::provisioned`, and
    /// nothing else (engineer2's contract for `open()`, sdk#255).
    pub fn provisioned(&self) -> bool {
        self.page.as_ref().is_some_and(|p| p.provisioned())
    }

    /// Opening's re-asks are SPENT: the signer did not answer the first
    /// exchange (the Register query, or the provisioning) within its budget —
    /// "not answering" (page-io's `exhausted`). One of `open()`'s named ends.
    pub fn exhausted(&self) -> bool {
        self.page.as_ref().is_some_and(|p| p.exhausted())
    }

    /// Opening is STILL WAITING past its first RTO: the first exchange is
    /// being re-asked and not yet answered. Named for display; empty when not.
    pub fn stalled(&self) -> String {
        match self.page.as_ref() {
            Some(p) if p.stalled() => "the signer has not answered yet; asking again".into(),
            _ => String::new(),
        }
    }

    /// Opening was REFUSED, by the signer or the node, in its own words
    /// (page-io's `refused`) — or empty. One of `open()`'s named ends.
    pub fn refused(&self) -> String {
        self.page.as_ref().and_then(|p| p.refused()).unwrap_or_default().to_string()
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
        // The node's copy of a subscription outlives the engine's context and
        // can be evicted at its cap without anyone being told (F39), so this
        // connection asks again. Idempotent at the node, so it costs nothing
        // when nothing was lost.
        self.subscribed = false;
        self.watching = false;
        // An app PUT's answer sent on the old socket never arrives on this one.
        self.puts.connection_lost();
    }

    /// PUT a contract the APP names — its code, params and state (builder#104:
    /// a web container, whose params are the hash of its state) — and return
    /// its key: the contract instance id, as the node names it and serves a
    /// web container under (`/v1/contract/web/<key>/`).
    ///
    /// The frames go out with the next `outbound`, through page-io (the only
    /// path to the node). [`Session::put_status`] says
    /// what the node answered, matched by this key. **An ack is not
    /// durability:** a publisher that must know reads it back.
    pub fn put_contract(&mut self, code: Vec<u8>, params: Vec<u8>, state: Vec<u8>) -> Result<String, JsValue> {
        let (key, contract, state) = wire::puts::contract(&code, &params, &state);
        let Some(p) = self.page.as_mut() else {
            return Err(JsValue::from_str("provision first — there is no path to the node before it"));
        };
        p.put_contract(contract, state).map_err(|e| JsValue::from_str(&e))?;
        self.puts.begin(key.clone());
        self.pump_page();
        Ok(key)
    }

    /// Open a VIEW of somebody's PUBLISHED head (sdk#239): the Register whose
    /// instance id is `register_id` (hex, as [`Session::head_id`] gives it on
    /// the publisher's session). Published data is readable by default;
    /// writing is access control, which a view does not have.
    ///
    /// Instead of `provision`: the in-page engine reads that head
    /// and its blocks through page-io's READER — no signer, nothing installed
    /// or registered on this node — and every write is refused before it
    /// reaches the store ([`Session::read_only`]).
    ///
    /// `range` (1..=255): this reader's stream-id range on the shared socket,
    /// one per open tree.
    pub fn open_named(&mut self, block_code: Vec<u8>, register_id: &str, range: u8) -> Result<(), JsValue> {
        let bad = || JsValue::from_str("open_named: a register id is 64 hex characters");
        if register_id.len() != 64 || !register_id.is_ascii() {
            return Err(bad());
        }
        let mut id = [0u8; 32];
        for (i, b) in id.iter_mut().enumerate() {
            *b = u8::from_str_radix(&register_id[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
        }
        if self.page.is_some() {
            return Err(JsValue::from_str("open_named: this session is already open on its own head"));
        }
        self.read_only = true;
        let server = page::server::Server::new(
            page::Page::unstarted(engine::Params::default(), page::PutPath::Page),
            page::server::SignerFacts::default(),
        );
        self.cold_chosen = true;
        self.switch_cold(false, Vec::new());
        self.page = Some(page_io::PageIo::reader(server, block_code, id, range));
        self.pump_page();
        self.envelope_engine_requests();
        Ok(())
    }

    /// This session is a VIEW (`open_named`): nothing can be written. What a
    /// runtime renders from — a view shows no inputs.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// The head this session stands on, as `open_named` takes it: the head
    /// Register's instance id in hex, or empty until `Identity` has named it.
    /// What a publisher records so a view can open the same head.
    pub fn head_id(&self) -> String {
        if self.head_id == [0u8; 32] {
            String::new()
        } else {
            self.head_id.iter().map(|b| format!("{b:02x}")).collect()
        }
    }

    /// Where the PUT of `key` (`put_contract`'s return) stands, as JSON:
    /// `{"state":"none"|"pending"|"put"|"refused"|"unanswered","said":"…"}`.
    /// `said` is the node's own words for a refusal: display only.
    pub fn put_status(&self, key: &str) -> String {
        use wire::puts::PutState;
        let (state, said) = match self.puts.state(key) {
            None => ("none", ""),
            Some(PutState::Pending) => ("pending", ""),
            Some(PutState::Put) => ("put", ""),
            Some(PutState::Refused(w)) => ("refused", w.as_str()),
            Some(PutState::Unanswered) => ("unanswered", ""),
        };
        serde_json::json!({ "state": state, "said": said }).to_string()
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
        // A VIEW defines nothing: a definition identical to the published
        // one is a no-op (an app defines its domains on open, and a view runs
        // the same app); any other is a write, refused. The schema is READ
        // through the same decision, so an unloaded one parks, never "none".
        if self.read_only {
            let r = self.db.schema(domain).and_then(|old| match old {
                Some(o) if o == s => Ok(()),
                _ => Err(DbError::Refused(format!("{READ_ONLY}: `{domain}` is not defined like that here"))),
            });
            return self.decided(r);
        }
        let r = self.db.define(domain, &s);
        self.decided(r)
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
        self.writable()?;
        let f = fields_of(fields)?;
        let r = self.db.put(domain, &f);
        json_of(self.decided(r)?)
    }

    /// Create at a derived slot, or answer the record already there
    /// (craftworks-sdk#149). The slot is READ first, and an unloaded one is a
    /// parked `NotLoaded` like any read — never taken for absent, which in a
    /// fresh session would write over the published record.
    pub fn create_at(&mut self, domain: &str, slot: &str, fields: &str) -> Result<String, JsValue> {
        self.writable()?;
        let f = fields_of(fields)?;
        let s = rkey_of(slot)?;
        let r = self.db.create_at(domain, s, &f);
        json_of(self.decided(r)?)
    }

    pub fn update(&mut self, domain: &str, id: &str, patch: &str) -> Result<String, JsValue> {
        self.writable()?;
        let p = fields_of(patch)?;
        let k = loc_of(id)?;
        let r = self.db.update(domain, k, &p);
        json_of(self.decided(r)?)
    }

    /// An id that does not parse answers `null`, the same as one that parses
    /// and is not there (craftworks-sdk#118).
    pub fn get(&mut self, domain: &str, id: &str) -> Result<String, JsValue> {
        let Some(k) = craftworks_sdk::id::loc_from_hex(id) else {
            // The domain is still checked: a read of a domain that does not
            // exist is a programming error, not a stale id from outside.
            let probe = self.db.schema(domain);
            self.answer(probe)?;
            return Ok("null".into());
        };
        let r = self.db.get(domain, k);
        self.answer(r)
    }

    pub fn delete(&mut self, domain: &str, id: &str) -> Result<bool, JsValue> {
        self.writable()?;
        let k = loc_of(id)?;
        let r = self.db.delete(domain, k);
        self.decided(r)
    }

    /// The children of one parent, as a bounded read (craftworks-sdk#122).
    ///
    /// The app names the PARENT and never builds a key range, which is the
    /// rule `preload` keeps for the same reason.
    pub fn children(
        &mut self,
        domain: &str,
        parent: &str,
        reverse: bool,
        limit: usize,
        after: &str,
    ) -> Result<String, JsValue> {
        let after = if after.is_empty() { None } else { Some(rkey_of(after)?) };
        let p = rkey_of(parent)?;
        let r = self
            .db
            .children(domain, &p, craftworks_sdk::Scan { reverse, limit, after });
        self.answer(r)
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
    /// Writes made here and not yet PUBLISHED, held ones included
    /// (craftworks-sdk#163): what the page's unsaved-changes guard and its
    /// "saving N…" count. Not `Accepted` — an accepted write can still be
    /// lost with the tab.
    pub fn unsaved_writes(&self) -> usize {
        self.db.store().unsaved_writes()
    }

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
        // A READ that keeps its own type rather than a JSON string, so it
        // takes `decided` like the writes do. Before this it called a private
        // `park` directly and never told `Loads` the chain had SUCCEEDED, so
        // a count that worked left the done-set standing for the next call.
        let r = self.db.count(domain);
        self.decided(r)
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
        // FIRST: TELL THE DELEGATE THE TIME.
        //
        // Everything below this line is the PAGE's own housekeeping — its
        // copies, its loads, its rolled-back writes. None of it reaches the
        // delegate, and for a while nothing did: the delegate has no clock
        // (F32), so a deadline it holds fires only when a connected client
        // says what time it is. Owed parity was therefore never put and a
        // stuck commit was never reported `Stalled`, on a page that was
        // ticking a thousand times a minute.
        //
        // At most one unanswered tick per session (sdk#174): a refused one is
        // benign, the next carries the time as it is then.
        self.db.store_mut().send_tick(now);
        // And the continuation of a write the engine parked: it runs when
        // its client asks after it, and nothing else asks (sdk#174).
        self.db.store_mut().ask_unheard(now);
        // THROUGH THE STORE'S OWN TICK, not straight to the copy.
        //
        // This called `copy.time_out(now)` directly, which does the rolling
        // back and nothing else — so everything else `CachedStore::tick` does
        // never ran on a page. What it does besides rolling back is drain the
        // OUTBOX: the queue of writes the engine refused with `Busy` while a
        // commit was in flight, which nothing else re-sends (sdk#106).
        //
        // That backstop exists because notifications are lossy (F39), and it
        // was dead here the day it was written. Reaching past a type's own
        // entry point to one of its fields is how: the copy is a field, and
        // calling it directly skipped every decision the store makes around
        // it.
        let told = self.db.store_mut().tick();
        // A LOCAL change is a change too: a write that rolled back moves the
        // rows a component is showing, and the component finds out the same
        // way it finds out about anybody else's.
        self.note_local(&told);
        // A load nobody answered ends as UNAVAILABLE rather than waiting for
        // ever. The read parked on it gets a fact; a page can show it.
        // A load the page's own cold read holds ends by its blocks' deadlines
        // (the cold reader's), not by the load's age.
        let cold = &self.cold;
        self.loads.time_out_except(now, |id| cold.holds(id));
        self.cold.tick(now);
        if let Some(p) = self.page.as_mut() {
            p.tick(page::Ms(now));
        }
        self.pump_page();
        self.pump_cold();
        // sdk#143/#144: a conflicted update or define is RE-RUN on the new
        // base. What it must load first goes through the ordinary load path
        // (and its timeout); what it could not keep is the app's news.
        let rerun = self.db.rerun(now, self.loads.budget_ms);
        for (lo, hi) in rerun.load {
            let _ = self.decide::<()>(Err(DbError::NotLoaded { lo, hi }));
        }
        let reruns: Vec<serde_json::Value> = rerun
            .events
            .iter()
            .map(|e| match e {
                craftworks_sdk::RerunEvent::Dropped { write_id, fields } => serde_json::json!({
                    "writeId": write_id,
                    "outcome": "dropped",
                    "fields": fields,
                    "line": "Some of your change was not kept: these fields were changed elsewhere first.",
                }),
                craftworks_sdk::RerunEvent::Deleted { write_id } => serde_json::json!({
                    "writeId": write_id,
                    "outcome": "deleted",
                    "line": "Your change was not kept: the record was deleted elsewhere.",
                }),
                craftworks_sdk::RerunEvent::Failed { write_id, reason } => serde_json::json!({
                    "writeId": write_id,
                    "outcome": "failed",
                    "reason": reason,
                    "line": "Your change could not be saved.",
                }),
            })
            .collect();
        // M2 (sdk#148): writes that did not apply because what they READ had
        // moved, and were not re-run (a create, a delete). Facts and one
        // default line; how to show them is the page's.
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let conflicts: Vec<serde_json::Value> = self
            .db
            .store_mut()
            .take_conflicts()
            .into_iter()
            .filter(|c| !rerun.taken.contains(&c.write_id))
            .map(|c| {
                serde_json::json!({
                    "writeId": c.write_id,
                    "key": hex(&c.key),
                    "current": c.current.map(|h| hex(&h)),
                    "line": "This change was not saved: the record was changed elsewhere first. Showing the current version.",
                })
            })
            .collect();
        // sdk#225b: rows of a saved write that another device of the same
        // identity replaced.
        let superseded: Vec<serde_json::Value> = self
            .db
            .store_mut()
            .take_superseded()
            .into_iter()
            .map(|s| {
                serde_json::json!({
                    "writeId": s.write_id,
                    "keys": s.keys.iter().map(|k| hex(k)).collect::<Vec<_>>(),
                    "line": "Your last save was replaced by your other device for these rows. Showing its version.",
                })
            })
            .collect();
        serde_json::json!({
            "rolledBack": told.rolled_back.len(),
            "loadsInFlight": self.loads.in_flight(),
            "conflicts": conflicts,
            "superseded": superseded,
            "reruns": reruns,
        })
        .to_string()
    }

    /// THE LAST THING A PAGE SAYS.
    ///
    /// A tab that goes away stops sending ticks, so whatever the engine was
    /// holding back to coalesce — owed parity, an applied write not yet in a
    /// commit — would sit unwritten until somebody opened the app again.
    /// This asks for all of it now.
    ///
    /// Called on `visibilitychange` to hidden and on `pagehide`, which is
    /// the closest a browser comes to telling a page it is ending: there is
    /// no event that reliably fires on close, and `beforeunload` does not
    /// fire on mobile at all. Both may fire, and both may fire more than
    /// once; a `Flush` is idempotent, so the cost of the extra ones is a
    /// frame the engine answers with nothing to do.
    ///
    /// The frame still has to LEAVE, which is the page's job: the caller
    /// pumps the socket after this, and a page that is already gone did what
    /// it could.
    pub fn flush(&mut self) {
        self.db.store_mut().send_flush();
    }

    /// How often a page should call [`Session::tick`], in milliseconds.
    ///
    /// From `protocol`, so the page that sends the time and the engine whose
    /// bounds are counted in it cannot hold two different numbers. A page
    /// that hard-coded 1000 would go on being right until the day this moved.
    pub fn tick_ms(&self) -> u32 {
        protocol::TICK_MS as u32
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
/// What every refusal of a view says first.
const READ_ONLY: &str = "read-only: this is a view of somebody's published data, and writing needs write access";

impl Session {
    /// A view refuses every write, before it reaches the store: the safety
    /// net under a runtime that renders a view with no inputs at all.
    fn writable(&self) -> Result<(), JsValue> {
        if self.read_only {
            return Err(db_err(&DbError::Refused(READ_ONLY.into())));
        }
        Ok(())
    }
}

fn db_err(e: &DbError) -> JsValue {
    let o = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&o, &"code".into(), &e.code().into());
    let _ = js_sys::Reflect::set(&o, &"message".into(), &e.to_string().into());
    let _ = js_sys::Reflect::set(&o, &"transient".into(), &e.is_transient().into());
    // Whether waiting for a confirmation and making the SAME write again may
    // succeed (NO_ROOM), with the bound it met (sdk#180).
    let _ = js_sys::Reflect::set(&o, &"retryable".into(), &e.is_retryable().into());
    if let Some(cap) = e.cap() {
        let _ = js_sys::Reflect::set(&o, &"cap".into(), &JsValue::from_f64(cap as f64));
    }
    o.into()
}

/// Serialize a value already recovered from its `Result`.
///
/// `as_json` takes the `Result` and so decides what a failure MEANS; a write
/// that reads has already had that decided by `decided`, which parks it.
fn json_of<T: serde::Serialize>(v: T) -> Result<String, JsValue> {
    serde_json::to_string(&v).map_err(|e| db_err(&DbError::Refused(e.to_string())))
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

/// A record id as an app holds it: 32 hex, or 64 when the domain keys its
/// records under a parent (craftworks-sdk#122). The length says which, so an
/// app never takes one apart.
fn loc_of(id: &str) -> Result<craftworks_sdk::id::Loc, JsValue> {
    craftworks_sdk::id::loc_from_hex(id)
        .ok_or_else(|| db_err(&DbError::Refused(format!("`{id}` is not a record id"))))
}
