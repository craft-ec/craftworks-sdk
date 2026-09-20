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

use craftworks_sdk::CachedStore;
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
    store: CachedStore,
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
        // Built here so the URL's `encodingProtocol=native` cannot be lost by
        // a page assembling its own: the node falls back to a different
        // encoding without it and says nothing that names the cause.
        wire::ws_url("127.0.0.1", port).map_err(|e| JsError::new(&e))?;
        Ok(Session {
            store: CachedStore::new(Box::new(crate::js_now_ms)),
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
                    if let Ok(protocol::Reply::Identity { head_writable, .. }) =
                        protocol::decode_reply(&m)
                    {
                        self.plan.on_identity(head_writable);
                        self.note_progress();
                    }
                    self.store.on_inbound(&m);
                }
            }
            Incoming::Ack(kind) => self.on_ack(kind),
            Incoming::Refused(why) => {
                // The node's wording, kept for display. Nothing branches on
                // it: the node chose it, and a reason that steers control flow
                // is an input from a stranger.
                self.plan.on_refused(&why.said);
            }
            Incoming::HeadChanged { .. } => {
                // A HINT. What it triggers is the same reload a tick does, so
                // a fabricated one costs a reload and can change nothing on
                // screen that the data does not verify.
            }
            Incoming::Unusable(why) => self.unusable.push(format!("{why:?}")),
            Incoming::Partial => {}
        }
    }

    fn on_ack(&mut self, kind: AckKind) {
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
        for bytes in self.store.take_outbound() {
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
    }

    /// The client's timer: roll back writes with no verdict, notice stalls.
    pub fn tick(&mut self) -> String {
        let now = crate::js_now_ms();
        let told = self.store.copy.time_out(now);
        let stalled = self.plan.tick(now);
        serde_json::json!({
            "rolledBack": told.rolled_back.len(),
            "stalled": stalled.map(|s| format!("{s:?}")),
        })
        .to_string()
    }
}
