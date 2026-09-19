//! Record ids: 16 bytes — 8 ms timestamp ‖ 4 device ‖ 4 tail — big-endian, so
//! byte order is time order. Ids from one generator strictly increase, even
//! within one millisecond or if the clock steps back.

pub type RKey = [u8; 16];

/// Time and randomness, injected so tests are deterministic.
pub trait Env {
    fn now_ms(&mut self) -> u64;
    fn rand32(&mut self) -> u32;
}

pub struct SystemEnv;

impl Env for SystemEnv {
    #[cfg(not(target_arch = "wasm32"))]
    fn now_ms(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
    #[cfg(target_arch = "wasm32")]
    fn now_ms(&mut self) -> u64 {
        js_sys::Date::now() as u64
    }
    fn rand32(&mut self) -> u32 {
        let mut b = [0u8; 4];
        // A failed RNG only weakens uniqueness across devices, which the device
        // field already separates; never fatal.
        let _ = getrandom::getrandom(&mut b);
        u32::from_le_bytes(b)
    }
}

pub struct IdGen {
    device: [u8; 4],
    last_ms: u64,
    last_tail: u32,
}

impl IdGen {
    pub fn new(device: [u8; 4]) -> Self {
        IdGen {
            device,
            last_ms: 0,
            last_tail: 0,
        }
    }

    pub fn next(&mut self, env: &mut dyn Env) -> RKey {
        let now = env.now_ms();
        if now > self.last_ms {
            self.last_ms = now;
            // Leave headroom so same-millisecond ids can count up without wrapping.
            self.last_tail = env.rand32() >> 1;
        } else if self.last_tail == u32::MAX {
            self.last_ms += 1;
            self.last_tail = 0;
        } else {
            self.last_tail += 1;
        }
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&self.last_ms.to_be_bytes());
        id[8..12].copy_from_slice(&self.device);
        id[12..].copy_from_slice(&self.last_tail.to_be_bytes());
        id
    }
}

pub fn created_ms(id: &RKey) -> u64 {
    u64::from_be_bytes(id[..8].try_into().unwrap())
}

pub fn to_hex(id: &RKey) -> String {
    crate::hex(id)
}

pub fn from_hex(s: &str) -> Option<RKey> {
    if s.len() != 32 || !s.is_ascii() {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(id)
}
