//! The retry clock of EVERY node call on the page path (main's B2 ruling): an
//! RFC 6298 retransmission timeout over completed calls, and — for block GETs
//! — a congestion window. Nothing here is fixed; both move with what the node
//! does (the owner: per call, adaptive, moving with the network).
//!
//! The same rules as the cold reader (`craftworks_sdk::cold`, sdk#212), kept
//! here because the page crate does not depend on the SDK crate:
//! * `RTO = SRTT + 4·RTTVAR`, a [`RTO_FLOOR_MS`] floor, [`RTO_INITIAL_MS`]
//!   before any sample, [`RTO_MAX_MS`] ceiling;
//! * KARN'S RULE — a call that was re-sent is never a sample: which copy was
//!   answered is unknowable;
//! * §5.5 BACK-OFF — a timeout doubles the RTO (once per tick, however many
//!   timed out in it), until a call answers on its first send;
//! * the WINDOW — start [`WINDOW_INITIAL`], +1 per answer below the slow-start
//!   threshold, +1 per window above it, halved (≥ 1) on a timeout, the
//!   threshold set there.

/// Before any sample (RFC 6298 §2.1).
pub const RTO_INITIAL_MS: f64 = 1_000.0;
/// A loopback node answers in a few ms; a timeout that close re-sends on
/// scheduling jitter alone.
pub const RTO_FLOOR_MS: f64 = 100.0;
/// RFC 6298 §2.5 allows a ceiling of at least 60 s.
pub const RTO_MAX_MS: f64 = 60_000.0;
/// Block GETs in flight to start with.
pub const WINDOW_INITIAL: f64 = 4.0;

#[derive(Debug, Clone)]
pub struct Rto {
    srtt: Option<f64>,
    rttvar: f64,
    backoff: u32,
}

impl Default for Rto {
    fn default() -> Rto {
        Rto { srtt: None, rttvar: 0.0, backoff: 0 }
    }
}

impl Rto {
    /// The timeout now, in ms.
    pub fn rto_ms(&self) -> u64 {
        let base = match self.srtt {
            None => RTO_INITIAL_MS,
            Some(s) => (s + 4.0 * self.rttvar).max(RTO_FLOOR_MS),
        };
        (base * f64::from(1u32 << self.backoff.min(10))).min(RTO_MAX_MS) as u64
    }

    /// A call answered on its FIRST send took `r` ms: a sample, and the
    /// back-off ends (RFC 6298 §2.2–2.3, §5.7).
    pub fn sample(&mut self, r: u64) {
        let r = r as f64;
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2.0;
            }
            Some(s) => {
                self.rttvar = 0.75 * self.rttvar + 0.25 * (s - r).abs();
                self.srtt = Some(0.875 * s + 0.125 * r);
            }
        }
        self.backoff = 0;
    }

    /// Something timed out this tick: back the timer off (§5.5).
    pub fn timed_out(&mut self) {
        self.backoff = (self.backoff + 1).min(10);
    }

    pub fn srtt_ms(&self) -> Option<f64> {
        self.srtt
    }
}

#[derive(Debug, Clone)]
pub struct Window {
    w: f64,
    ssthresh: Option<f64>,
}

impl Default for Window {
    fn default() -> Window {
        Window { w: WINDOW_INITIAL, ssthresh: None }
    }
}

impl Window {
    /// How many GETs may be in flight now.
    pub fn size(&self) -> usize {
        self.w.max(1.0).floor() as usize
    }

    /// A GET was answered.
    pub fn opened(&mut self) {
        let w = self.w.max(1.0);
        self.w = if self.ssthresh.is_some_and(|t| w >= t) { w + 1.0 / w } else { w + 1.0 };
    }

    /// A GET timed out this tick: halve, and remember where.
    pub fn halved(&mut self) {
        let half = (self.w.max(1.0) / 2.0).max(1.0);
        self.w = half;
        self.ssthresh = Some(half);
    }
}
