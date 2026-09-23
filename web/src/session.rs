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
//!
//! # One owner for the head, the tree and a write's fate (READ-STATE, design B)
//!
//! The engine runs in this page (`page::Server`, hosted by `page_io::PageIo`),
//! and the store `Db` reads through OWNS that host
//! ([`craftworks_sdk::PageStore`]): a read walks the engine's tree from its
//! root over the blocks the page holds. This session keeps no head, no root,
//! no rows and no ranges — each was a copy of the engine's, and every defect
//! of 2026-09-23 was one of them going stale. The one head-shaped fact kept
//! here is each LIVE binding's `RenderedAt`, which only the binding can know.

use craftworks_sdk::{DbError, Outcome, PageStore, SystemEnv};
use page_io::PageIo;
use wasm_bindgen::prelude::*;
use wire::{AckKind, Incoming};

/// The store a page's `Db` reads and writes through.
type Store = PageStore<PageIo>;

/// Everything one page-to-node connection needs.
#[wasm_bindgen]
pub struct Session {
    /// The database AND the store it reads through — and the store owns the
    /// page (`PageIo`, the in-page engine's host) once there is one. One
    /// object, because a page has one connection and one tree.
    db: craftworks_sdk::Db<Store, SystemEnv>,
    /// Frames waiting to go out. WIRE frames, already enveloped — the page
    /// sends bytes and never learns what a `ClientRequest` is.
    out: Vec<Vec<u8>>,
    port: u16,
    /// Messages this build could not use, by reason.
    unusable: Vec<String>,
    /// Frames on this socket that NO session took (`unowned`). COUNTED, not
    /// described: the node chooses what it sends, and a count is the thing
    /// that says whether it is happening at all.
    foreign_notifications: usize,
    /// What this page has bound LIVE, by watch key (`Db::watch_key`: a
    /// domain, or one parent's band of it), each with its `RenderedAt`
    /// (READ-STATE; `craftworks_sdk::LiveBindings`, where it is tested).
    bound: craftworks_sdk::LiveBindings,
    /// The SIGNER delegate's wasm, handed in with [`Session::provision`].
    signer_code: Vec<u8>,
    /// `Identity` sent to the in-page server once the signer is provisioned.
    page_identity_sent: bool,
    /// The signer's provisioning was reported by `take_progress`.
    provision_told: bool,
    /// THE APP this session is (the forest ruling): a person has ONE tree,
    /// divided by app. Every domain name crossing into this session is
    /// app-relative and gains `<app>.` here, so an app has no name for
    /// another app's data and cannot write it. `None`: nothing is written.
    app: Option<String>,
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
                PageStore::new(Box::new(crate::js_now_ms), Box::new(crate::js_now_ms)),
                SystemEnv,
                device,
            ),
            out: Vec::new(),
            port,
            unusable: Vec::new(),
            foreign_notifications: 0,
            bound: craftworks_sdk::LiveBindings::default(),
            app: None,
            signer_code: Vec::new(),
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
        self.pump_page();
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
    /// Returns whether the frame was THIS session's. One socket carries a
    /// person's own session and every tree they read (`tree`, sdk#239); each
    /// is offered every frame and takes only what it asked for. A frame no
    /// session takes is counted once, with [`Session::unowned`].
    pub fn on_inbound(&mut self, bytes: &[u8]) -> bool {
        // Every node frame is the page executor's (page-io): the engine runs
        // in this page and the node is reached only through it.
        let owned = match self.page_mut() {
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

    /// A read's answer, with a refusal for want of blocks given its ticket.
    fn answer<T: serde::Serialize>(&mut self, r: Result<T, DbError>) -> Result<String, JsValue> {
        match self.decide(r) {
            Outcome::Done(v) => {
                serde_json::to_string(&v).map_err(|e| db_err(&DbError::Refused(e.to_string())))
            }
            Outcome::Wait(e, t) => Err(db_err_waiting(&e, Some(t))),
            Outcome::Told(e) => Err(db_err(&e)),
        }
    }

    /// THE DECISION, which lives in the SDK so something native can run it
    /// (`PageStore::decide`; sdk#89).
    fn decide<T>(&mut self, r: Result<T, DbError>) -> Outcome<T> {
        self.db.store_mut().decide(r)
    }

    /// [`Session::answer`]'s sibling for calls whose value keeps its own type
    /// (a write's `()`, `bool` or `Record`; `count`'s `usize`).
    fn decided<T>(&mut self, r: Result<T, DbError>) -> Result<T, JsValue> {
        match self.decide(r) {
            Outcome::Done(v) => Ok(v),
            Outcome::Wait(e, t) => Err(db_err_waiting(&e, Some(t))),
            Outcome::Told(e) => Err(db_err(&e)),
        }
    }

    /// Tickets that ended since this was last asked, as JSON:
    /// `[{"id", "ok", "code", "why"}]`, `code` one of LOADED / UNAVAILABLE /
    /// NOT_ANSWERING, `why` the engine's reason for an UNAVAILABLE (or null).
    ///
    /// The page resolves its parked reads from THIS, called when a message
    /// arrives. Never a timer: a timer either spins or answers late, and
    /// neither of those is a fact about the data.
    pub fn take_loads(&mut self) -> String {
        let out: Vec<serde_json::Value> = self
            .db
            .store_mut()
            .take_ended()
            .into_iter()
            .map(|(id, how)| (id, how, self.db.store_mut().why(id)))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|(id, how, why)| serde_json::json!({ "id": id, "ok": how == craftworks_sdk::Ended::Loaded, "code": how.code(), "why": why }))
            .collect();
        serde_json::to_string(&out).unwrap_or_else(|_| "[]".into())
    }

    /// A parked read woke on `ticket`: the NEXT call on this session walks
    /// the root that ticket's read was made at, so a chain of hops ends on
    /// one tree rather than chasing a head that keeps moving (READ-STATE
    /// inv. 2). `engine-db.js`'s `once` calls this right before it asks again.
    ///
    /// A JavaScript NUMBER, as `take_loads` hands the id out: a `u64` here
    /// crosses as a BigInt, the page's number THROWS, and the read failed as
    /// UNAVAILABLE on every page (measured in the notes acceptance). Ticket
    /// ids are a counter from 1, far inside 2^53.
    pub fn resume(&mut self, ticket: f64) {
        self.db.store_mut().resume(ticket as u64);
    }

    /// How many reads are waiting on a ticket right now.
    pub fn loads_in_flight(&self) -> usize {
        self.db.store().open_tickets()
    }

    /// The domains where one of THIS client's own writes changed state
    /// (published, parity-complete, lost, failed, conflict, superseded) since
    /// this was last asked, as JSON. Drains. Every binding of this client on
    /// them re-reads, LIVE or not: its own write's state reaching it is the
    /// same rule as its own write reaching it, and it costs no network
    /// (builder#107). LIVE governs only OTHER writers' changes
    /// ([`Session::take_stale`]).
    pub fn take_state_changed(&mut self) -> String {
        let keys = self.db.store_mut().writes.take_state_changed();
        // Back to the app-relative names JavaScript holds, as `take_stale`
        // does — the stored `<app>.<name>` is keyed by no binding (#267).
        let domains = craftworks_sdk::Db::<Store, SystemEnv>::own_domains_of_keys(self.app.as_deref(), &keys);
        craftworks_sdk::app::to_js(&domains)
    }

    /// Which LIVE bindings' ranges changed since each was last told, as JSON
    /// (app-relative watch keys). Drains.
    ///
    /// **Rust decides which, not the page.** Per bound watch key: the diff
    /// from its `RenderedAt` to the engine's root over the key's range — the
    /// tree's own diff, walked here over the blocks the page holds. A key
    /// whose range changed is named, and its `RenderedAt` moves to that root:
    /// its binding re-reads at that root or a newer one, so no change is
    /// missed; at worst one re-read finds nothing new. A diff that cannot be
    /// walked (a block not held) counts as a change, and the re-read waits on
    /// the fetch.
    pub fn take_stale(&mut self) -> String {
        let head = self.db.store_mut().head();
        let changed = self.bound.take_changed(self.db.store_mut(), head, craftworks_sdk::Db::<Store, SystemEnv>::watch_range);
        // Back to the app-relative keys JavaScript holds.
        let changed: Vec<craftworks_sdk::app::AppName> = changed.iter().filter_map(|k| self.own_name(k.stored())).collect();
        craftworks_sdk::app::to_js(&changed)
    }

    /// What this page is showing LIVE — a watch key from
    /// [`Session::watch_key`] — so a head move can name it. It has rendered
    /// nothing yet: its `RenderedAt` is set by [`Session::rendered`] when its
    /// first read completes, never here.
    pub fn bind(&mut self, domain: &str) {
        if let Ok(key) = self.read_name(domain) {
            self.bound.bind(craftworks_sdk::live_bindings::WatchKey::of(key));
        }
    }

    /// The binding of `domain` (a watch key) has just SHOWN what the read
    /// that completed in this same call answered: its `RenderedAt` is the
    /// root that read walked (`PageStore::answered_at` — the pinned root it
    /// resumed at, or the head). Called by `engine-db.js` synchronously after
    /// the binding's read returns, so no other read runs in between. A
    /// re-read that failed never calls it, and its change is reported again
    /// (the architect on sdk#289).
    pub fn rendered(&mut self, domain: &str) {
        if let Ok(key) = self.read_name(domain) {
            let root = self.db.store().answered_at();
            self.bound.rendered(&craftworks_sdk::live_bindings::WatchKey::of(key), root);
        }
    }

    /// The watch key for a binding of `domain`, over one `parent`'s band when
    /// `parent` is not empty (sdk#137). JavaScript holds it as an opaque name;
    /// what it MEANS is decided here.
    pub fn watch_key(&self, domain: &str, parent: &str) -> Result<String, JsValue> {
        if parent.is_empty() {
            return Ok(craftworks_sdk::Db::<Store, SystemEnv>::watch_key(domain, None));
        }
        let p = rkey_of(parent)?;
        Ok(craftworks_sdk::Db::<Store, SystemEnv>::watch_key(domain, Some(&p)))
    }

    pub fn unbind(&mut self, domain: &str) {
        if let Ok(key) = self.read_name(domain) {
            self.bound.unbind(&craftworks_sdk::live_bindings::WatchKey::of(key));
        }
    }

    /// How this session actually finds out that the head moved.
    ///
    /// REPORTED, never assumed. `HeadSubscribed` only after the node has
    /// ACCEPTED the subscription — asking is not being answered — and
    /// `Polled` says, in words, why it is not: the LIVE switch in a builder
    /// shows which one a component really has, so a binding that silently
    /// fell back to polling cannot look like one that did not.
    pub fn live_mode(&self) -> String {
        // THE PAGE PATH'S SUBSCRIPTION IS PAGE-IO'S (sdk#259). The head is
        // read by GET with `subscribe`, and the node's answer to THAT is what
        // makes this page live. The MAPPING is page-io's
        // (`HeadSubscription::live_mode`), where a native test pins every
        // branch; this only serializes it. (No mode is named in quotes
        // anywhere in this function, comments included: the web crate's
        // page-path wiring test refuses a quoted mode here, as one would be a
        // second mapping no native test can reach.)
        let m = self.page().map(|p| p.head_subscription().live_mode()).unwrap_or_else(page_io::LiveMode::no_page);
        let (mode, why, changes) = (m.mode, m.why, m.head_changes);
        serde_json::json!({
            "mode": mode,
            "why": why,
            "foreignNotifications": self.foreign_notifications,
            // What the subscription has DELIVERED, so "subscribed" can be told
            // from "subscribed and being told".
            "headChanges": changes,
        })
        .to_string()
    }

    /// The page, once provisioning (or `open_named`) has made one. It lives
    /// in the store: the store reads by walking its engine.
    fn page(&self) -> Option<&PageIo> {
        self.db.store().host()
    }

    fn page_mut(&mut self) -> Option<&mut PageIo> {
        self.db.store_mut().host_mut()
    }

    /// The signer answered that it holds NO key: this person's first page on
    /// this node. Mint one and provision it — the only place a key is minted.
    fn mint_if_needed(&mut self) {
        let Some(p) = self.page_mut() else { return };
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
        if let Some(p) = self.page_mut() {
            p.provision_with(sk.to_bytes().to_vec(), params);
        }
    }

    /// PAGE MODE: what `page-io` produced, carried out — the store's frames
    /// reach the in-page server and its replies the store (`PageStore::sync`),
    /// the page's node frames go out, and once the signer is provisioned the
    /// in-page engine is started with `Identity`.
    fn pump_page(&mut self) {
        self.mint_if_needed();
        if self.page().is_none() {
            return;
        }
        self.db.store_mut().sync();
        let sent = self.page_identity_sent;
        let p = self.page_mut().expect("checked");
        let frames = p.take_frames();
        let ready = p.provisioned() && !sent;
        let others = p.take_others();
        self.out.extend(frames);
        // The app's own PUTs are the page's (`put_status`): what is left is
        // an answer about a contract this session never put.
        for answer in others {
            self.unusable.push(format!("a PUT answer for a contract this session never put: {answer:?}"));
        }
        if ready {
            self.page_identity_sent = true;
            self.db.store_mut().writes.client.send(&protocol::Request::Identity);
            self.pump_page();
        }
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

    /// The Session's OWN cold reads are gone (sdk#258): the in-page engine
    /// fetches every block itself through page-io, each node call on its RTO
    /// (#227). Turning them ON is refused by name rather than half-done.
    pub fn set_cold_reads(&mut self, on: bool, block_code: Vec<u8>) {
        let _ = block_code;
        if on {
            self.unusable.push("set_cold_reads(true): the in-page engine reads cold itself; the Session's own cold reads are gone (sdk#258)".into());
        }
    }

    /// Milliseconds until the page executor's next timer (its every node call
    /// retries on its RTO, and the page's 1 s tick is too coarse for it), or
    /// -1 when none is due. The page arms a one-shot timer for it and calls
    /// [`Session::cold_tick`] then.
    pub fn cold_due_ms(&self) -> i32 {
        let now = crate::js_now_ms();
        match self.page().and_then(|p| p.next_due()).map(|d| d.0.saturating_sub(now)) {
            Some(ms) => ms.min(i32::MAX as u64) as i32,
            None => -1,
        }
    }

    /// The page executor's clock alone, at the moment
    /// [`Session::cold_due_ms`] named.
    pub fn cold_tick(&mut self) {
        let now = crate::js_now_ms();
        if let Some(p) = self.page_mut() {
            p.tick(page::Ms(now));
        }
        self.pump_page();
    }

    /// Messages this build could not use, by reason.
    pub fn unusable(&self) -> String {
        // AND page-io's, in page mode: what the page's own I/O could not use
        // (a refused provisioning, a frame it could not make, a mint it was
        // stopped from) is this session's to report. Kept apart, it was
        // invisible — core dev's M254 minted on every pump and nothing showed.
        let mut all = self.unusable.clone();
        if let Some(p) = self.page() {
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
        if self.page().is_some_and(|p| p.read_only()) {
            self.unusable.push(format!("{READ_ONLY}: provisioning refused"));
            return;
        }
        self.signer_code = signer;
        self.provision_page(block, register);
    }

    /// WHOSE NODE IS THIS: ask the node's EXISTING signer which Register it
    /// signs for, registering nothing (`PageIo::ask`). A publisher's page
    /// compares the answer with the app's publisher head: equal, the person
    /// opening it holds the key on this node, and it opens WRITABLE through
    /// `provision`; otherwise it is another user's node, and nothing was installed or
    /// minted here. The answer: [`Session::asked`].
    pub fn ask_signer(&mut self, signer: Vec<u8>, block: Vec<u8>, register: Vec<u8>) {
        if self.page().is_some() {
            self.unusable.push("ask_signer: this session already has a page".into());
            return;
        }
        let (_, key) = wire::delegate_from_code(&signer);
        // Kept for `open_own`: a node with no signer gets this one then.
        self.signer_code = signer;
        let art = page_io::Artefacts { block_code: block, register_code: register, register_params: Vec::new(), signer: key };
        let server = page::server::Server::new(
            page::Page::unstarted(engine::Params::default(), page::PutPath::Page),
            page::server::SignerFacts::default(),
        );
        let mut io = page_io::PageIo::new(server, art);
        io.ask();
        self.db.store_mut().set_host(io);
        self.pump_page();
    }

    /// [`Session::ask_signer`]'s answer, as JSON:
    /// `{"state":"pending"|"register"|"nokey"|"nosigner"|"refused"|"silent","register":"<hex>","said":"…"}`.
    pub fn asked(&self) -> String {
        use page_io::Asked;
        let (state, register, said) = match self.page().and_then(|p| p.asked()) {
            None => ("pending", String::new(), String::new()),
            Some(Asked::Register(id)) => ("register", craftworks_sdk::hex(id), String::new()),
            Some(Asked::NoKey) => ("nokey", String::new(), String::new()),
            Some(Asked::NoSigner(w)) => ("nosigner", String::new(), w.clone()),
            Some(Asked::Refused(w)) => ("refused", String::new(), w.clone()),
            Some(Asked::NotAnswering) => ("silent", String::new(), String::new()),
        };
        serde_json::json!({ "state": state, "register": register, "said": said }).to_string()
    }

    /// The provisioning: a TEST key minted here and FORGOTTEN (as the
    /// delegate path's; real keys are sdk#14), the head Register named by it,
    /// and the SIGNER registered and provisioned — all through `page-io`. The
    /// page's own cold reads are OFF here: the in-page engine fetches blocks
    /// itself, through `page-io`, on the RTO estimator and the window.
    fn provision_page(&mut self, block: Vec<u8>, register: Vec<u8>) {
        if self.page().is_some() {
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
        self.db.store_mut().set_host(io);
        self.pump_page();
    }

    /// Has provisioning finished — because the SIGNER said so: it answered
    /// Provisioned to a fresh key, or NAMED the Register it signs for (a
    /// reload, a second tab: nothing minted). `PageIo::provisioned`, and
    /// nothing else (engineer2's contract for `open()`, sdk#255).
    pub fn provisioned(&self) -> bool {
        self.page().is_some_and(|p| p.provisioned())
    }

    /// Opening's re-asks are SPENT: the signer did not answer the first
    /// exchange (the Register query, or the provisioning) within its budget —
    /// "not answering" (page-io's `exhausted`). One of `open()`'s named ends.
    pub fn exhausted(&self) -> bool {
        self.page().is_some_and(|p| p.exhausted())
    }

    /// Opening is STILL WAITING past its first RTO: the first exchange is
    /// being re-asked and not yet answered. Named for display; empty when not.
    pub fn stalled(&self) -> String {
        match self.page() {
            Some(p) if p.stalled() => "the signer has not answered yet; asking again".into(),
            _ => String::new(),
        }
    }

    /// Opening was REFUSED, by the signer or the node, in its own words
    /// (page-io's `refused`) — or empty. One of `open()`'s named ends.
    pub fn refused(&self) -> String {
        self.page().and_then(|p| p.refused()).unwrap_or_default().to_string()
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
        // The head subscription is page-io's, and it re-reads the head on the
        // new connection: nothing to reset here (sdk#259). An app PUT whose
        // answer went with the old socket is re-sent at its deadline, as every
        // op is.
    }

    /// PUT a contract the APP names — its code, params and state (builder#104:
    /// a web container, whose params are the hash of its state) — and return
    /// its key: the contract instance id, as the node names it and serves a
    /// web container under (`/v1/contract/web/<key>/`).
    ///
    /// The PAGE sends it, like every op: on its deadline, re-sent while
    /// unanswered, and ENDED by `page::APP_PUT_BUDGET_MS` at the latest.
    /// [`Session::put_status`] says where it stands, matched by this key.
    /// **An ack is not durability:** a publisher that must know reads it back.
    pub fn put_contract(&mut self, code: Vec<u8>, params: Vec<u8>, state: Vec<u8>) -> Result<String, JsValue> {
        let (key, contract, state) = wire::puts::contract(&code, &params, &state);
        let Some(p) = self.page_mut() else {
            return Err(JsValue::from_str("provision first — there is no path to the node before it"));
        };
        p.put_contract(contract, state, page::Ms(crate::js_now_ms())).map_err(|e| JsValue::from_str(&e))?;
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
    /// reaches the store ([`Session::can_write`] says "no").
    ///
    /// `range` (1..=255): this reader's stream-id range on the shared socket,
    /// one per open tree.
    pub fn open_named(&mut self, block_code: Vec<u8>, register_id: &str, range: u8) -> Result<(), JsValue> {
        let id = head_of_hex(register_id).ok_or_else(|| JsValue::from_str("open_named: a register id is 64 hex characters"))?;
        if self.page().is_some() {
            return Err(JsValue::from_str("open_named: this session is already open on its own head"));
        }
        let server = page::server::Server::new(
            page::Page::unstarted(engine::Params::default(), page::PutPath::Page),
            page::server::SignerFacts::default(),
        );
        self.db.store_mut().set_view();
        self.db.store_mut().set_host(page_io::PageIo::reader(server, block_code, id, range));
        self.pump_page();
        Ok(())
    }

    /// MAY THIS SESSION WRITE `head`? The ONE decision a runtime renders
    /// inputs from (DATA-SOURCE; the architect's point 5), and the same one
    /// every write is refused by: `head` is a head id in hex, or "" for this
    /// session's OWN tree (a `mine` component's). As JSON:
    /// `{"answer":"yes"|"no"|"unknown","why":"…"}`. Derived each time from
    /// what page-io holds (the signer's answer, the page's opening), and
    /// cached nowhere -- not here, not in JS.
    pub fn can_write(&self, head: &str) -> String {
        let (answer, why) = match self.may_write(head) {
            page_io::MayWrite::Yes => ("yes", String::new()),
            page_io::MayWrite::No(w) => ("no", w),
            page_io::MayWrite::Unknown(w) => ("unknown", w),
        };
        serde_json::json!({ "answer": answer, "why": why }).to_string()
    }

    /// OPEN THE USER'S OWN TREE on an asked session (DATA-SOURCE `mine`):
    /// `PageIo::claim`, then what opening does -- a key is minted only where
    /// the signer holds none (`mint_if_needed`), and the head is created by
    /// the first write. Answers `can_write("")` afterwards. A session opened
    /// with `provision` is open on its own tree already: nothing to do.
    pub fn open_own(&mut self) -> String {
        if self.page().is_some_and(|p| p.asking()) {
            let (container, _) = wire::delegate_from_code(&self.signer_code);
            if let Some(p) = self.page_mut() {
                p.claim(container);
            }
            self.pump_page();
        }
        self.can_write("")
    }

    /// The head this session stands on, as `open_named` takes it: the head
    /// Register's instance id in hex, or empty until `Identity` has named it.
    /// What a publisher records so a view can open the same head.
    pub fn head_id(&self) -> String {
        // page-io's: it names the Register it reads (sdk#239), and nothing
        // here keeps a second copy of it.
        match self.page().map(|p| p.register_id()) {
            Some(id) if id != [0u8; 32] => id.iter().map(|b| format!("{b:02x}")).collect(),
            _ => String::new(),
        }
    }

    /// Which app this session is (`open({ app })`). Set once: an app id is
    /// 1–32 of a-z 0-9 _ - (no `.`, which separates it from the domain).
    /// THE app-id rule, `app::check` — its one statement — for this session's
    /// JavaScript (`engine-db`'s `other(app)`), in its words. Nothing is set.
    pub fn check_app(&self, app: &str) -> Result<(), JsValue> {
        craftworks_sdk::app::check(app).map_err(|e| db_err(&e))
    }

    pub fn set_app(&mut self, app: &str) -> Result<(), JsValue> {
        craftworks_sdk::app::check(app).map_err(|e| db_err(&e))?;
        match &self.app {
            Some(a) if a != app => Err(db_err(&DbError::Refused(format!("this session is app `{a}`; it cannot become `{app}`")))),
            _ => {
                self.app = Some(app.to_string());
                Ok(())
            }
        }
    }

    /// Where the PUT of `key` (`put_contract`'s return) stands, as JSON:
    /// `{"state":"none"|"pending"|"put"|"refused"|"failed","said":"…"}`.
    /// It always ENDS (the page's budget): `refused` carries the node's words,
    /// `failed` what was tried. `said` is display only.
    pub fn put_status(&self, key: &str) -> String {
        use page::AppPut;
        let (state, said) = match self.page().and_then(|p| p.app_put(key)) {
            None => ("none", ""),
            Some(AppPut::Pending) => ("pending", ""),
            Some(AppPut::Put) => ("put", ""),
            Some(AppPut::Refused(w)) => ("refused", w.as_str()),
            Some(AppPut::GaveUp(w)) => ("failed", w.as_str()),
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
        self.write_name(domain)?;
        if let page_io::MayWrite::No(why) | page_io::MayWrite::Unknown(why) = self.may_write("") {
            let name = self.read_name(domain)?;
            let r = self.db.schema(&name).and_then(|old| match old {
                Some(o) if o == s => Ok(()),
                _ => Err(DbError::Refused(format!("{why}: `{domain}` is not defined like that here"))),
            });
            return self.decided(r);
        }
        let name = self.write_name(domain)?;
        let r = self.db.define(&name, &s);
        self.decided(r)
    }

    pub fn schema(&mut self, domain: &str) -> Result<String, JsValue> {
        let name = self.read_name(domain)?;
        let r = self.db.schema(&name);
        self.answer(r)
    }

    pub fn domains(&mut self) -> Result<String, JsValue> {
        // THIS app's domains, by the names it gave them.
        let r = self.db.domains().map(|all| all.into_iter().filter_map(|d| self.own_name(&craftworks_sdk::app::StoredName::of_tree(d))).map(|n| n.as_str().to_string()).collect::<Vec<_>>());
        self.answer(r)
    }

    pub fn put(&mut self, domain: &str, fields: &str) -> Result<String, JsValue> {
        // The NAME first: whose data it is decides before whether this node
        // may write (another app's is never written, wherever).
        let name = self.write_name(domain)?;
        self.writable()?;
        let f = fields_of(fields)?;
        let r = self.db.put(&name, &f);
        json_of(self.decided(r)?)
    }

    /// Create at a derived slot, or answer the record already there
    /// (craftworks-sdk#149). The slot is READ first, and an unloaded one is a
    /// parked `NotLoaded` like any read — never taken for absent, which in a
    /// fresh session would write over the published record.
    pub fn create_at(&mut self, domain: &str, slot: &str, fields: &str) -> Result<String, JsValue> {
        // The NAME first: whose data it is decides before whether this node
        // may write (another app's is never written, wherever).
        let name = self.write_name(domain)?;
        self.writable()?;
        let f = fields_of(fields)?;
        let s = rkey_of(slot)?;
        let r = self.db.create_at(&name, s, &f);
        json_of(self.decided(r)?)
    }

    pub fn update(&mut self, domain: &str, id: &str, patch: &str) -> Result<String, JsValue> {
        // The NAME first: whose data it is decides before whether this node
        // may write (another app's is never written, wherever).
        let name = self.write_name(domain)?;
        self.writable()?;
        let p = fields_of(patch)?;
        let k = loc_of(id)?;
        let r = self.db.update(&name, k, &p);
        json_of(self.decided(r)?)
    }

    /// An id that does not parse answers `null`, the same as one that parses
    /// and is not there (craftworks-sdk#118).
    pub fn get(&mut self, domain: &str, id: &str) -> Result<String, JsValue> {
        let name = self.read_name(domain)?;
        let domain = name.as_str();
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
        // The NAME first: whose data it is decides before whether this node
        // may write (another app's is never written, wherever).
        let name = self.write_name(domain)?;
        self.writable()?;
        let k = loc_of(id)?;
        let r = self.db.delete(&name, k);
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
        let name = self.read_name(domain)?;
        let r = self
            .db
            .children(&name, &p, craftworks_sdk::Scan { reverse, limit, after });
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
        let name = self.read_name(domain)?;
        let r = self.db.scan(
            &name,
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
    /// IS the tree. This one holds the blocks it has walked or written, of a
    /// tree the node holds, so those three are **`null` and not 0**.
    /// Zero would render as a real, empty database — the same
    /// not-loaded-versus-empty confusion this whole layer exists to prevent,
    /// arriving through a statistics panel instead of a read.
    ///
    /// The numbers that ARE this client's own are reported beside them.
    /// Writes made here and not yet PUBLISHED (craftworks-sdk#163): this
    /// session's writes in the engine's queue, pulled (R-b) -- what the
    /// page's unsaved-changes guard and its "saving N…" count. A queued write
    /// is still lost with the tab.
    pub fn unsaved_writes(&self) -> usize {
        self.db.store().unsaved_writes()
    }

    pub fn stats(&mut self) -> Result<String, JsValue> {
        // The page's write queue (R-b): every session's writes not yet
        // committed, and their bytes -- what `QUEUE_FULL` is measured on.
        let (n_pending, pending_bytes) = self.db.store().queue_load();
        let held = self.page().map(|p| p.server.page.blocks().len());
        as_json::<serde_json::Value>(Ok(serde_json::json!({
            // Properties of the TREE, which lives on the node.
            "blocks": serde_json::Value::Null,
            "bytes": serde_json::Value::Null,
            "height": serde_json::Value::Null,
            // Properties of THIS CLIENT, which are the ones it can state.
            // Blocks this page holds (the only cache it has, by content id).
            "heldBlocks": held,
            "pendingWrites": n_pending,
            "pendingBytes": pending_bytes,
        })))
    }

    pub fn count(&mut self, domain: &str) -> Result<usize, JsValue> {
        // A READ that keeps its own type rather than a JSON string, so it
        // takes `decided` like the writes do. Before this it called a private
        // `park` directly and never told `Loads` the chain had SUCCEEDED, so
        // a count that worked left the done-set standing for the next call.
        let name = self.read_name(domain)?;
        let r = self.db.count(&name);
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
        // App-relative, like every name that crosses in.
        let domains = domains.iter().map(|d| self.read_name(d)).collect::<Result<Vec<_>, _>>()?;
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
        // Each domain WALKED now: a range whose blocks are held answers at
        // once and costs nothing more; one that is not has the engine fetch
        // them, so the app's first read of it walks warm.
        for d in &domains {
            let (lo, hi) = craftworks_sdk::Db::<Store, SystemEnv>::domain_range(d);
            let _ = craftworks_sdk::Reads::scan(self.db.store_mut(), &lo, &hi, false, usize::MAX);
            self.db.store_mut().take_ticket();
        }
        self.pump_page();
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
        let next = self.db.store().writes.next_write_id();
        if next <= 1 {
            return "null".into();
        }
        let of = protocol::TraceOf::Write(next - 1);
        let Some(t) = self.db.store_mut().writes.client.trace(of) else {
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
                .writes
                .client
                .trace_on(Box::new(crate::js_now_ms));
        } else {
            self.db.store_mut().writes.client.trace_off();
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
        self.db.store_mut().writes.send_tick(now);
        // And the continuation of a write the engine parked: it runs when
        // its client asks after it, and nothing else asks (sdk#174).
        self.db.store_mut().ask_after_applying();
        // NOTHING HERE TIMES A WRITE OUT (R-b, sdk#291). The engine owns the
        // queue, and a write waiting its turn behind the engine's own commits
        // is not "unanswered": the outbox's 60 s timeout rolled such writes
        // back -- 34 of 3,000 paced puts gone while "saving" read 0. A write's
        // only exits are the named fates the engine gives it, pulled here.
        self.db.store_mut().sync();
        let ended = self.db.store_mut().writes.take_ended();
        let mut lost_gave_up = Vec::new();
        let mut forced_lost = Vec::new();
        let ended: Vec<serde_json::Value> = ended
            .into_iter()
            .map(|(id, how)| {
                use craftworks_sdk::writes::Ended as E;
                let (fate, line, extra) = match how {
                    E::Lost => {
                        lost_gave_up.push(id);
                        ("LOST", format!("write {id} was told Lost with its tries spent and was not saved"), serde_json::json!({}))
                    }
                    E::ForcedLost => {
                        forced_lost.push(id);
                        ("LOST", format!("write {id} was forced past its reads and was lost; it was not sent again, because a forced write cannot be re-checked"), serde_json::json!({}))
                    }
                    E::Failed => ("FAILED", format!("write {id} could not be saved"), serde_json::json!({})),
                    E::Unknown => ("UNKNOWN", format!("write {id} may or may not have been saved (its confirmation was lost); check it"), serde_json::json!({})),
                    E::TooLarge { limit, got, .. } => ("TOO_LARGE", format!("write {id} is over the engine's limit ({got} against {limit}); split it into smaller writes"), serde_json::json!({ "limit": limit, "got": got })),
                    E::QueueFull { bytes, limit } => ("QUEUE_FULL", format!("write {id} was not taken: {bytes} of {limit} bytes of writes were already waiting to be saved; wait for them, then try again"), serde_json::json!({ "bytes": bytes, "limit": limit })),
                };
                self.unusable.push(line.clone());
                serde_json::json!({ "writeId": id, "fate": fate, "line": line, "detail": extra })
            })
            .collect();
        // sdk#235: writes refused at the door — OUR bug, never the person's —
        // each with every write that fell with it, NAMED.
        let unread: Vec<serde_json::Value> = self
            .db
            .store_mut()
            .writes
            .take_unread()
            .into_iter()
            .map(|u| {
                let key = u.key.as_deref().map(craftworks_sdk::hex);
                let line = format!(
                    "The app's SDK wrote {} without reading it first, so {} change{} {} not saved. This is a bug in the SDK, not something you did.",
                    key.as_deref().map(|k| format!("key {k}")).unwrap_or_else(|| "a key".into()),
                    u.write_ids.len(),
                    if u.write_ids.len() == 1 { "" } else { "s" },
                    if u.write_ids.len() == 1 { "was" } else { "were" },
                );
                self.unusable.push(line.clone());
                serde_json::json!({ "writeIds": u.write_ids, "key": key, "line": line })
            })
            .collect();
        // How many writes the engine took forced past their reads: the
        // transitional form, as a number a person can see (sdk#281 removes
        // `Any` at zero). Only the SDK's store-level batches build one.
        let forced_writes = self.page().map(|p| p.forced_writes()).unwrap_or(0);
        // A rolled-back write's keys reach its bindings through
        // `take_state_changed`, as every own-write state change does.
        //
        // A ticket nobody ended ends NOT_ANSWERING, and one ended and never
        // resumed lets its root go (`TICKET_LIFE_MS`).
        self.db.store_mut().tick(now);
        if let Some(p) = self.page_mut() {
            p.tick(page::Ms(now));
        }
        self.pump_page();
        // sdk#143/#144: a conflicted update or define is RE-RUN on the new
        // base. What it must load first goes through the ordinary load path
        // (and its timeout); what it could not keep is the app's news.
        // What it must read first is fetched by the walk that stopped on it
        // (its ticket, unwaited: the next tick walks again), bounded by the
        // ticket's own lifetime.
        let rerun = self.db.rerun(now, craftworks_sdk::page_store::TICKET_LIFE_MS);
        self.db.store_mut().take_ticket();
        self.db.store_mut().unpin();
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
            .writes
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
            .writes
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
            "rolledBack": ended.len() + unread.len() + conflicts.len(),
            "ended": ended,
            "lostGaveUp": lost_gave_up,
            "unread": unread,
            "forcedLost": forced_lost,
            "forcedWrites": {
                "count": forced_writes,
                "line": format!("{forced_writes} write{} forced past their reads (by the SDK's store-level batches)", if forced_writes == 1 { "" } else { "s" }),
            },
            "loadsInFlight": self.db.store().open_tickets(),
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
        self.db.store_mut().writes.send_flush();
        self.pump_page();
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



/// A `DbError` as JavaScript sees it: a stable `code` and a message that is
/// for a person to read, never for code to branch on.
impl Session {
    /// A domain (or watch key) this app READS, as the tree stores it
    /// (`craftworks_sdk::app::read`).
    fn read_name(&self, name: &str) -> Result<craftworks_sdk::app::StoredName, JsValue> {
        craftworks_sdk::app::read(self.app.as_deref(), name).map_err(|e| db_err(&e))
    }

    /// A domain this app WRITES: only its own (`craftworks_sdk::app::write`).
    fn write_name(&self, name: &str) -> Result<craftworks_sdk::app::StoredName, JsValue> {
        craftworks_sdk::app::write(self.app.as_deref(), name).map_err(|e| db_err(&e))
    }

    /// A stored name back to what this app calls it; `None` for another app's.
    fn own_name(&self, stored: &craftworks_sdk::app::StoredName) -> Option<craftworks_sdk::app::AppName> {
        craftworks_sdk::app::own(self.app.as_deref(), stored)
    }
}

use page_io::READ_ONLY;

impl Session {
    /// THE DECISION, page-io's (`PageIo::may_write`): `head` in hex, or "" for
    /// this session's own tree. No page yet: not known.
    fn may_write(&self, head: &str) -> page_io::MayWrite {
        let Some(p) = self.page() else { return page_io::MayWrite::Unknown("this session is not open on a node yet".into()) };
        if head.is_empty() {
            return p.may_write(None);
        }
        match head_of_hex(head) {
            Some(h) => p.may_write(Some(h)),
            None => page_io::MayWrite::No(format!("`{head}` is not a head id (64 hex characters)")),
        }
    }

    /// Every write is refused by the SAME decision the runtime renders from
    /// (`can_write("")`), before it reaches the store. An asked session's
    /// first write opens the user's own tree (`open_own`): the head is
    /// created on first write.
    fn writable(&mut self) -> Result<(), JsValue> {
        if self.page().is_some_and(|p| p.asking()) {
            self.open_own();
        }
        match self.may_write("") {
            page_io::MayWrite::Yes => Ok(()),
            page_io::MayWrite::No(why) | page_io::MayWrite::Unknown(why) => Err(db_err(&DbError::Refused(why))),
        }
    }
}

/// A head id as `head_id()` gives it: 64 hex characters.
pub(crate) fn head_of_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut id = [0u8; 32];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(id)
}

fn db_err(e: &DbError) -> JsValue {
    let o = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&o, &"code".into(), &e.code().into());
    let _ = js_sys::Reflect::set(&o, &"message".into(), &e.to_string().into());
    let _ = js_sys::Reflect::set(&o, &"transient".into(), &e.is_transient().into());
    // Whether the SAME write made again once the queue drains may succeed
    // (QUEUE_FULL: `engine-db.js` waits on it up to the app's deadline), with
    // the bound it met (sdk#180; R-b).
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
