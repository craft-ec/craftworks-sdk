//! THE CLIENT'S WRITE PATH, rebuilt from the table — STRUCTURE ONLY
//! (WRITE-PATH.md revision 3, "A write's life at the client"; build step 3).
//!
//! Every function here is one EVENT of the table, and its doc comment is that
//! event's column, cell by cell, stage by stage — so the mapping from the
//! design to the code can be reviewed before any behaviour exists. Bodies are
//! `todo!()`, and the module is behind the `write-path-v5` feature, OFF: it is
//! compiled and linted only when asked for (`cargo check --features
//! write-path-v5`), and nothing calls it.
//!
//! What this REPLACES when it lands: `PendingWrite::{queued, held, at_node}`,
//! `CachedStore::{held, drain_queued}`, `Copy::{hold, sent, answered,
//! submitted, queued}` — one `Stage` per write instead of three flags and a
//! queue, and ONE function through which a write reaches the wire.
//!
//! Questions the table leaves open are marked `OPEN(Qn)`; they are asked, not
//! chosen.

#![allow(dead_code, unused_variables)]

use protocol::{Ack, Op, WriteState};

/// Where one write is, at the client. The ONLY per-write state.
///
/// * `Held` — made, not at the node: never sent, or refused with nothing
///   applied (`Busy`, `OutOfOrder`). Leaves through [`WritePath::leave`].
/// * `AtNode { since }` — sent, no verdict. The window (W5) is these writes
///   (and their bytes), DERIVED — never a second record.
/// * `Taken` — the engine has it: `Accepted`, or `id ≤ taken_through`. **No
///   age timeout**: a cold commit legitimately outlasts any T (F50: 75 s), and
///   `Stalled` says so. The only clock is SILENCE, per session (below).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Held,
    AtNode { since: u64 },
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
    /// Recovery met silence twice. The ONLY place `Unknown` remains.
    Unknown,
}

/// One session's write path. Holds the un-ended writes, the session's clock
/// of SILENCE, and the counters the model test asserts on.
#[derive(Debug, Default)]
pub struct WritePath {
    pub unended: Vec<Unended>,
    /// When THIS SESSION was last answered anything (any reply carrying its
    /// Ack). Silence is measured from here — never per write.
    pub last_heard_ms: u64,
    /// Verdicts in a cell the table calls impossible (footnote ¹). Counted,
    /// otherwise ignored; the model test asserts 0 at the end of every run.
    pub impossible_verdicts: u64,
    /// Verdicts for a write this session no longer holds ("(not pending)" row).
    pub not_pending_verdicts: u64,
    /// Recoveries in a row that met silence (two → `Unknown`).
    pub silent_recoveries: u8,
}

impl WritePath {
    // ------------------------------------------------------------ leaving

    /// THE ONE FUNCTION THROUGH WHICH A WRITE REACHES THE WIRE.
    ///
    /// Sets `AtNode { since: now }` and sends the write WHOLE, as
    /// `WriteFrom { write_id, floor, ops }` with `floor` = the lowest id this
    /// session still holds un-ended — computed AFTER `Copy::fall` has taken
    /// any later same-key writes with a fallen one. sdk#179 a (a slot never
    /// freed) and b (a re-send keeping the old clock) cannot be written
    /// against this shape: there is no second place a write leaves from, and
    /// no slot record beside the stage.
    pub fn leave(&mut self, write_id: u64, now_ms: u64) {
        todo!("step 3")
    }

    /// The lowest write id this session still holds un-ended — the `floor`
    /// on every `WriteFrom` (rule 3's `max(next, floor)`).
    pub fn floor(&self) -> u64 {
        todo!("step 3")
    }

    // ------------------------------------------------ per-stage cells: events

    /// **window has room** — `Held` → `AtNode{now}`, sent WHOLE through
    /// [`leave`](Self::leave). Held writes leave OLDEST FIRST, and only while
    /// no LOWER write id is `Held`. `AtNode`, `Taken`: —.
    ///
    /// The window is the count AND the bytes of writes `AtNode` (W5:
    /// `WRITES_IN_FLIGHT` = 16, `BYTES_IN_FLIGHT`; always at least one).
    /// OPEN(Q1): `BYTES_IN_FLIGHT`'s value is not in the table.
    pub fn on_room(&mut self, now_ms: u64) {
        todo!("step 3")
    }

    /// **`Accepted`** —
    /// * `Held`: impossible¹ (counted).
    /// * `AtNode`: → `Taken`.
    /// * `Taken`: stay.
    /// * not pending: counted.
    pub fn on_accepted(&mut self, write_id: u64) {
        todo!("step 3")
    }

    /// **`Published` / `ParityComplete` / an Ack with `published_through ≥ id`**
    /// — in EVERY stage: gone; base moves (⁴ for `Held`: a write re-held after
    /// the engine forgot can still be published).
    /// * not pending: counted.
    ///
    /// OPEN(Q2): does a v5 engine still SEND a per-write `ParityComplete`
    /// after `Published`? If it does, every one arrives "not pending" and the
    /// counter is noise (the sdk#192 lesson); if parity is only the Ack's stat,
    /// the per-write verdict should be gone from v5.
    pub fn on_published(&mut self, write_id: u64) {
        todo!("step 3")
    }

    /// **`Busy`** —
    /// * `Held`: impossible¹.
    /// * `AtNode`: → `Held`². Re-offered when any other reply arrives, or on
    ///   `tick` — NOT at once (today's `drain_queued` rule).
    /// * `Taken`: impossible¹.
    /// * not pending: counted.
    pub fn on_busy(&mut self, write_id: u64) {
        todo!("step 3")
    }

    /// **`OutOfOrder { expected }`** —
    /// * `Held`: impossible¹.
    /// * `AtNode`: → `Held`; `expected` leaves AT ONCE (go-back-N: a request
    ///   refused with no verdict — F50's 9th, F51's 102nd — costs a round
    ///   trip, not a 60 s false rollback).
    /// * `Taken`: impossible¹.
    /// * not pending: counted.
    ///
    /// OPEN(Q3): `expected` names a write this session does NOT hold (already
    /// ended here — e.g. its `Failed` arrived after the engine's number moved).
    /// Nothing to send: fall back to `floor`, count it, or both?
    pub fn on_out_of_order(&mut self, write_id: u64, expected: u64) {
        todo!("step 3")
    }

    /// **`Failed` / `Lost` / `TooLarge`** —
    /// * `Held`: impossible¹.
    /// * `AtNode`, `Taken`: falls³ — `Copy::fall`, the write and every later
    ///   write on any of its keys, WHOLE (W1). The number is RELEASED (only a
    ///   publish consumes one), so the next `floor` passes it.
    /// * not pending: counted.
    pub fn on_fell(&mut self, write_id: u64, why: WriteState) {
        todo!("step 3")
    }

    /// **`Duplicate`** — in every stage: read its Ack⁴. A `Duplicate` is never
    /// "no information": its Ack says published (→ gone) or taken (→ `Taken`).
    /// * not pending: counted.
    ///
    /// OPEN(Q4): by the engine's rule 1 a Duplicate means `id < next =
    /// published_through + 1`, i.e. ALWAYS published — when does footnote ⁴'s
    /// "taken" arm happen?
    pub fn on_duplicate(&mut self, write_id: u64, ack: &Ack) {
        todo!("step 3")
    }

    /// **`Stalled`** —
    /// * `Held`: impossible¹.
    /// * `AtNode`: stay.
    /// * `Taken`: stay; the row says so.
    /// * not pending: counted.
    pub fn on_stalled(&mut self, write_id: u64) {
        todo!("step 3")
    }

    // ------------------------------------------------ session-level events

    /// **Any reply carrying an `Ack`** (this session's; a foreign Ack is
    /// dropped, as a foreign `SessionWriteState` is):
    /// * every write with `id ≤ published_through` is gone, base moves;
    /// * every `AtNode` with `id ≤ taken_through` → `Taken`;
    /// * **`taken_through` can go BACKWARDS** (the engine forgot): a `Taken`
    ///   write above BOTH numbers → `Held`, or "no age timeout" waits for ever.
    ///
    /// This is how a dropped or misrouted verdict heals. It also resets the
    /// session's silence clock (`last_heard_ms`) and `silent_recoveries`.
    pub fn on_ack(&mut self, ack: &Ack, now_ms: u64) {
        todo!("step 3")
    }

    /// **The Ack's parity stat** `Some(Parity { owed_groups, at_seq })` — a
    /// write is DURABLE when `owed_groups == 0` at a seq ≥ the one that
    /// published it. `None` = the engine does not know: never "durable".
    ///
    /// OPEN(Q5): the client must know the SEQ that published each write to
    /// compare — the Ack carries `published_through` (an id), not the seq.
    /// Where does "its publishing seq" come from?
    pub fn on_parity(&mut self, ack: &Ack) {
        todo!("step 3")
    }

    /// **SILENCE** (`T` with no reply to this session while it has writes
    /// `AtNode`/`Taken`), **RECONNECT**, **`pageshow(persisted)`**, **CLIENT
    /// CLOCK JUMP** (> T in one tick) — ONE recovery: ask (`tick`), read the
    /// Ack, settle by it; then re-send everything still un-ended from the
    /// LOWEST id, in order, whole — safe against a fact (rule 1). Nothing is
    /// rolled back as `Unknown` by a timer alone.
    ///
    /// OPEN(Q6): T's value (today's `pending_timeout_ms` is 60 s) — and does a
    /// clock jump count from the client's clock alone, or also the Ack's?
    pub fn recover(&mut self, now_ms: u64) {
        todo!("step 3")
    }

    /// **Recovery itself meets silence, twice** — the un-ended writes fall as
    /// `Unknown`, TOLD. The only place `Unknown` remains.
    pub fn on_recovery_silent(&mut self, now_ms: u64) -> Vec<(u64, Ended)> {
        todo!("step 3")
    }

    /// **A write `AtNode` with no verdict for 1 s** (it may be PARKED: cold
    /// blocks being fetched; the client cannot tell) — `AskWrite(write_id)`,
    /// about once a second, at most ONE unanswered per session. A PULL, so its
    /// answer comes in a call the writer started (not misroutable).
    ///
    /// OPEN(Q7): engineer2's sdk#174 already has `ask_unheard` with its own
    /// `unheard` map and `tick_gate::OneAtATime`. Does this REPLACE its map
    /// with the stage (AtNode + since), keeping `OneAtATime` for the
    /// one-at-a-time rule — and does it ask `Taken` writes too (the table says
    /// `AtNode`; #174 asks after `Accepted`)?
    pub fn ask_unheard(&mut self, now_ms: u64) -> Option<u64> {
        todo!("step 3")
    }

    /// **A host error naming no request** — NOT a cell: it has no addressee.
    /// It is nothing; silence or `OutOfOrder{expected}` covers what it meant.
    pub fn on_host_error(&mut self) {}

    /// **Make-time refusal** (too large, the copy's cap) — the id is NOT
    /// minted (today it is, leaving gaps: sdk#186), and the refusal is the
    /// write's return value (sdk#180). Lives where ids are minted
    /// (`CachedStore::submit`), not here; listed so the table is complete.
    ///
    /// OPEN(Q8): with ids no longer gapped, rule 3's `floor > next` arm still
    /// happens (a write FALLS client-side, releasing its number) — confirm the
    /// floor arm stays for that, not only for make-time gaps.
    pub fn make_time_refusal(&mut self) {}
}
