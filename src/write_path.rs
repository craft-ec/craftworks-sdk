//! THE CLIENT'S WRITE PATH, rebuilt from the table — STRUCTURE ONLY
//! (WRITE-PATH.md revision 3, "A write's life at the client" and "Rulings made
//! while mapping the table to code"; build step 3).
//!
//! Every function here is one EVENT of the table, and its doc comment is that
//! event's column, cell by cell, stage by stage — so the mapping from the
//! design to the code can be reviewed before any behaviour exists. Bodies are
//! `todo!()`, and the module is behind the `write-path-v5` feature, OFF: it is
//! compiled and linted only when asked for (`cargo check --features
//! write-path-v5`), and nothing calls it.
//!
//! TWO DOORS. A write reaches the wire through ONE function, [`WritePath::leave`];
//! a reply reaches the write path through ONE function, [`WritePath::on_reply`].
//! Everything else is private to one of them.
//!
//! What this REPLACES when it lands: `PendingWrite::{queued, held, at_node}`,
//! `CachedStore::{held, drain_queued}`, `Copy::{hold, sent, answered,
//! submitted, queued}`, and — for v5 — sdk#174's `unheard` map (folded into
//! `Stage::AtNode`).

#![allow(dead_code, unused_variables)]

use protocol::{Ack, Op, Reply, WriteState};

/// The window's byte bound (W5), with `WRITES_IN_FLIGHT` = 16 its count bound:
/// an EIGHTH of the node's 64 MiB park budget (F15). PROVISIONAL — re-measured
/// in build step 5. Always at least one write, whatever its size.
pub const BYTES_IN_FLIGHT: usize = 8 * 1024 * 1024;

/// Silence this long (no reply to this session while it has writes `AtNode`
/// or `Taken`) → the ONE recovery. Recovery is harmless (every re-send is
/// safe against rule 1), so this clock is short.
pub const T_RECOVER_MS: u64 = 10_000;

/// A recovery attempt that stays silent this long has failed; `Unknown` only
/// after TWO. Giving up is not harmless, so this clock is long: the node's
/// `PARK_TTL` of 90 s plus margin (F50).
pub const T_GIVE_UP_MS: u64 = 100_000;

/// A gap longer than this between two of this client's own 1 s ticks is a
/// CLOCK JUMP (the machine slept) — judged on the client's clock alone.
pub const CLOCK_JUMP_MS: u64 = 5_000;

/// Where one write is, at the client. The ONLY per-write state.
///
/// * `Held` — made, not at the node: never sent, or refused with nothing
///   applied (`Busy`, `OutOfOrder`). Leaves through [`WritePath::leave`].
/// * `AtNode { since, asked_at }` — sent, no verdict. The window (W5) is these
///   writes (and their bytes), DERIVED — never a second record. `asked_at` is
///   when it was last asked after (`AskWrite`), `None` if never: sdk#174's
///   `unheard` map, folded into the stage.
/// * `Taken` — the engine has it: `Accepted`, or `id ≤ taken_through`. **No
///   age timeout** (a cold commit legitimately outlasts any T: F50's 75 s) and
///   NOT asked after — a `Taken` write learns from the Ack on every tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Held,
    AtNode { since: u64, asked_at: Option<u64> },
    Taken,
}

/// One write the client holds un-ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unended {
    pub write_id: u64,
    pub stage: Stage,
    /// ALL its ops — it leaves whole or not at all (W1).
    pub ops: Vec<Op>,
}

/// How a write ended, as the person is told — once (W4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Published,
    /// Fell: `Failed`, `Lost`, `TooLarge`, or behind one of those on a key
    /// (`Copy::fall`, whole).
    Fell(WriteState),
    /// Recovery gave up TWICE. The ONLY place `Unknown` remains.
    Unknown,
}

/// What made this session silent — every kind is ONE event, and the answer is
/// the same recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Silence {
    /// `T_RECOVER_MS` with no reply to this session while it has writes
    /// `AtNode`/`Taken`.
    NoReply,
    /// The socket dropped and a new one opened.
    Reconnect,
    /// The page came back from the back/forward cache (`pageshow`,
    /// `persisted`).
    PageShow,
    /// A gap > `CLOCK_JUMP_MS` between two of this client's own ticks.
    ClockJump,
}

/// One session's write path.
#[derive(Debug, Default)]
pub struct WritePath {
    pub unended: Vec<Unended>,
    /// When THIS SESSION was last answered anything (any reply carrying its
    /// Ack). Silence is measured from here — never per write.
    pub last_heard_ms: u64,
    /// When this client last ticked, for [`Silence::ClockJump`].
    pub last_tick_ms: u64,
    /// Recovery attempts that met silence in a row (two → `Unknown`).
    pub silent_recoveries: u8,
    /// Verdicts in a cell the table calls impossible (footnote ¹) — and any
    /// per-write `ParityComplete`, which a v5 session is never sent. Counted,
    /// otherwise ignored; the model test asserts 0 at the end of every run.
    pub impossible_verdicts: u64,
    /// Verdicts for a write this session no longer holds ("(not pending)").
    pub not_pending_verdicts: u64,
    /// Bodies ignored because they would move a write BACKWARDS from what
    /// the same reply's Ack had just established.
    pub behind_their_ack: u64,
}

impl WritePath {
    // ================================================= door 1: a write leaves

    /// THE ONE FUNCTION THROUGH WHICH A WRITE REACHES THE WIRE.
    ///
    /// Sets `AtNode { since: now, asked_at: None }` and sends the write WHOLE,
    /// as `WriteFrom { write_id, floor, ops }` with `floor` = [`floor`](Self::floor),
    /// computed AFTER `Copy::fall` has taken any later same-key writes with a
    /// fallen one. sdk#179 a (a slot never freed) and b (a re-send keeping the
    /// old clock) cannot be written against this shape: there is no second
    /// place a write leaves from, and no slot record beside the stage.
    pub fn leave(&mut self, write_id: u64, now_ms: u64) {
        todo!("step 3")
    }

    /// The lowest write id this session still holds un-ended — the `floor` on
    /// every `WriteFrom`. The engine's rule-3 floor arm STAYS once ids are no
    /// longer gapped at make (sdk#186): a client-side FALL still releases
    /// numbers, and an evicted session returns through it (ruling Q8).
    pub fn floor(&self) -> u64 {
        todo!("step 3")
    }

    /// **window has room** — `Held` → `AtNode{now}` through [`leave`](Self::leave),
    /// WHOLE, OLDEST FIRST, and only while no LOWER write id is `Held`.
    /// `AtNode`, `Taken`: —. The window: at most `WRITES_IN_FLIGHT` writes and
    /// [`BYTES_IN_FLIGHT`] bytes `AtNode`; always at least one (ruling Q1).
    pub fn on_room(&mut self, now_ms: u64) {
        todo!("step 3")
    }

    // ================================================= door 2: a reply arrives

    /// THE ONE FUNCTION THROUGH WHICH A REPLY REACHES THE WRITE PATH (ruling
    /// Q4's reading order). A v5 reply is `Acked { ack, body }`:
    /// 1. an Ack for ANOTHER session is dropped, as a foreign
    ///    `SessionWriteState` is;
    /// 2. the Ack is applied FIRST ([`apply_ack`](Self::apply_ack));
    /// 3. then the body's verdict, through the per-verdict cells below — and a
    ///    body that would move a write BACKWARDS from what its own Ack just
    ///    established (a `Busy` for a write the Ack says taken) is IGNORED and
    ///    counted in `behind_their_ack`.
    ///
    /// Every reply to this session also resets its silence clock.
    pub fn on_reply(&mut self, ack: &Ack, body: &Reply, now_ms: u64) {
        todo!("step 3")
    }

    /// **Any reply carrying an `Ack`** (this session's):
    /// * every write with `id ≤ published_through` is gone, base moves;
    /// * every `AtNode` with `id ≤ taken_through` → `Taken`;
    /// * **`taken_through` can go BACKWARDS** (the engine forgot): a `Taken`
    ///   write above BOTH numbers → `Held`, or "no age timeout" waits for ever.
    ///
    /// **Durable** (ruling Q5): the FIRST Ack with `published_through ≥ id`
    /// AND `parity == Some { owed_groups: 0, .. }`. `owed_groups` counts
    /// UNCODED groups, so 0 never means "not coded"; `None` is never durable;
    /// `at_seq` is diagnostic only — no publishing seq is needed.
    fn apply_ack(&mut self, ack: &Ack, now_ms: u64) {
        todo!("step 3")
    }

    /// **`Accepted`** —
    /// * `Held`: impossible¹ (counted).
    /// * `AtNode`: → `Taken`.
    /// * `Taken`: stay.
    /// * not pending: counted.
    fn on_accepted(&mut self, write_id: u64) {
        todo!("step 3")
    }

    /// **`Published`** — in EVERY stage: gone; base moves (⁴ for `Held`: a
    /// write re-held after the engine forgot can still be published).
    /// * not pending: counted.
    ///
    /// A per-write **`ParityComplete`** is NEVER sent to a v5 session — parity
    /// is the Ack's stat — so one arriving is counted in
    /// `impossible_verdicts` (ruling Q2).
    fn on_published(&mut self, write_id: u64) {
        todo!("step 3")
    }

    /// **`Busy`** —
    /// * `Held`: impossible¹.
    /// * `AtNode`: → `Held`². Re-offered when any other reply arrives, or on
    ///   `tick` — NOT at once (today's `drain_queued` rule).
    /// * `Taken`: impossible¹ — and a `Busy` for a write its own Ack says
    ///   taken never reaches here: [`on_reply`](Self::on_reply) ignores it.
    /// * not pending: counted.
    fn on_busy(&mut self, write_id: u64) {
        todo!("step 3")
    }

    /// **`OutOfOrder { expected }`** —
    /// * `Held`: impossible¹.
    /// * `AtNode`: → `Held`, and the session's OWN lowest un-ended write leaves
    ///   AT ONCE with a fresh floor (go-back-N: a request refused with no
    ///   verdict — F50's 9th, F51's 102nd — costs a round trip, not a 60 s
    ///   false rollback). `expected` is ADVICE: a write this session no longer
    ///   holds is NEVER resurrected (ruling Q3).
    /// * `Taken`: impossible¹.
    /// * not pending: counted.
    fn on_out_of_order(&mut self, write_id: u64, expected: u64) {
        todo!("step 3")
    }

    /// **`Failed` / `Lost` / `TooLarge`** —
    /// * `Held`: impossible¹.
    /// * `AtNode`, `Taken`: falls³ — `Copy::fall`, the write and every later
    ///   write on any of its keys, WHOLE (W1). The number is RELEASED (only a
    ///   publish consumes one), so the next `floor` passes it.
    /// * not pending: counted.
    fn on_fell(&mut self, write_id: u64, why: WriteState) {
        todo!("step 3")
    }

    /// **`Duplicate`** — in every stage: read its Ack⁴ (already applied by
    /// [`on_reply`](Self::on_reply)). A Duplicate is never "no information":
    /// * rule 1 (`id < next`): the Ack says PUBLISHED → gone;
    /// * rule 1b (the write IS the session's commit in flight — ruling Q4):
    ///   the Ack says TAKEN (`taken_through`) → `Taken`.
    /// * not pending: counted.
    fn on_duplicate(&mut self, write_id: u64) {
        todo!("step 3")
    }

    /// **`Stalled`** —
    /// * `Held`: impossible¹.
    /// * `AtNode`: stay.
    /// * `Taken`: stay; the row says so.
    /// * not pending: counted.
    fn on_stalled(&mut self, write_id: u64) {
        todo!("step 3")
    }

    // ================================================= the session's clocks

    /// **Silence of any kind** — no reply for [`T_RECOVER_MS`], a reconnect,
    /// `pageshow(persisted)`, a clock jump (> [`CLOCK_JUMP_MS`] between two of
    /// this client's own ticks, judged on the client clock alone) — is ONE
    /// event, never a rollback: → [`recover`](Self::recover) (ruling Q6).
    pub fn on_silence(&mut self, kind: Silence, now_ms: u64) {
        todo!("step 3")
    }

    /// THE ONE RECOVERY: ask (`tick`), read the Ack, settle by it; then
    /// re-send everything still un-ended from the LOWEST id, in order, whole
    /// — safe against a fact (rule 1). Nothing is rolled back as `Unknown` by
    /// a timer alone.
    fn recover(&mut self, now_ms: u64) {
        todo!("step 3")
    }

    /// **A recovery attempt silent for [`T_GIVE_UP_MS`]** — counted; after
    /// TWO in a row the un-ended writes fall as `Unknown`, TOLD. The only
    /// place `Unknown` remains.
    pub fn on_recovery_silent(&mut self, now_ms: u64) -> Vec<(u64, Ended)> {
        todo!("step 3")
    }

    /// **A write `AtNode` with no verdict for 1 s** (it may be PARKED: cold
    /// blocks being fetched; the client cannot tell) — `AskWrite(write_id)`,
    /// about once a second, at most ONE unanswered per session, for `AtNode`
    /// writes ONLY (a `Taken` write learns from the Ack on every tick). Asked
    /// time lives in `AtNode { asked_at }`; the one-at-a-time rule is
    /// `tick_gate::OneAtATime` (ruling Q7).
    pub fn ask_unheard(&mut self, now_ms: u64) -> Option<u64> {
        todo!("step 3")
    }

    // ================================================= not cells

    /// **A host error naming no request** — for the WRITE PATH it has no
    /// addressee and is nothing: silence or `OutOfOrder{expected}` covers what
    /// it meant. It still answers one FRAME for the SEND GATE (sdk#196, 1b):
    /// the frame it refused is no longer outstanding there.
    pub fn on_host_error(&mut self) {}

    /// **Make-time refusal** (too large, the copy's cap) — the id is NOT
    /// minted (sdk#186), and the refusal is the write's return value
    /// (sdk#180). Lives where ids are minted (`CachedStore::submit`), not
    /// here; listed so the table is complete.
    pub fn make_time_refusal(&mut self) {}
}
