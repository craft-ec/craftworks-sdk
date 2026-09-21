# A commit's life: inventory (engine + shell)

This is an inventory with no design in it. It lists every state a commit,
a parked write or an owed group can be in, every event that reaches them,
and what the code does for each pair. Every cell cites code at `af9f2c5`.
A pair with no code for it is written **NOT HANDLED — falls through to
&lt;what&gt;**.

Citation prefixes:

| prefix | file |
|---|---|
| `L` | `engine/src/lib.rs` |
| `A` | `engine/src/asks.rs` |
| `S` | `engine-delegate/src/shell.rs` |
| `E` | `engine-delegate/src/entry.rs` |
| `Q` | `engine-delegate/src/schedule.rs` |
| `V` | `engine-delegate/src/serve.rs` |

Every engine event ends in `keep_saveable` (L1179–1197, L1230). It is
listed as its own column because it acts on the states after each event.

---

## 1. States

### Engine (in the context unless marked *per call*)

| id | state | where it lives | carried across calls? |
|---|---|---|---|
| **I** | Idle: no commit, no parked write | `pending: None`, `parked_write: None` (L910, L948) | yes |
| **C1** | Commit in flight, data not all read back (`head_sent = false`) | `pending: Some(Commit)` (L800, L910) | yes, while `context_carries_pending` (default true, L682–730; L3754). **Not** its pack bodies: `#[serde(skip)] packs` (L813) |
| **C2** | Commit in flight, head bump emitted (`head_sent = true`) | `Commit::head_sent` (set at L2663, L2711) | yes |
| **C3** | Commit in flight, its writes told `Stalled` | `told_stalled` (L958), `in_flight_since` (L956) | yes (L3564) |
| **C4** | Commit in settle rounds (silent for `reask_after·2^rounds`) | `Commit::settle_at`, `settle_rounds` (L800) | yes |
| **P** | Parked write: its apply stopped on a block that is not held | `parked_write: Option<ParkedWrite>` (L777, L948); at most one | yes, ops and all (L3549, L3875) |
| **O0** | Owed group, not asked yet | `owed` entry (L735, L919), no `asks` entry | ids + ages only, **no bytes** (L3758; rebuilt empty L3890) |
| **O1** | Owed group, parity put asked for (paced) | `asks` (A67–A100, L925) | yes |
| **O2** | Owed group, some of its 3 parity blocks confirmed | `parity_confirmed` (L923) | yes |
| **O3** | Owed group coded by the commit still in flight | `owed` ∩ `pending.groups` | yes; filtered out of every emit (L3116) |
| **O4** | Owed group rehydrated with no bytes | `Owed::blocks` empty (L3890) | – |
| **W** | A write waiting for ParityComplete | `parity_waiting` (L941), capped by `max_parity_waiting_refs` | yes |
| **x** | Transient: `folded`, `unpublished`, `coded_since_commit` (L914, L974, L982) | handed off within one event; empty at every point an event can observe (L1868, L2018–2021, L2830) | **no** (not in `Context`, L3518) |
| **r** | `recovered` flag (L964) | set at L1388, L1414, L1440, L3879 | **read nowhere** in `engine/` or `engine-delegate/` |

### Shell (engine-delegate)

| id | state | where it lives | carried? |
|---|---|---|---|
| **S-aw** | A put acked, sync read missed, waiting for a GET read-back (rounds < 12) | `Shell::awaiting` (S317, `MAX_READ_BACK_ROUNDS` S176) | yes, only if the engine resumed (S451) |
| **S-hd** | A head issued and not yet read back | `Shell::head` (S318), set when the `Op::Head` leaves (S712) | yes, only if the engine resumed (S452) |
| **S-he** | "Register exists" for this call | `head_exists` (S319, S1326) | derived per call |
| **Q-held / Q-ready** | Scheduler ops waiting on a dependency, or for room (`max_gets 4`, `max_puts 128`) | `Scheduler` (Q76–96) | **no**: built fresh each call (S516); leftovers are counted as `stranded` (S746) and dropped |

## 2. Events

| event | engine entry | who produces it |
|---|---|---|
| `Write` (on_write) | L1837 | client `P::Write` (S840). `P::WriteFrom` (v5) never reaches it: `serve` answers v5 `Unsupported` (V42–56), with a `debug_assert` backstop (S814) |
| `AskWrite` (on_ask) | L1490 | client `P::AskWrite` (S856). No client sends it (comment S852) |
| `Tick(now)` (on_tick) | L2876 | client `P::Tick` (S860) only; the node fires no wake-ups |
| `Flush` (on_flush) | L2865 | client `P::Flush` (S861), a frame. There is no disconnect hook in the shell |
| `ClientGone` | L1363 (subs only) | **not produced** anywhere in `engine-delegate/src` |
| `PutConfirmed(id)` (put ack + read-back) | L2671 | S1006 (sync read on an empty GET), S994 (GET read-back), S1067 (sync read on the ack) |
| `PutFailed(id)` | L2720 | S1057 (ack `ok=false`), S1033 (read-back rounds exhausted) |
| `BlockArrived` (block arrival) | L3264 | S996: every `GotState` with bytes, read-back or not |
| `BlockMissed` | L3338 | S1035: an empty GET for an id not in `awaiting` |
| `HeadConfirmed(seq)` | L2763 | S1109–1111: the read-back (seq, root) equals what the shell wrote |
| `HeadRead` (head read) | L1398 | S1120–1127: a head read with no head written (`S-hd` empty) |
| `HeadMissing` | L1426 | S1119, S1129; also E489/E511 when there is no Register to ask |
| `HeadConflict` | L1451 | S1113: a head read-back differing from the one written |
| `Start` | L1385 | client `P::Identity` (S818) |
| context load | `from_context_or_new` L3821 → `read_context` L3832 / `hydrate` L3866 | every call: E151 → `Shell::resume_with` S422 → S443 |
| `keep_saveable` | L1230 | after **every** event (L1194–1196) |
| (shell only) `HeadAcked{ok}` | – | E231, E244 → S1082 |
| (shell only) unmatched contract answer | – | S561–575 → `Dropped::Unexpected` |

---

## 3. Summary matrix

`H` = handled (see the section-4 tables), `N` = NOT HANDLED, `·` = the event
does not touch this state, `D` = handled, but differently from the name of
the state (read the cell).

| state ↓ / event → | Write | Ask | Tick | Flush | PutConf | PutFail | Arrived | Missed | HeadConf | HeadRead | HeadMiss | HeadConfl | Start | ctx load | keep_saveable |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| I | H | D | H | H | · | · | · | · | D | H | H | H | H | H/N | · |
| C1 | H | D | H | H | H | H/N | · | · | D | H | H | H | H | H/N | H |
| C2 | H | D | H | H | · | · | · | · | H | H | H | H | H | H | H |
| C3 | H | D | H | · | H | H | · | · | H | H | H | H | · | H | · |
| C4 | · | · | H | · | · | · | · | · | · | · | · | · | · | H | · |
| P | H | **N** | **N** | **N** | · | · | H | H | · | D | · | D | · | H | H |
| O0/O1/O2 | H | · | H | H | H | H | · | · | H | **N** | · | D | · | H | H |
| O3 | · | · | H | H | · | · | · | · | H | · | · | H | · | H | · |
| O4 | · | · | H | H | · | · | **N** | · | · | · | · | · | · | H | · |
| W | H | D | · | · | H | · | · | · | H | **N** | · | D | · | H | H |
| S-aw | · | · | · | · | H | H | H | H | · | · | · | · | · | H | · |
| S-hd | · | · | · | · | · | · | · | · | H | H | H | H | · | H | · |
| Q-held/ready | H | · | H | H | H | · | · | · | · | · | · | · | · | **N** | · |

---

## 4. Per-state tables

### I: Idle (no commit, no parked write)

| event | what the code does today | cite |
|---|---|---|
| Write, over `max_write_bytes` | `TooLarge{WriteBytes}`, nothing applied | L1869–1878 |
| Write, apply needs a block not held | parks it → **P**, emits `FetchBlock` per need | L1924–1925 → L2035, L2088–2106 |
| Write, apply refused (bad key etc.) | `Failed` | L1930–1938 |
| Write, emits > `max_commit_blocks` | `TooLarge{CommitBlocks}` | L1946–1960 |
| Write that changes nothing (no blocks, root == published, nothing unpublished) | `Accepted` + `Published`; `ParityComplete` if nothing is owed, else waits on **every** currently-owed group → **W** | L1976–1998 |
| Write, ordinary | apply, `record_owed` (→ **O0**, may supersede → transfer waiters, may leave parity uncoded at the cap), `Accepted`, `start_commit` → **C1** | L2004–2025, L2155, L2384 |
| Write before any `Start`/head read | **NOT GATED**: applied against whatever `root` the engine holds (the empty leaf for a fresh engine, L1011–1100). `recovered` (**r**) is never read. A foreign head later turns up as `HeadConflict` → `Lost` | L1837–1888, L964 |
| AskWrite | "known" only if the id is in `folded`, `pending.writes` or `parity_waiting`, matched on **write id only, not client**. Otherwise `Lost`, including for a write that already published and completed | L1491–1504 |
| Tick | clock handling (first clock / back-step ignored / reset > 600 → re-anchor); `age_out_accepted` and `settle_by_fact` return early with no commit; owed parity emitted | L2890–2945, L3595 |
| Flush | nothing to commit (`unpublished` empty); all owed parity emitted whatever its age | L2867–2873 |
| HeadConfirmed | ignored (no commit) | L2766–2767 |
| HeadRead | `adopt(seq, root)`: root, published_* and next_seq move; subscribers told; `recovered = true` | L1413–1421, L1516 |
| HeadMissing | tries the next epoch (`ReadHead`); when out of epochs, adopts the empty tree at seq 0 | L1432–1441 |
| HeadConflict | `pending` is None, so there are no dead groups and no Lost; `adopt(seq, root)` | L1456–1485 |
| Start | records key and epochs, `recovered = false`, `ReadHead{first epoch}` or HeadMissing | L1385–1396 |
| ctx load, readable | `hydrate`: every carried field restored | L3866–3911 |
| ctx load, unreadable (magic/version/checksum/decode) | fresh `Engine::new`: seq 0, empty root. The shell drops `awaiting` and `head` too (S451–452). Nothing is announced; it waits for a client `Identity` → `Start` | L3821–3825, L3832–3864 |

### C1: Commit in flight, data not all read back

| event | what the code does today | cite |
|---|---|---|
| Write (any id, including a re-send of one in this commit) | `Busy`, nothing applied. There is no "Duplicate" | L1856–1861 |
| AskWrite for one of this commit's writes | known → **returns no effect at all** (no reply to the ask) | L1491–1497 |
| Tick | `age_out_accepted`: after `max_accept_age` (64) since `in_flight_since`, `Stalled` once per write → **C3**. `settle_by_fact`: once `reask_after·2^rounds` has passed → **C4** | L2936–2937, L2974–3001, L2526 |
| Flush | commit left alone; other owed groups' parity emitted (this commit's groups are filtered, **O3**) | L2867–2873, L3116 |
| PutConfirmed(data id) | inserted in `confirmed`; on the last one (`head_before_packs` false) `head_sent = true` and `UpdateHead{seq, root, after: data}` → **C2** | L2689–2716 |
| PutConfirmed(id not in commit) | ignored ("duplicates and stragglers") | L2689–2697 |
| PutFailed(pack id) | re-sends the body from `packs`, **same call only** (`packs` is `serde(skip)`, L813). With `pack_on_write: false` no packs exist | L2745–2750 |
| PutFailed(block id) the node holds | `PutBlock` re-sent from `blocks.get` | L2753–2758 |
| PutFailed(block id) the node does **not** hold | **NOT HANDLED — falls through to an empty `Vec`** (L2760). Recovery waits for the next due `settle_by_fact` (re-put from the carried ops, or `release_lost`) | L2720–2761 |
| BlockArrived / BlockMissed | the commit is not consulted; the shell turns read-backs into `PutConfirmed` itself (S991–994) | L3264–3329, L3338 |
| HeadConfirmed(seq ≠ c.seq) | ignored | L2768–2770 |
| HeadRead (same seq and root) | treated as `on_head` (published) | L1404–1407 |
| HeadRead (older seq) | `head_not_landed`: since data is not all confirmed, `head_sent = false`, nothing emitted | L1407–1408, L2655–2663 |
| HeadRead (newer seq, or same seq with another root) | `on_head_conflict` (see below) | L1409–1410 |
| HeadMissing | `head_not_landed` | L1428–1430 |
| HeadConflict (older seq) | `head_not_landed` | L1453–1455 |
| HeadConflict (same or newer) | commit dropped; its groups and `coded_since_commit` forgotten and removed from every waiter; `adopt` the winner; **every write → `Lost`** | L1456–1486, L2643–2651 |
| Start | no guard for a commit in flight: `ReadHead` goes out, and the read comes back into the pending-aware `on_head_read` above | L1385–1396, L1404 |
| ctx load, readable | commit restored **without pack bodies**; the timer and `told_stalled` restored | L3873, L3877–3878 |
| ctx load, unreadable | the commit is gone with no `Lost` sent. Its writes hear nothing unless the client sends `AskWrite`, which no client does (S852) | L3821–3825 |
| keep_saveable | the commit is never shed | L1230–1280 (doc L1212–1226) |

### C2: Head bump emitted, waiting for its read-back

| event | what the code does today | cite |
|---|---|---|
| (shell) the head op leaves | `Shell::head = Some((seq, root))` recorded when the op is issued, not when it is emitted | S712–716 |
| (entry) the Register is not writable, or signing fails | the op is **dropped without a word** (E435, E442). `S-hd` is still set, so recovery goes through the Tick → settle path (next row) | E435–444 |
| (shell) HeadAcked ok | `ReadHead` offered | S1089–1091 |
| (shell) HeadAcked !ok | `head = None`; **the engine is told nothing** (`head_sent` stays true) | S1083–1088 |
| Tick, settle due, all data confirmed, `head_sent` | `ReadHead` emitted (the fact check) | L2553–2559 |
| HeadConfirmed(c.seq) | commit ends: published_* move, subscribers notified, `Published` per write; per write: uncoded → no ParityComplete, no still-owed groups → `ParityComplete`, else → **W** | L2763–2832 |
| HeadRead / HeadMissing / HeadConflict (older) | `head_not_landed`: all confirmed, so `head_sent = true` and `UpdateHead` re-emitted | L2655–2669 |
| HeadConflict (same or newer) | as in C1: `Lost` for all, adopt | L1456–1486 |
| Write | `Busy` | L1856 |
| keep_saveable | never shed | L1230 |

### C3: Writes told `Stalled`

| event | what the code does today | cite |
|---|---|---|
| Tick | no repeat (`told_stalled.insert` guard) | L2992 |
| HeadConfirmed | `Published` as usual. `told_stalled` is **not** cleared in `on_head` (only in `start_commit` L2392–2394 and `release_lost` L2632) | L2771–2785 |
| release_lost | removed from `told_stalled` and `parity_waiting`; `Lost` | L2618–2640 |
| HeadConflict | `Lost`. `told_stalled` entries remain until the next `start_commit` clears the same (client, id) pairs | L1456–1486 |

### C4: Settle from fact (on Tick, when due)

| sub-case | what the code does today | cite |
|---|---|---|
| not yet due (`now − settle_at < reask_after·2^min(rounds,6)`) | nothing | L2530–2536 |
| data block now **held** on the node (sync read) | `on_confirmed` for it, and the head may go out | L2537–2546 |
| all confirmed and `head_sent` | `ReadHead` | L2553–2559 |
| all confirmed and not `head_sent` | nothing (unreachable unless `head_before_packs`) | L2553–2560 |
| missing blocks, rounds ≤ 3, ops carried (≤ 16 KiB) and they re-derive the same root | re-emit the missing `PutBlock`s | L2561–2566, L2587–2597 |
| missing, the re-derive needs a cold block | `FetchBlock` (attempt 1); retried next round | L2600–2608 |
| missing, no ops carried / root differs / rounds > 3 | `release_lost`: every write `Lost`, root back to published, groups forgotten → **I** | L2567, L2618–2640 |

### P: Parked write

| event | what the code does today | cite |
|---|---|---|
| Write (any, including the same id re-sent) | `Busy` | L1856–1861 |
| AskWrite for the parked write | **NOT HANDLED — falls through to `Lost`**: `known` checks `folded`, `pending`, `parity_waiting`, not `parked_write`. The write is still parked and will apply | L1491–1504 |
| BlockArrived for a needed id, others still needed | removes it from `needs`; waits | L2116–2126 |
| BlockArrived, last need, `p.root == self.root` | `apply_write` re-run: may re-park (→ **P**, rounds+1), fail, `TooLarge`, or apply → `Accepted` + commit → **C1** | L2139, L1890 |
| BlockArrived, last need, root moved | `Failed` | L2128–2136 |
| BlockArrived inside a pack | members wake the parked write as well | L3285–3292, L3322–3325 |
| BlockMissed for a needed id | re-parks with `rounds+1`: `FetchBlock` for all of `needs` again; over `max_apply_rounds` (32) → `Failed` | L3347–3357, L2044–2050 |
| a re-park after the root moved | `park_write` records `root: self.root` **again** (L2102), so a head change followed by a miss or a partial-progress round is **not** caught by the stale check at L2128 | L2097–2106 |
| ops over `max_parked_write_bytes` | not parked; fetches still emitted; `Busy` | L2065–2087 |
| Tick | **NOT HANDLED — falls through to nothing**: `on_tick` never looks at `parked_write` (L2876–2946). If its `FetchBlock` was stranded (more than `max_gets` 4 in the call, dropped with the per-call scheduler, S516, Q176–196) or never answered, no `BlockArrived`/`BlockMissed` comes, the write stays parked, and **every later Write is `Busy`** (L1856) | L2876–2946 |
| Flush | **NOT HANDLED — falls through to nothing** (`on_flush` looks only at `pending` and `unpublished`) | L2865–2874 |
| HeadRead / HeadConflict (no commit) | `adopt` moves `root`; the parked write is not touched here. See "BlockArrived, root moved" and the re-park row | L1413, L1485 |
| ctx load | restored with its ops | L3875 |
| keep_saveable, context over the bound after every parked read is shed | `debug_assert!(false)`; in release, `Busy` and shed (`shed.writes`) | L1256–1272 |

### O: Owed parity group (O0 not asked · O1 asked · O2 part-confirmed · O3 own commit in flight · O4 no bytes)

| event | what the code does today | cite |
|---|---|---|
| Write (record_owed) | the new groups go in `owed` (newest wins, `last_changed = now`). An owed group no longer listed after the write, and **not touched** (neither asked nor confirmed), is superseded: forgotten, its waiters transferred to the new groups (or settled if none). Past `max_owed_groups` (128) the new groups are **not coded**: `shed.uncoded`, the commit is marked `uncoded` | L2155–2317, L2257, L2268–2274, L2295 |
| Tick (coalesce on) | due = not O3 and (unchanged this tick or older than `parity_age` 32) and some block unconfirmed and `asks.due` → `recompute_owed` if O4 → `PutParity{after: published_root}`; a new ask waits if `asks` is full (132) | L2944, L3105–3164, L3151 |
| Flush | `emit_parity(|_| true)`: every group whatever its age, still paced by `asks.due`, O3 still filtered | L2872, L3116 |
| O4 at emit | walks the tree from `root` (≤ 512 blocks) to recover the bytes; `FetchBlock` for what it cannot read; the group stays owed | L3016–3103 |
| O4 + BlockArrived | **NOT HANDLED — falls through to nothing**: an arrival wakes reads and the parked write only (L3310–3325). The bytes are recomputed only at the next due emit | L3264–3329 |
| PutConfirmed(parity id) | ask settled, id into `parity_confirmed`; all three in → group forgotten, `settle_group` → `ParityComplete` to waiters emptied | L2679–2687, L2322 |
| PutFailed(parity id) | `asks.failed` re-dates it (paced like silence, no immediate re-put) | L2724–2734, A93 |
| HeadConfirmed | a group of the published commit stops being O3 and is emitted at the next due tick; with coalesce off, all are emitted now | L2791–2796, L2824–2826 |
| HeadRead (no commit, another writer's root) | **NOT HANDLED — falls through to nothing**: `adopt` leaves `owed`, `asks`, `parity_confirmed` untouched. Later puts go out `after` the new root, for groups that describe the old tree | L1516–1522, L3159 |
| HeadConflict | only the dead commit's groups and `coded_since_commit` are forgotten; other owed groups stay | L1458–1461, L2643–2651 |
| release_lost | the dead commit's groups forgotten | L2626 |
| clock jump / first clock | `since` and `last_changed` re-dated; asks re-anchored | L2895–2934, A111–124 |
| ctx load | ids + ages restored, bytes empty (→ O4); `asks` and `parity_confirmed` restored | L3880–3894 |
| keep_saveable | **never** shed (bounded at `record_owed` instead) | L1212–1226 |
| (shell) the parity put is held | goes out `after: published_root`; the scheduler seeds `published_root` as confirmed only when `published_seq > 0` | S535–537 |

`Asks::retain` (A105) has no caller.

### W: A write waiting for ParityComplete

| event | what the code does today | cite |
|---|---|---|
| entered | at `HeadConfirmed` (L2816–2819), or on a no-op write while something is owed (L1986–1992) | |
| group settles | removed; `ParityComplete` when empty | L2322–2341 |
| group superseded | transferred to the replacements (`transfer_superseded_waiters`, default true); `ParityComplete` if none | L2355–2381 |
| superseded while at the owed cap | the waiter's ref is dropped; if that empties it, the waiter is **removed silently**, counted `shed.waits`, never told | L2280–2293 |
| HeadConflict / release_lost | dead groups removed from every waiter's set. A set this empties is **not** notified and stays in the map (only `settle_group`/`transfer_waiters` remove empty sets) | L2643–2651 |
| HeadRead adopting another root | **NOT HANDLED — falls through to nothing** (the waiter keeps waiting on groups of the old tree) | L1516–1522 |
| AskWrite | known → no reply | L1495–1497 |
| keep_saveable over `max_parity_waiting_refs` (1024) | the **oldest-keyed** waiter (lowest (client, id)) is removed, counted, never told | L1232–1240 |
| ctx load | restored | L3903–3905 |

### S-aw: The shell waiting for a read-back

| event | what the code does today | cite |
|---|---|---|
| PutAcked ok, sync read hits | `PutConfirmed` now; the scheduler is told | S1059–1067 |
| PutAcked ok, sync read misses | → `awaiting[id] = 0`, GET offered | S1069–1075 |
| PutAcked !ok | `PutFailed` | S1056–1057 |
| GotState with bytes | `PutConfirmed` then `BlockArrived` | S991–996 |
| GotState empty, sync read now hits | `PutConfirmed` | S1002–1007 |
| GotState empty, rounds + 1 < 12 | GET re-offered (attempt rounds+2). **If stranded by `max_gets`, NOT HANDLED — falls through to** the engine's Tick `settle_by_fact` sync read (L2537) | S1022–1028 |
| GotState empty, rounds exhausted | `PutFailed` | S1030–1034 |
| answer naming a contract that matches nothing waited on | `Dropped::Unexpected` | S561–575 |
| end of call | `awaiting` kept only for ids the engine still `waiting_on` | S745, L1129 |
| ctx load | kept if the engine resumed, else dropped | S451 |

### S-hd: The shell waiting for its head read-back

| event | what the code does today | cite |
|---|---|---|
| GotHead (seq, root) == written | `HeadConfirmed(seq)` | S1109–1111 |
| GotHead differs | `HeadConflict{seq, root}` (the engine may map an older seq to not-landed, L1453) | S1112–1114 |
| NoHead | `HeadMissing` | S1119 |
| HeadAcked !ok | cleared | S1087 |
| no Register configured | `ReadHead` → `no_head` → the shell re-entered with `NoHead` in the same call | E487–511 |

### Q: The per-call scheduler

| event | what the code does today | cite |
|---|---|---|
| any effect with `after` not confirmed | held | Q128–138 |
| confirm | released | Q156–170 |
| over `max_gets` (4) / `max_puts` (128) in one return | kept in `ready` "for the next entry", but the scheduler is **rebuilt empty every call** (S516), so this is **NOT HANDLED — falls through to** `stranded` in the call report (S746–749) and the op is gone; the engine re-emits only what a later event re-derives | Q176–196 |
| next call | seeded with `published_root` (if seq > 0) and the commit's `confirmed` | S535–549, S693–695 |
| no contract code | every put removed, counted `refused_no_code` | S702–706 |
| a `PutPack` reaches the entry | refused, counted | E418–421 |

---

## 5. The NOT HANDLED cells, collected

1. **P × Tick / Flush**: nothing re-drives a parked write whose fetch was stranded or never answered, and every Write is `Busy` while it stays (L1856, L2876–2946, L2865–2874).
2. **P × AskWrite**: a parked write is answered `Lost` (L1491–1504).
3. **P, a re-park after the root moved**: `root` is re-captured at L2102, defeating the stale check at L2128.
4. **C1 × PutFailed** for a block the node does not hold: empty (L2760); left to the settle path.
5. **C1/C2 × unreadable context**: the commit is dropped with no `Lost` sent (L3821–3825; the shell side S451–452).
6. **I × Write before `Start`**: `recovered` is never read (L964).
7. **C2 × HeadAcked !ok**: the engine is not told (S1083–1088); left to the settle path's `ReadHead`.
8. **C2, head op dropped at the entry** (not writable / sign failed): silent (E435, E442).
9. **O × HeadRead adopting another root**: owed groups and asks are untouched (L1516–1522).
10. **O4 × BlockArrived**: no recompute until the next due emit.
11. **W × HeadRead adopting another root**: waiters are untouched.
12. **W emptied by HeadConflict / release_lost of a dead group**: an empty set stays and nobody is told (L2643–2651).
13. **Q, over a per-return limit**: an op kept "for the next entry" is dropped with the call (S516, Q191).
14. **AskWrite, known**: returns no effect, so the asker gets no reply (L1496–1497); unknown includes completed writes, and matching ignores the client (L1491–1495).
15. **`ClientGone`** is never produced by the shell; **`Flush`** only arrives as a client frame (S861).
16. **C3 × HeadConfirmed**: `told_stalled` is not cleared at publish (L2771–2832).
