//! # page-io: the page executor's I/O over Freenet's client API (ruling B, part 2)
//!
//! [`page::server::Server`] answers the page ↔ engine protocol in-process and
//! emits [`page::Op`]s; this crate is the ONLY place they become client-API
//! frames (`wire`, `wire::signer`) and the node's answers become
//! [`page::Answer`]s again (main's condition 3: no other path to the node).
//! Sans-IO like everything under it: frames in, frames out, and the time.
//!
//! | op | frame |
//! |---|---|
//! | `Put { id, bytes }` | PUT of the Block contract for `id` (`wire::block`), state `kind ‖ body` |
//! | `Get { id }` | GET of that contract |
//! | `ReadHead` | GET **with subscribe** of the head Register: on a peered node a delegate-created register is not served to a bare GET (F55), and the subscription is how a head move reaches the page |
//! | `Update { state }` | the first head this page has never seen a register for → PUT of the Register contract (it CREATES it); after that, UPDATE |
//! | `Sign { id, .. }` | `wire::signer::frame_sign` under the request's own id (SG02) |
//! | `AskHeld { id }` | `wire::signer::frame_held` of the block's contract |
//!
//! | answer | becomes |
//! |---|---|
//! | `Got` of the Register | `Head(record)`; the register now exists |
//! | NotFound of the Register | `Head(None)` ONLY if the signer holds no record for it (asked once, `ask_record`); otherwise silence — re-asked on the RTO with no end, shown as "not answering for N s" (sdk#175; a peered NotFound can be false, F55) |
//! | a REFUSED GET (`ContractError::Get`) of anything | nothing: it says nothing about the contract, so it is not an answer — re-asked on the RTO (rule 7), never read as absent |
//! | `Got` of a block | `Got { id, body }` (the executor verifies it against its id) |
//! | NotFound of a block | `GetMissed` |
//! | `Ack(Put)` of a block | `PutOk` |
//! | `Ack(Put)` / `Ack(Updated)` of the Register | `Updated` (it says nothing more, F56); the register exists |
//! | a signer answer | `Signer { id, answer }`; a `Held { present }` goes back to the blocks its id asked about |
//! | `Ack(Put)` / `PutFailed` of any OTHER contract | handed back unread ([`PageIo::take_others`]): the app's own PUTs ([`PageIo::put_contract`], builder#104) are the caller's to match |
//!
//! THE FIRST-PUT RACE: two pages on one key both see no register and both
//! PUT it. The signer signs ONE record from the genesis (at most one
//! signature per prev), so the second page is answered `AlreadySigned` with
//! the FIRST record and PUTs the same bytes: an equal decision, the held
//! bytes win (F56 only ever displaced DIFFERENT roots). Neither PUT fails;
//! the loser's own commit is judged by the read-back and told `Lost`.

use freenet_prolly::Cid;
use freenet_stdlib::prelude::*;
use page::server::{Server, SignerFacts};
use page::{Answer, Ext, Label, Ms, Op, Publication};
use std::collections::BTreeMap;
use wire::{DelegateKey, Incoming};

/// What the page needs to know about the platform: the Block contract's code
/// (a page PUT carries it), the head Register's code and params, and the
/// signer delegate's key.
pub struct Artefacts {
    pub block_code: Vec<u8>,
    pub register_code: Vec<u8>,
    pub register_params: Vec<u8>,
    pub signer: DelegateKey,
}

/// What the node's signer said to [`PageIo::ask`]: whose node this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asked {
    /// It signs for this Register (instance id): the node holds this key.
    Register([u8; 32]),
    /// The signer is there and holds no key.
    NoKey,
    /// The node has no signer delegate at all (a node that never opened this
    /// person's tree): measured on 0.2.136, it answers the request EMPTY.
    /// In its words.
    NoSigner(String),
    /// The node or the signer refused: in their words.
    Refused(String),
}

/// What every refusal of a view says first (a reader, `PageIo::reader`).
pub const READ_ONLY: &str = "read-only: this is a view of somebody's published data, and writing needs write access";

/// MAY THIS PAGE WRITE a head ([`PageIo::may_write`]): the ONE decision
/// inputs are shown from and writes are refused by (DATA-SOURCE; the
/// architect's point 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MayWrite {
    Yes,
    No(String),
    /// The signer ANSWERED and did not say yes or no for this head (it
    /// refused to say, or refused the provisioning): inputs are shown
    /// DISABLED, with this reason.
    Unknown(String),
    /// NOT DECIDED YET: the signer has not answered (still asking, or silent
    /// -- rule 8, silence is not an answer). A write WAITS for the answer;
    /// inputs are shown disabled meanwhile, with this reason.
    Undecided(String),
}

/// The head subscription as page-io can honestly report it (sdk#259).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadSubscription {
    /// The head was read with `subscribe`.
    pub asked: bool,
    /// The node answered that read: it holds this page's subscription.
    pub answered: bool,
    /// Head moves the subscription has delivered.
    pub changes: usize,
    /// Head reads answered with a failure (no head yet, a refusal, or F55).
    pub failed: usize,
    /// Opening ended, in words: no head read is coming.
    pub ended: Option<String>,
}

/// What a page tells its app about being kept up to date (sdk#259): the
/// MAPPING from the facts above to `LiveMode`, here — beside the facts, and
/// natively testable — rather than inside the web Session, which no native
/// test can build (a mutant that made "subscribed" unreachable there survived
/// every suite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveMode {
    /// `HeadSubscribed` or `Polled`.
    pub mode: &'static str,
    /// Why not, in words; empty when subscribed.
    pub why: String,
    /// Head moves the subscription has delivered.
    pub head_changes: usize,
}

impl LiveMode {
    /// A connection with no page yet.
    pub fn no_page() -> LiveMode {
        LiveMode { mode: "Polled", why: "there is no page on this connection yet".into(), head_changes: 0 }
    }
}

impl HeadSubscription {
    /// The report, in the order a reader should look: ANSWERED wins over
    /// everything (a failure counted before the head existed does not undo a
    /// subscription the node now holds); then an opening that ENDED; then
    /// head reads the node FAILED; then asked-and-unanswered; then unasked.
    pub fn live_mode(&self) -> LiveMode {
        let tick = " The tick keeps the data right meanwhile.";
        let (mode, why) = if self.answered {
            ("HeadSubscribed", String::new())
        } else if let Some(said) = &self.ended {
            ("Polled", format!("opening ended, so the head is never read: {said}"))
        } else if self.failed > 0 {
            (
                "Polled",
                format!(
                    "the node answered the head read with a failure {} time(s) — no head yet, a refusal, or a peered node's false NotFound (F55); it is asked again.{tick}",
                    self.failed
                ),
            )
        } else if self.asked {
            ("Polled", format!("the head read with subscribe has not been answered yet.{tick}"))
        } else {
            ("Polled", "the head has not been read on this connection yet".to_string())
        };
        LiveMode { mode, why, head_changes: self.changes }
    }
}

/// A site in flight: its contract (code + relabelled params), id and key, and its web part.
struct Site {
    contract: ContractContainer,
    id: [u8; 32],
    key: String,
    web: Vec<u8>,
}

/// Why a site cannot be published or linked yet: the node's signer has not named the Register (the key) this
/// page signs under, so there is no authority to relabel. Not a "no": it waits on the signer's answer.
pub const NO_REGISTER_YET: &str = "not yet: the node's signer has not said which key this head signs under";

/// A site's contract: `site_code` under `app`'s site params, derived from the person's Register params
/// (`contract_keys::site::site_params`, the ONE relabelling). `None` for a bad app id or a keyset that is not
/// mode 0. Its id is the site's stable link (builder#117).
pub fn site_contract(site_code: &[u8], register_params: &[u8], app: &str) -> Option<ContractContainer> {
    let params = contract_keys::site::site_params(register_params, app)?;
    Some(ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
        std::sync::Arc::new(ContractCode::from(site_code.to_vec())),
        Parameters::from(params),
    ))))
}

pub struct PageIo {
    pub server: Server,
    art: Artefacts,
    register: ContractContainer,
    register_id: [u8; 32],
    register_key: String,
    /// The Register exists on the node: this page read it, or its PUT was
    /// answered. Until then a head goes out as a PUT, which creates it.
    register_seen: bool,
    /// THE HEAD SUBSCRIPTION, as page-io can honestly report it (sdk#259):
    /// whether the GET-with-subscribe has been SENT, whether the node has
    /// ANSWERED it, and how many head moves it has delivered. A page that
    /// believed it was being notified while it was polling is the failure
    /// `LiveMode` exists to make impossible, and on the page path the
    /// subscription is page-io's, not the Session's.
    head_asked: bool,
    head_answered: bool,
    head_changes: usize,
    /// Head reads the node answered with a FAILURE: no head yet, a refusal,
    /// or a peered node's false NotFound (F55) — page-io cannot tell which,
    /// and says so rather than guessing (sdk#259).
    head_failed: usize,
    /// Blocks in flight, by contract id and by key string (a PUT's answer
    /// names the key).
    by_contract: BTreeMap<[u8; 32], Cid>,
    by_key: BTreeMap<String, Cid>,
    /// `Held` asks in flight, by signer request id: the blocks, in order.
    held: BTreeMap<u32, Vec<Cid>>,
    next_held_id: u32,
    frames: wire::Reassembler,
    stream: u32,
    out: Vec<Vec<u8>>,
    replies: Vec<Vec<u8>>,
    unusable: Vec<String>,
    /// The signer answered `Provisioned`: it holds the key and the naming.
    provisioned: bool,
    /// Does the SIGNER hold a record for this register (main's ruling on
    /// sdk#175)? `None`: not asked yet. Only a signer with NO record — and no
    /// head it can read — makes a failed head read "no head"; otherwise the
    /// head exists and a NotFound (which a peered node can answer FALSELY,
    /// F55) is only silence.
    signer_has_record: Option<bool>,
    /// A failed head read waiting on that answer.
    head_failed_pending: bool,
    /// Node answers about contracts that are not this page's register or
    /// blocks — the app's own PUTs — handed back unread (`take_others`).
    others: Vec<Incoming>,
    /// The app's PUTs by contract key: the exact contract and state, framed
    /// again whenever the page re-sends [`Op::PutApp`].
    app_contracts: BTreeMap<String, (ContractContainer, WrappedState)>,
    /// SITES being published (builder#117), by app: the site contract and the web part the page's record is
    /// framed around. The page owns the publication; these bytes are page-io's only while it is `Publishing`
    /// and are dropped when it ends (no app PUT entry: the site's PUT is the page's `Update`).
    sites: BTreeMap<String, Site>,
    /// The last time the node or the clock spoke, for answers made locally.
    now: Ms,
    /// A reader's stream-id range (`reader`), in the top byte; 0 for the
    /// person's own page, which keeps the whole space below it.
    stream_base: u32,
    /// `begin` asked the signer which Register it signs for, and it holds
    /// none: the caller mints a key and calls `provision_with`.
    needs_key: bool,
    /// The Provision in flight carries a key THIS page minted (`provision_with`
    /// after "no key"), not one it was given: a `KeyAlreadyProvisioned` then
    /// means another page provisioned first, and this one opens that Register
    /// (sdk#343).
    minted: bool,
    /// The signer's registration was answered (its `Ack(Registered)`).
    signer_registered: bool,
    /// The signer's FIRST request — the Register query (`begin`) or
    /// Provision (`provision`) — and whether it is out. HELD until the
    /// registration is answered: sent together with it, the node answered it
    /// with an EMPTY response 7 times in 12 (#260, measured), and the page
    /// waited for ever.
    /// SENT, RE-SENT AND ANSWERED THROUGH THE PAGE'S SENDER (rule 5): the
    /// registration ([`Ext::RegisterSigner`]) and this first request
    /// ([`Ext::SignerFirst`]) wait on the page's deadline and RTO until the
    /// node answers (rules 7, 8) — no timer, count or budget of page-io's own.
    first: Option<First>,
    /// The signer's container, framed again whenever the page re-sends its
    /// registration.
    signer_container: Option<DelegateContainer>,
    /// WHY OPENING ENDED, by name (what `open()` reports): the signer's or the
    /// node's refusal of the first exchange, in its words — a real answer,
    /// never time.
    refused: Option<String>,
    /// [`PageIo::ask`]: this page only ASKS the node's signer which Register
    /// it holds — nothing is registered, minted or provisioned — and the
    /// answer ends here ([`Asked`]).
    asking: bool,
    asked: Option<Asked>,
    /// [`PageIo::claim`]: this asked page opens the person's OWN tree after
    /// all. The answer stays what it was; the page carries on as `begin`'s.
    claimed: bool,
}

/// The signer's first request, kept so it can be sent once the registration
/// is answered, and again after an empty response.
enum First {
    /// "Which Register do you sign for?" (`begin`).
    Query,
    /// Provision with this signing key (`provision`).
    Provision(Vec<u8>),
}

/// The counter part of a reader's stream id; the top byte is its range.
const STREAM_COUNTER: u32 = 0x00FF_FFFF;

/// The id the record query goes out under.
const RECORD_QUERY_ID: u32 = (1 << 31) - 2;

/// The id the "which Register do you sign for?" query goes out under (`begin`).
const REGISTER_QUERY_ID: u32 = (1 << 31) - 3;

/// A root no node holds: the record query names it so that nothing can be
/// signed (see `ask_record`).
const UNHELD_ROOT: [u8; 32] = [0xA5; 32];

/// The id a provisioning request goes out under: far from the executor's own
/// sign ids (from 1) and from the `Held` ids (from 2³¹).
const PROVISION_ID: u32 = (1 << 31) - 1;

/// The page's I/O as the store's host (READ-STATE): a call on the server is
/// carried out at once — its node ops framed, its replies kept for the client.
impl page::server::Host for PageIo {
    fn with_server<R>(&mut self, f: impl FnOnce(&mut Server) -> R) -> R {
        let r = f(&mut self.server);
        self.pump();
        r
    }
    fn peek<R>(&self, f: impl FnOnce(&Server) -> R) -> R {
        f(&self.server)
    }
    fn client(&mut self, frame: &[u8]) {
        PageIo::client(self, frame);
    }
    fn take_replies(&mut self) -> Vec<Vec<u8>> {
        PageIo::take_replies(self)
    }
}

impl PageIo {
    pub fn new(server: Server, art: Artefacts) -> PageIo {
        let register = ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
            std::sync::Arc::new(ContractCode::from(art.register_code.clone())),
            Parameters::from(art.register_params.clone()),
        )));
        let mut register_id = [0u8; 32];
        register_id.copy_from_slice(&register.key().id().as_bytes()[..32]);
        let register_key = register.key().to_string();
        PageIo {
            server,
            art,
            register,
            register_id,
            register_key,
            register_seen: false,
            head_asked: false,
            head_answered: false,
            head_changes: 0,
            head_failed: 0,
            by_contract: BTreeMap::new(),
            by_key: BTreeMap::new(),
            held: BTreeMap::new(),
            // High, so it never meets the executor's own sign ids (from 1).
            next_held_id: 1 << 31,
            frames: wire::Reassembler::default(),
            stream: 0,
            out: Vec::new(),
            replies: Vec::new(),
            unusable: Vec::new(),
            provisioned: false,
            signer_has_record: None,
            head_failed_pending: false,
            others: Vec::new(),
            app_contracts: BTreeMap::new(),
            sites: BTreeMap::new(),
            now: Ms(0),
            stream_base: 0,
            needs_key: false,
            minted: false,
            signer_registered: false,
            first: None,
            signer_container: None,
            refused: None,
            asking: false,
            asked: None,
            claimed: false,
        }
    }

    /// A READER of somebody's published head (sdk#239): the page reads the
    /// Register `register_id` names and the blocks under it, and can do
    /// nothing else. Published data is readable by default; writing is access
    /// control, which a reader does not have.
    ///
    /// * NO SIGNER, and nothing provisioned or registered on the node: a
    ///   reader leaves no trace on the node it reads from beyond the blocks
    ///   it caches and its subscription to the head.
    /// * The head is read by GET with subscribe (a peered node serves a bare
    ///   GET of a delegate-created register falsely, F55; and the subscription
    ///   carries the publisher's next head here).
    /// * A failed head read is SILENCE, re-asked on the RTO — never "no head":
    ///   the head was named because it was published.
    /// * `Put`, `Update` and `Sign` are never framed. `AskHeld` (is a block
    ///   local to the signer?) is answered here: not held, so it is fetched.
    ///
    /// `range` (1..=255) is this reader's stream-id range on the socket it
    /// shares: one per tree, so their chunked frames can never be joined.
    pub fn reader(server: Server, block_code: Vec<u8>, register_id: [u8; 32], range: u8) -> PageIo {
        let (_, no_signer) = wire::delegate_from_code(&[]);
        let mut io = PageIo::new(
            server,
            Artefacts { block_code, register_code: Vec::new(), register_params: Vec::new(), signer: no_signer },
        );
        io.register_id = register_id;
        io.register_key = wire::contract_id(register_id).to_string();
        io.server.page.set_read_only();
        io.stream_base = u32::from(range.max(1)) << 24;
        io.provisioned = true;
        // "The head exists": a failed read of it is silence, and the signer
        // is never asked (there is none).
        io.signer_has_record = Some(true);
        io.server.set_facts(SignerFacts { head_writable: false, head_id: register_id });
        io
    }

    /// WHOSE NODE IS THIS? Ask the node's EXISTING signer which Register it
    /// signs for — the same query [`PageIo::begin`] sends, on the same first-
    /// request clock — WITHOUT registering it first. On the node that holds
    /// a person's key the answer names their Register; anywhere else the node
    /// has no such delegate (refused), or it holds no key: either way nothing
    /// is installed, minted or provisioned, and a reader leaves no trace.
    /// The answer: [`PageIo::asked`].
    pub fn ask(&mut self) {
        self.asking = true;
        // Not registered by this page, and never will be: the query goes out
        // at once, and a node without the delegate says so.
        self.signer_registered = true;
        self.first = Some(First::Query);
        self.server.page.send_ext(Ext::SignerFirst, self.now);
        self.pump();
    }

    /// [`PageIo::ask`]'s answer, once there is one.
    pub fn asked(&self) -> Option<&Asked> {
        self.asked.as_ref().filter(|_| self.asking)
    }

    /// MAY THIS PAGE WRITE `head` (`None`: its OWN tree, a `mine`
    /// component's)? Derived each time from what this page holds -- the
    /// signer's answer, the opening -- and kept nowhere else.
    ///
    /// A display gate, not the enforcement: the signer refuses to sign for
    /// anyone else whatever this says, so a wrong answer can only show or hide
    /// inputs. "Yes" means THIS NODE'S SIGNER SIGNS FOR that head; a keyset
    /// (a device key among an identity's keys) is Phase 6.
    pub fn may_write(&self, head: Option<[u8; 32]>) -> MayWrite {
        if self.read_only() {
            return MayWrite::No(READ_ONLY.into());
        }
        let other = |r: &[u8; 32]| MayWrite::No(format!("this node signs for another head ({})", hex(r)));
        if self.asking() {
            return match (&self.asked, head) {
                (None, _) => MayWrite::Undecided("asking this node's signer whose node it is".into()),
                (Some(Asked::Register(_)), None) => MayWrite::Yes,
                (Some(Asked::Register(r)), Some(h)) if h == *r => MayWrite::Yes,
                (Some(Asked::Register(r)), Some(_)) => other(r),
                // Nothing of this person's here yet: their own tree is made on
                // first use (a key minted, the head created by the first
                // write); anyone else's head is not theirs to write.
                (Some(Asked::NoKey | Asked::NoSigner(_)), None) => MayWrite::Yes,
                (Some(Asked::NoKey | Asked::NoSigner(_)), Some(_)) => MayWrite::No("this node holds no key for that head".into()),
                (Some(Asked::Refused(w)), None) => MayWrite::Unknown(w.clone()),
                (Some(Asked::Refused(w)), Some(_)) => MayWrite::No(w.clone()),
            };
        }
        if let Some(r) = self.refused.as_ref() {
            return if head.is_none() { MayWrite::Unknown(r.clone()) } else { MayWrite::No(r.clone()) };
        }
        if self.provisioned {
            return match head {
                None => MayWrite::Yes,
                Some(h) if h == self.register_id => MayWrite::Yes,
                Some(_) => other(&self.register_id),
            };
        }
        // Still opening its own tree: a write waits in the engine for the
        // head, as it always has; another head is not known to be ours yet.
        match head {
            None => MayWrite::Yes,
            Some(_) => MayWrite::Undecided("opening: this node's signer has not said whose node it is yet".into()),
        }
    }

    /// Was this page only asked (`ask`), and not (yet) claimed?
    pub fn asking(&self) -> bool {
        self.asking && !self.claimed
    }

    /// THE USER HAS NO TREE HERE YET (DATA-SOURCE `mine`: made on the first
    /// WRITE): an asked page whose node's signer holds no key, or has no
    /// signer, and which has not been provisioned. Its tree reads as EMPTY --
    /// its head read is answered "missing" here, and nothing goes to the node
    /// -- until a first write provisions it ([`PageIo::claim`], then the
    /// existing provision path).
    pub fn no_tree_yet(&self) -> bool {
        self.asking && !self.provisioned && matches!(self.asked, Some(Asked::NoKey | Asked::NoSigner(_)))
    }

    /// OPEN THE PERSON'S OWN TREE ON AN ASKED PAGE (DATA-SOURCE `mine`):
    /// the same opening as [`PageIo::begin`], from where the answer left it.
    /// - It signs for a Register: that is this person's tree on this node,
    ///   opened as it is. Nothing is registered or minted.
    /// - It holds no key: [`PageIo::needs_key`], and the caller mints one
    ///   (`provision_with`), as `begin`'s.
    /// - There is no signer here: it is registered and asked, as `begin`'s.
    ///
    /// Refused, or not answered yet: nothing is claimed, and
    /// `false` says so. The head itself is created on the first write, by
    /// the first commit's PUT (as `begin`'s).
    pub fn claim(&mut self, signer: DelegateContainer) -> bool {
        if !self.asking() {
            return self.asking && self.claimed;
        }
        match self.asked.clone() {
            Some(Asked::Register(_)) => {
                self.claimed = true;
                self.provisioned = true;
                self.signer_provisioned();
                true
            }
            Some(Asked::NoKey) => {
                self.claimed = true;
                self.needs_key = true;
                true
            }
            Some(Asked::NoSigner(_)) => {
                self.claimed = true;
                self.signer_registered = false;
                self.register_signer(signer, First::Query);
                true
            }
            Some(Asked::Refused(_)) | None => false,
        }
    }

    /// OPEN THE PERSON'S OWN TREE (a switch-over blocker): register the signer
    /// and ASK it which Register it signs for, before anything is minted. A
    /// page that minted a key on every load would be a new identity after
    /// every reload and in every second tab. The answer either names the
    /// Register — this page opens it, and nothing is provisioned — or says the
    /// signer holds no key: [`PageIo::needs_key`], and the caller mints one
    /// and calls [`PageIo::provision_with`]. The key never leaves the signer.
    pub fn begin(&mut self, signer: DelegateContainer) {
        self.register_signer(signer, First::Query);
    }

    /// Register the signer ALONE; its first request goes once the node has
    /// answered the registration (`inbound`, `Ack(Registered)`).
    fn register_signer(&mut self, signer: DelegateContainer, first: First) {
        self.signer_container = Some(signer);
        self.first = Some(first);
        self.server.page.send_ext(Ext::RegisterSigner, self.now);
        self.pump();
    }

    /// The signer holds no key (`begin`'s answer): mint one and `provision_with` it.
    pub fn needs_key(&self) -> bool {
        self.needs_key
    }

    /// Provision a signer that holds no key, for the Register `register_params`
    /// names (the minted key's). Only after `begin` said it needs one.
    pub fn provision_with(&mut self, signing_key: Vec<u8>, register_params: Vec<u8>) {
        if !std::mem::take(&mut self.needs_key) {
            self.unusable.push("provision_with: the signer was not asked, or already holds a key".into());
            return;
        }
        self.set_register(register_params);
        self.minted = true;
        // The signer is registered already (it answered the query): the
        // Provision is the first request now, sent and re-sent like one.
        self.first = Some(First::Provision(signing_key));
        self.server.page.send_ext(Ext::SignerFirst, self.now);
        self.pump();
    }

    /// Name the Register this page's head lives in.
    fn set_register(&mut self, params: Vec<u8>) {
        let register = ContractContainer::from(ContractWasmAPIVersion::V1(WrappedContract::new(
            std::sync::Arc::new(ContractCode::from(self.art.register_code.clone())),
            Parameters::from(params.clone()),
        )));
        self.register_id.copy_from_slice(&register.key().id().as_bytes()[..32]);
        self.register_key = register.key().to_string();
        self.register = register;
        self.art.register_params = params;
    }

    /// THE SEQ THIS PAGE'S HEAD IS PUBLISHED AT, as the network acknowledged
    /// it: the page's published seq, which moves only when the register's
    /// read-back shows this page's own head (`HeadConfirmed`) or a head is
    /// read from the network. A commit signed or in flight, or one that is
    /// later Lost, never moves it -- so a publisher that records it (an app's
    /// published-head floor, sdk#349) names a head that exists. 0: none.
    pub fn published_seq(&self) -> u64 {
        self.server.page.published().0
    }

    /// A reader of a named head (`reader`): nothing can be written.
    pub fn read_only(&self) -> bool {
        // THE PAGE's: the one owner of "read-only" (it makes no commit op).
        self.server.page.read_only()
    }

    /// PUT a contract the APP names (builder#104: a web container). Framed
    /// here, on this page's stream counter, because this is the only path to
    /// the node (main's condition 3) and two chunked requests on one stream id
    /// would be reassembled into each other. The answer comes back through
    /// [`PageIo::take_others`], named by the contract's key.
    pub fn put_contract(&mut self, contract: ContractContainer, state: WrappedState, now: Ms) -> Result<(), String> {
        if self.read_only() {
            return Err("read-only: a reader PUTs nothing".into());
        }
        let key = contract.key().to_string();
        self.app_contracts.insert(key.clone(), (contract, state));
        // THE PAGE SENDS IT (its deadline, re-send and end), like every op.
        self.server.page.put_app(key, now);
        self.pump();
        Ok(())
    }

    /// PUBLISH `web` as `app`'s site (builder#117): the page reads the site, signs the next version through the
    /// ONE head path, PUTs the record framed around `web`, and reads it back ([`PageIo::publication`]). A reader,
    /// a bad app id or a Register that is not this person's own key publishes nothing, by name.
    pub fn publish_site(&mut self, app: &str, site_code: &[u8], web: Vec<u8>, now: Ms) -> Result<(), String> {
        self.now = now;
        if self.read_only() {
            return Err("read-only: a reader publishes nothing".into());
        }
        if self.art.register_params.is_empty() {
            return Err(NO_REGISTER_YET.into());
        }
        let Some(contract) = site_contract(site_code, &self.art.register_params, app) else {
            return Err(format!("no site for {app:?}: not an app id, or this head has no single key to sign it"));
        };
        let mut id = [0u8; 32];
        id.copy_from_slice(&contract.key().id().as_bytes()[..32]);
        let key = contract.key().to_string();
        let value = *blake3::hash(&web).as_bytes();
        self.sites.insert(app.to_string(), Site { contract, id, key, web });
        self.server.page.publish_site(app, value, now);
        self.pump();
        Ok(())
    }

    /// `app`'s site LINK: its contract's instance id, as the node serves it (`/v1/contract/web/<link>/`). The same
    /// for every publish (builder#117). `None`: not an app id, or this head has no single key.
    pub fn site_link(&self, site_code: &[u8], app: &str) -> Option<String> {
        site_contract(site_code, &self.art.register_params, app).map(|c| c.key().id().encode())
    }

    /// Has the node's signer named the Register this page signs under (so a site has an authority)?
    pub fn register_params_known(&self) -> bool {
        !self.art.register_params.is_empty()
    }

    /// How `app`'s site publication stands: the page's, the one owner. `None`: never published here.
    pub fn publication(&self, app: &str) -> Option<&Publication> {
        self.server.page.publication(app)
    }

    /// The person cancels `app`'s publication: it ends `Cancelled` and its bytes go.
    pub fn cancel_site(&mut self, app: &str) {
        self.server.page.cancel_site(app);
        self.pump();
    }

    /// The site in flight whose contract id is `id`, by app.
    fn site_by_id(&self, id: &[u8; 32]) -> Option<String> {
        self.sites.iter().find(|(_, s)| s.id == *id).map(|(a, _)| a.clone())
    }

    /// The site in flight whose contract key is `key`, by app.
    fn site_by_key(&self, key: &str) -> Option<String> {
        self.sites.iter().find(|(_, s)| s.key == key).map(|(a, _)| a.clone())
    }

    /// A person cancels the pending PUT of `key` (the page's, named).
    pub fn cancel_app_put(&mut self, key: &str) {
        self.server.page.cancel_app_put(key);
    }

    /// Where the app's PUT of `key` stands (the page's [`page::AppPut`]).
    pub fn app_put(&self, key: &str) -> Option<&page::AppPut> {
        self.server.page.app_put(key)
    }

    /// Node answers this page did not own (see `others`), oldest first.
    pub fn take_others(&mut self) -> Vec<Incoming> {
        std::mem::take(&mut self.others)
    }

    /// PROVISION the signer: register its delegate (from its wasm) and hand it
    /// the head's key, the Register it signs for and the Block code. The key
    /// is the page's TEST key, minted by the caller and forgotten after this
    /// (real device keys are sdk#14). `provisioned()` turns true when the
    /// signer answers `Provisioned`.
    pub fn provision(&mut self, signer: DelegateContainer, signing_key: Vec<u8>) {
        // A READER installs nothing on the node it reads from (sdk#239).
        if self.read_only() {
            self.unusable.push("read-only: provisioning refused — a reader installs nothing".into());
            return;
        }
        self.register_signer(signer, First::Provision(signing_key));
    }

    /// The signer said it holds the key and the naming.
    pub fn provisioned(&self) -> bool {
        self.provisioned
    }

    /// THE SOCKET WAS REPLACED (sdk#376). Everything that belonged to the old
    /// connection is dropped or asked again, in THIS call, so the frames leave
    /// with the caller's next `take_frames`:
    /// - the reassembly of a chunked reply half-received on the old socket (its
    ///   stream ids restart; joined to the new connection's it would decode as a
    ///   message nobody sent);
    /// - the head subscription: the node held it for the old connection, so it
    ///   is not "answered" on this one until the re-sent head read is
    ///   (`Page::reconnected` re-sends it: a GET with subscribe, the one path).
    pub fn reconnected(&mut self, now: Ms) {
        self.frames = wire::Reassembler::default();
        self.head_answered = false;
        self.server.page.reconnected(now);
        self.tick(now);
    }

    /// The head Register's instance id (what `Identity` reports as `head_id`).
    /// THE HEAD SUBSCRIPTION AS IT REALLY IS (sdk#259): asked, answered, and
    /// how many head moves it has delivered. The page path's `LiveMode` is
    /// built from this — the Session's own `subscribed`/`watching` belong to
    /// the delegate path, which no longer exists, so reporting from them said
    /// "Polled" on a page that was subscribed the whole time.
    pub fn head_subscription(&self) -> HeadSubscription {
        HeadSubscription {
            asked: self.head_asked,
            answered: self.head_answered,
            changes: self.head_changes,
            failed: self.head_failed,
            // Opening ended — refused in someone's words, or its re-asks spent:
            // there is no head read coming, so no subscription either.
            ended: self.refused.clone(),
        }
    }

    pub fn register_id(&self) -> [u8; 32] {
        self.register_id
    }

    /// Tell the server what the signer holds, once it is provisioned.
    pub fn signer_provisioned(&mut self) {
        if self.read_only() {
            return;
        }
        self.server.set_facts(SignerFacts { head_writable: true, head_id: self.register_id });
        // The user's own tree was NOT yet theirs to sign (`no_tree_yet`)
        // and now is: the queue held while it could not sign is cut, as ONE
        // commit.
        if self.asking && matches!(self.asked, Some(Asked::NoKey | Asked::NoSigner(_))) {
            self.step_can_sign();
        }
    }

    /// Tell the engine whether this page can SIGN its head now
    /// (`engine::Event::CanSign`): derived from the ONE signer decision at
    /// each of its changes -- the answer "no key here" / "no signer here"
    /// (it cannot), and that tree's provisioning (it can). Kept nowhere else.
    fn step_can_sign(&mut self) {
        let can = !self.no_tree_yet();
        self.server.page.event(engine::Event::CanSign(can));
    }

    /// A client protocol frame (what `sdk::Client` would have sent a delegate).
    pub fn client(&mut self, bytes: &[u8]) {
        self.server.client(bytes);
        self.pump();
    }

    /// A frame from the node.
    ///
    /// Returns whether the frame was THIS page's — what it asked for, by
    /// contract id or key. One socket can carry several pages (a person's own
    /// tree and the trees they read, sdk#239): each is offered every frame and
    /// takes only its own, and a frame nobody takes is counted once, by the
    /// caller. So a frame that is not this page's is left alone here, not
    /// counted as unusable.
    pub fn inbound(&mut self, bytes: &[u8], now: Ms) -> bool {
        self.now = now;
        let incoming = wire::unframe(&mut self.frames, bytes);
        if !self.owns(&incoming) {
            return false;
        }
        match incoming {
            Incoming::Got { id, state } => {
                if id == self.register_id {
                    self.register_seen = true;
                    // The GET that carried `subscribe` was answered: the node
                    // holds this page's subscription to the head (sdk#259).
                    self.head_answered = true;
                    // The head WHOLE (root ‖ ledger), tolerantly: the root is
                    // the value's first 32 bytes whatever ledger follows.
                    self.server.node(Answer::Head { label: Label::Head, read: page::HeadRead::from_record(&state) }, now);
                } else if let Some(app) = self.site_by_id(&id) {
                    // A site's record is its framing's META. A state that does not frame is no answer (the site
                    // contract admits none): named, and the read stays silent, re-asked on the RTO.
                    match contract_keys::site::framing(&state) {
                        Some((meta, _)) => {
                            let read = page::HeadRead::from_record(meta);
                            self.server.node(Answer::Head { label: Label::Site(app), read }, now)
                        }
                        None => self.unusable.push(format!("a site state for {app} that is not a web framing")),
                    }
                } else if let Some(cid) = self.by_contract.get(&id).copied() {
                    let body = wire::block::block_of_state(&state).map(|(_, b)| b.to_vec()).unwrap_or_default();
                    self.server.node(Answer::Got { id: cid, bytes: body }, now);
                } else {
                    self.unusable.push("a GET answer for a contract this page never asked".into());
                }
            }
            // A REFUSAL is not an answer about the contract: only the node's
            // NotFound says it is absent (#332 ruling). A refused GET stays
            // unanswered and the page's sender re-asks it on the RTO (rule 7);
            // read as absent, a head refusal on a signer with no record would
            // open an empty tree over an app that exists, and a block
            // refusal would end a read the next ask could serve.
            Incoming::GetFailed { id, why: wire::GetFail::Refused(_) } => {
                if id == self.register_id {
                    self.head_failed += 1;
                }
            }
            Incoming::GetFailed { id, why: wire::GetFail::NotFound } => {
                if let Some(app) = self.site_by_id(&id) {
                    // No site at this address: the genesis (only a NotFound says so).
                    self.server.node(Answer::Head { label: Label::Site(app), read: None }, now);
                } else if id == self.register_id {
                    self.head_failed += 1;
                    // The node's explicit NotFound for the head — which a
                    // PEERED node can answer falsely (F55) — is "no head" ONLY if the signer holds no
                    // record for this register. Otherwise the head exists and
                    // this is SILENCE: re-asked on the RTO with no end (rule 8),
                    // shown as "not answering for N s". Opening an empty tree over an existing app
                    // would have its first commit PUT a second register that
                    // F56 then merges against the real one (sdk#175).
                    match (self.register_seen, self.signer_has_record) {
                        (false, Some(false)) => self.server.node(Answer::Head { label: Label::Head, read: None }, now),
                        (false, None) => {
                            self.head_failed_pending = true;
                            self.ask_record();
                        }
                        _ => {}
                    }
                } else if let Some(cid) = self.by_contract.get(&id).copied() {
                    self.server.node(Answer::GetMissed(cid), now);
                }
            }
            Incoming::Ack(wire::AckKind::Put(key)) | Incoming::Ack(wire::AckKind::Updated(key))
                if key == self.register_key || self.by_key.contains_key(&key) =>
            {
                if key == self.register_key {
                    self.register_seen = true;
                    self.server.node(Answer::Updated { label: Label::Head }, now);
                } else if let Some(cid) = self.by_key.get(&key).copied() {
                    self.server.node(Answer::PutOk(cid), now);
                }
            }
            // THE SIGNER'S REGISTRATION ANSWERED — or, once it has been, an
            // EMPTY response to its outstanding first request. `wire::unframe`
            // maps ANY empty DelegateResponse to `Ack(Registered)`, so the two
            // look alike: the first is the registration, and any after it,
            // while the first request is out, is "no answer" — re-sent by the
            // page's sender on its RTO — never a second registration.
            //
            // Except when this page only ASKS: then it registered nothing, and
            // (measured on 0.2.136) a node WITHOUT the signer delegate answers
            // a request to it EMPTY — the node's own answer, "no such signer
            // here" (another user's node). A page that registered the signer itself
            // reads an EMPTY as a request that arrived before the registration
            // took (#260) — NOT an answer — and leaves it to the RTO.
            Incoming::Ack(wire::AckKind::Registered(key)) if key == self.art.signer.to_string() => {
                if !self.signer_registered {
                    self.signer_registered = true;
                    self.server.page.ext_answered(Ext::RegisterSigner, now);
                    self.server.page.send_ext(Ext::SignerFirst, now);
                } else if self.asking() && self.first.is_some() && self.server.page.ext_waiting(Ext::SignerFirst) {
                    self.first = None;
                    self.server.page.ext_answered(Ext::SignerFirst, now);
                    self.asked = Some(Asked::NoSigner("no signer on this node: it answered EMPTY".into()));
                    // What the engine may do now depends on it (#342: no tree yet → writes wait, unput).
                    self.step_can_sign();
                }
            }
            // A site's PUT was answered: the page reads it back (it says nothing about which record was kept).
            Incoming::Ack(wire::AckKind::Put(key)) | Incoming::Ack(wire::AckKind::Updated(key)) if self.site_by_key(&key).is_some() => {
                let app = self.site_by_key(&key).expect("matched");
                self.server.node(Answer::Updated { label: Label::Site(app) }, now)
            }
            Incoming::PutFailed { key, said } if self.site_by_key(&key).is_some() => {
                let app = self.site_by_key(&key).expect("matched");
                self.server.node(Answer::SiteRefused { app, said }, now)
            }
            // The app's PUT: the page ends its deadline.
            Incoming::Ack(wire::AckKind::Put(key)) if self.app_contracts.contains_key(&key) => {
                self.server.node(Answer::AppPutOk(key), now)
            }
            // A PUT answer nobody here sent: handed back unread.
            answer @ Incoming::Ack(wire::AckKind::Put(_)) => self.others.push(answer),
            // A refused PUT of our own register or block is what a refusal
            // naming nothing was before `PutFailed` existed: reported.
            Incoming::PutFailed { key, said } if key == self.register_key || self.by_key.contains_key(&key) => {
                self.unusable.push(format!("the node refused: {said}"))
            }
            Incoming::PutFailed { key, said } if self.app_contracts.contains_key(&key) => {
                self.server.node(Answer::AppPutRefused { key, said }, now)
            }
            answer @ Incoming::PutFailed { .. } => self.others.push(answer),
            Incoming::EngineBytes(msgs) => {
                for m in msgs {
                    let answer = wire::signer::read_answer(&m);
                    // A REAL answer to the signer's first request ends it: no
                    // more re-sends, and a later empty response is nobody's.
                    if matches!(answer, Some((REGISTER_QUERY_ID | PROVISION_ID, _))) {
                        self.first = None;
                        self.server.page.ext_answered(Ext::SignerFirst, now);
                    }
                    if matches!(answer, Some((RECORD_QUERY_ID, _))) {
                        self.server.page.ext_answered(Ext::AskRecord, now);
                    }
                    match answer {
                        // `begin`'s question: which Register? Named: open it,
                        // provisioned already. None: the caller mints a key.
                        // Only ASKED (`ask`): the answer is recorded, and
                        // nothing is opened, minted or provisioned.
                        Some((REGISTER_QUERY_ID, signer_proto::Answer::Register { params })) if self.asking() => {
                            self.asked = Some(match params {
                                Some(params) => {
                                    self.set_register(params);
                                    Asked::Register(self.register_id)
                                }
                                None => Asked::NoKey,
                            });
                            self.step_can_sign();
                        }
                        Some((REGISTER_QUERY_ID, signer_proto::Answer::Register { params })) => match params {
                            Some(params) => {
                                self.set_register(params);
                                // Whether it has SIGNED anything is the record
                                // query's to say (`ask_record`): provisioned is
                                // not "has a head".
                                self.provisioned = true;
                                self.signer_provisioned();
                            }
                            None => self.needs_key = true,
                        },
                        Some((_, signer_proto::Answer::Provisioned)) => {
                            self.provisioned = true;
                            self.signer_provisioned();
                        }
                        // A KEY IS HERE ALREADY (sdk#343): another page, told
                        // "no key" as this one was, provisioned first. An
                        // ANSWER, not an end: ask again which Register the
                        // signer holds and open THAT one -- one identity per
                        // node (rule 15); the key this page minted is dropped.
                        // Only for a minted key: a key the page was GIVEN
                        // (`provision`) is refused as before.
                        Some((PROVISION_ID, signer_proto::Answer::Refused(signer_proto::Why::KeyAlreadyProvisioned)))
                            if std::mem::take(&mut self.minted) =>
                        {
                            self.first = Some(First::Query);
                            self.server.page.send_ext(Ext::SignerFirst, now);
                        }
                        Some((PROVISION_ID, signer_proto::Answer::Refused(why))) => {
                            self.refused = Some(format!("the signer refused provisioning: {why:?}"));
                            self.unusable.push(format!("the signer refused provisioning: {why:?}"));
                        }
                        Some((REGISTER_QUERY_ID, signer_proto::Answer::Refused(why))) if self.asking() => {
                            self.asked = Some(Asked::Refused(format!("the signer refused to say which Register it signs for: {why:?}")));
                        }
                        Some((REGISTER_QUERY_ID, signer_proto::Answer::Refused(why))) => {
                            self.refused = Some(format!("the signer refused to say which Register it signs for: {why:?}"));
                            self.unusable.push(format!("the signer refused the register query: {why:?}"));
                        }
                        // The record query's answer (`ask_record`).
                        Some((RECORD_QUERY_ID, answer)) => {
                            use signer_proto::{Answer as A, Why};
                            let has = match answer {
                                // No record, no head it can read: only genesis
                                // would be signable, and the unheld root stops it.
                                A::Refused(Why::RootNotHeld) => Some(false),
                                // A record from genesis, or a later truth.
                                A::AlreadySigned(_) | A::NotNext { .. } | A::Refused(Why::Forked { .. }) => Some(true),
                                _ => None,
                            };
                            if let Some(h) = has {
                                self.signer_has_record = Some(h);
                                if !h && std::mem::take(&mut self.head_failed_pending) && !self.register_seen {
                                    self.server.node(Answer::Head { label: Label::Head, read: None }, now);
                                }
                            }
                        }
                        Some((id, signer_proto::Answer::Held { present })) => {
                            let asked = self.held.remove(&id).unwrap_or_default();
                            for (cid, p) in asked.into_iter().zip(present) {
                                self.server.node(Answer::Held { id: cid, present: p }, now);
                            }
                        }
                        Some((id, answer)) => self.server.node(Answer::Signer { id, answer }, now),
                        None => self.unusable.push("a delegate message that is not the signer's".into()),
                    }
                }
            }
            // The head moved on the node (the subscription the head read
            // took): the RELOAD TRIGGER. A hint only — the page READS the
            // register and adopts only what that read shows (sdk#225).
            Incoming::HeadChanged { key } if key == self.register_key => {
                // What the subscription DELIVERED: counted, so "subscribed"
                // can be told from "subscribed and being told" (sdk#259).
                self.head_changes += 1;
                self.server.head_hint();
            }
            Incoming::Refused(r) => {
                // While the first exchange is unanswered, a refusal that names
                // nothing is the node refusing IT: opening ends, by name.
                if self.first.is_some() {
                    self.first = None;
                    self.server.page.ext_answered(Ext::SignerFirst, now);
                    self.refused = Some(format!("the node refused: {}", r.said));
                    if self.asking() {
                        self.asked = Some(Asked::Refused(format!("the node refused: {}", r.said)));
                    }
                }
                self.unusable.push(format!("the node refused: {}", r.said))
            }
            Incoming::Unusable(u) => self.unusable.push(format!("{u:?}")),
            _ => {}
        }
        self.pump();
        true
    }

    /// Is this frame one this page asked for? A READER owns only its head and
    /// its blocks; a writer also owns its signer's answers, answers that name
    /// nothing, and PUT answers for the app's own contracts (handed back).
    fn owns(&self, incoming: &Incoming) -> bool {
        let mine = |id: &[u8; 32]| *id == self.register_id || self.by_contract.contains_key(id) || self.sites.values().any(|s| s.id == *id);
        let my_key = |k: &String| *k == self.register_key || self.by_key.contains_key(k) || self.sites.values().any(|s| s.key == *k);
        match incoming {
            Incoming::Got { id, .. } | Incoming::GetFailed { id, .. } => mine(id),
            Incoming::Ack(wire::AckKind::Put(k)) | Incoming::PutFailed { key: k, .. } => my_key(k) || !self.read_only(),
            Incoming::Ack(wire::AckKind::Updated(k)) | Incoming::Ack(wire::AckKind::Subscribed(k)) => my_key(k),
            // A site's change is its own (taken, and read by nobody: the page does not follow a site).
            Incoming::HeadChanged { key } => *key == self.register_key || self.sites.values().any(|s| s.key == *key),
            Incoming::Partial => true,
            Incoming::EngineBytes(_) | Incoming::Ack(_) | Incoming::Refused(_) | Incoming::Unusable(_) => !self.read_only(),
        }
    }

    /// The page's clock.
    pub fn tick(&mut self, now: Ms) {
        self.now = now;
        self.server.tick(now);
        self.pump();
    }

    /// Opening was REFUSED — by the signer or the node, in its words.
    pub fn refused(&self) -> Option<&str> {
        self.refused.as_deref()
    }

    /// The request that has waited longest for an answer, and for how long
    /// (ms): what a page shows as "not answering for N s" (rule 8). The
    /// page's sender keeps re-sending it; this never ends anything.
    pub fn not_answering(&self) -> Option<(String, u64)> {
        self.server.page.not_answering()
    }

    /// Still waiting on the first exchange past its first RTO.
    pub fn stalled(&self) -> bool {
        (self.server.page.ext_waiting(Ext::RegisterSigner) || self.server.page.ext_waiting(Ext::SignerFirst))
            && self.server.page.not_answering().is_some_and(|(_, ms)| ms >= page::rto::RTO_INITIAL_MS as u64)
    }

    /// When the page's next timer falls (a host arms a one-shot for it): the
    /// page's own deadlines, which the signer's requests are among.
    pub fn next_due(&self) -> Option<Ms> {
        self.server.page.next_due()
    }

    /// Frames for the node, in order.
    pub fn take_frames(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.out)
    }

    /// Protocol replies for the client, in order.
    pub fn take_replies(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.replies)
    }

    /// Writes the engine took forced past their reads (sdk#235).
    pub fn forced_writes(&self) -> u64 {
        self.server.forced_writes()
    }

    pub fn unusable(&self) -> &[String] {
        &self.unusable
    }

    /// Ask the signer whether it holds a record for this register, WITHOUT
    /// being able to sign anything: a sign request from the genesis naming a
    /// root no node holds. `signer::decide` answers it in this order —
    /// `AlreadySigned` if its record's prev is the genesis, `NotNext` if its
    /// record or a head it can read is the truth, and only then, with neither,
    /// `Refused(RootNotHeld)` because the root is not held. Nothing is ever
    /// signed: the root check comes before any signature. ASSUMPTION (a verb
    /// of its own would be clearer; the signer's owner may add one): the
    /// order of `decide` stays as it is, which signer/tests pin.
    fn ask_record(&mut self) {
        let now = self.now;
        self.server.page.send_ext(Ext::AskRecord, now);
    }

    fn next_stream(&mut self) -> u32 {
        if self.stream_base == 0 {
            self.stream = self.stream.wrapping_add(1).max(1);
            return self.stream;
        }
        // A READER's ids stay in its own range: `base` in the top byte, a
        // counter below it, so no two pages on one socket share a stream id.
        self.stream = (self.stream.wrapping_add(1) & STREAM_COUNTER).max(1);
        self.stream_base | self.stream
    }

    /// Every op the server emitted becomes a frame; every reply it made is
    /// queued for the client.
    fn pump(&mut self) {
        self.replies.extend(self.server.take_replies());
        let mut not_held = Vec::new();
        let mut not_sent = Vec::new();
        let mut no_head = false;
        for op in self.server.take_ops() {
            // No tree yet: there is no head to read, and asking the node for
            // one would name a Register nobody has made. Answered here.
            if matches!(op, Op::ReadHead { label: Label::Head }) && self.no_tree_yet() {
                no_head = true;
                continue;
            }
            if self.read_only() {
                match op {
                    Op::AskHeld { id } => {
                        not_held.push(id);
                        continue;
                    }
                    // A view's page makes no commit op; one that arrives is
                    // REFUSED BY NAME and handed back as the op's answer --
                    // loud, and ended: nothing waits on a frame never sent.
                    Op::Update { .. } | Op::Sign { .. } | Op::PutApp { .. } | Op::Ext(_) => {
                        let why = format!("read-only: a {} is never sent from a view", op_name(&op));
                        self.unusable.push(why.clone());
                        not_sent.push((op, why));
                        continue;
                    }
                    // A repair PUT (the only PUT a view's page makes), reads.
                    Op::Put { .. } | Op::Get { .. } | Op::ReadHead { .. } => {}
                }
            }
            let stream = self.next_stream();
            let framed = match op {
                Op::Put { id, bytes } => match wire::block::block_state(&id, &bytes) {
                    Some(state) => {
                        let c = wire::block::block_contract(&self.art.block_code, &id);
                        self.by_key.insert(c.key().to_string(), id);
                        self.by_contract.insert(wire::block::contract_for(&self.art.block_code, &id), id);
                        wire::frame_put(c, WrappedState::new(state), stream)
                    }
                    None => Err("a block whose bytes hash under no kind".into()),
                },
                Op::Get { id } => {
                    let contract = wire::block::contract_for(&self.art.block_code, &id);
                    self.by_contract.insert(contract, id);
                    wire::frame_get(wire::contract_id(contract), false, stream)
                }
                Op::ReadHead { label: Label::Head } => {
                    self.head_asked = true;
                    wire::frame_get(wire::contract_id(self.register_id), true, stream)
                }
                Op::ReadHead { label: Label::Site(app) } => match self.sites.get(&app) {
                    Some(site) => wire::frame_get(wire::contract_id(site.id), true, stream),
                    None => Err(format!("a read of {app}'s site, which is not being published")),
                },
                // A site's write is a PUT of its framing around exactly the signer's record (invariant 2).
                Op::Update { label: Label::Site(app), state } => match self.sites.get(&app) {
                    Some(site) => wire::frame_put(site.contract.clone(), WrappedState::new(contract_keys::site::frame(&state, &site.web)), stream),
                    None => Err(format!("a PUT of {app}'s site, which is not being published")),
                },
                Op::Update { label: Label::Head, state } => {
                    if self.register_seen {
                        wire::frame_update(self.register.key(), state, stream)
                    } else {
                        // The first head: the Register does not exist yet, and
                        // a PUT is what creates it (no delegate Install on this
                        // path). An existing one merges a PUT like an UPDATE.
                        wire::frame_put(self.register.clone(), WrappedState::new(state), stream)
                    }
                }
                Op::Sign { id, prev_seq, prev_root, seq, root, ledger, label } => {
                    let label = match label {
                        Label::Head => Ok(signer_proto::Label::Head),
                        Label::Site(app) => match self.sites.get(&app) {
                            Some(site) => Ok(signer_proto::Label::Site { app, contract: site.id }),
                            None => Err(format!("a sign for {app}'s site, which is not being published")),
                        },
                    };
                    label.and_then(|label| {
                        wire::signer::frame_sign(
                            &self.art.signer,
                            id,
                            signer_proto::Head { seq: prev_seq, root: prev_root },
                            signer_proto::Next { seq, root, ledger },
                            label,
                            stream,
                        )
                    })
                }
                // Page-io's own requests, sent and re-sent by the page's
                // sender (rule 5): framed HERE and nowhere else.
                Op::Ext(Ext::RegisterSigner) => match self.signer_container.clone() {
                    Some(c) => wire::frame_register_delegate(c, stream),
                    None => Err("the signer's registration, with no signer to register".into()),
                },
                Op::Ext(Ext::SignerFirst) => match self.first.as_ref() {
                    Some(First::Query) => wire::signer::frame_register_query(&self.art.signer, REGISTER_QUERY_ID, stream),
                    Some(First::Provision(key)) => wire::signer::frame_provision(
                        &self.art.signer,
                        PROVISION_ID,
                        key.clone(),
                        self.art.register_code.clone(),
                        self.art.register_params.clone(),
                        self.art.block_code.clone(),
                        stream,
                    ),
                    None => Ok(Vec::new()),
                },
                Op::Ext(Ext::AskRecord) => wire::signer::frame_sign(
                    &self.art.signer,
                    RECORD_QUERY_ID,
                    signer_proto::Head { seq: 0, root: self.server.page.published().1 },
                    signer_proto::Next { seq: 1, root: UNHELD_ROOT, ledger: Vec::new() },
                    signer_proto::Label::Head,
                    stream,
                ),
                Op::PutApp { key } => match self.app_contracts.get(&key) {
                    Some((c, st)) => wire::frame_put(c.clone(), st.clone(), stream),
                    None => Err(format!("an app PUT of {key}, whose contract this page does not hold")),
                },
                Op::AskHeld { id } => {
                    let hid = self.next_held_id;
                    self.next_held_id = self.next_held_id.wrapping_add(1).max(1 << 31);
                    self.held.insert(hid, vec![id]);
                    wire::signer::frame_held(&self.art.signer, hid, vec![wire::block::contract_for(&self.art.block_code, &id)], stream)
                }
            };
            match framed {
                Ok(f) => self.out.extend(f),
                Err(e) => self.unusable.push(format!("could not frame an op: {e}")),
            }
        }
        if !not_held.is_empty() || !not_sent.is_empty() || no_head {
            let now = self.now;
            for id in not_held {
                self.server.node(Answer::Held { id, present: false }, now);
            }
            for (op, why) in not_sent {
                // An app's PUT has its refusal already (`AppPutRefused`); the
                // commit ops a view never makes have none, so `NotSent`.
                let answer = match op {
                    Op::PutApp { key } => Answer::AppPutRefused { key, said: why },
                    op => Answer::NotSent { op, why },
                };
                self.server.node(answer, now);
            }
            if no_head {
                self.server.node(Answer::Head { label: Label::Head, read: None }, now);
            }
            self.pump();
        }
        // A site's bytes are page-io's only while its publication is in flight.
        let page = &self.server.page;
        self.sites.retain(|app, _| matches!(page.publication(app), Some(Publication::Publishing { .. })));
    }
}

fn op_name(op: &Op) -> &'static str {
    match op {
        Op::Put { .. } => "block PUT",
        Op::Update { .. } => "head update",
        Op::Sign { .. } => "sign request",
        Op::Get { .. } => "block GET",
        Op::ReadHead { .. } => "head read",
        Op::AskHeld { .. } => "held query",
        Op::PutApp { .. } => "app PUT",
        Op::Ext(_) => "signer request",
    }
}

use core_types::hex::encode as hex;
