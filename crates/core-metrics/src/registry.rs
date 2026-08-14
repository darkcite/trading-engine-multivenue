//! Counter + gauge registry. Boot-only insertion; zero-alloc record.

use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// Maximum number of counters per registry. Bump and recompile.
pub const MAX_COUNTERS: usize = 64;
// SAFETY: the registry's internal arrays are sized at compile time
// from these constants. Bumping doesn't affect hot-path behavior;
// each counter/gauge is still 64-byte aligned.
/// Maximum number of gauges per registry.
/// Max registrable gauges. Bumped to 128 in Phase 7-prep so we
/// can carry per-bucket tick-age gauges (one per
/// `engine::SYM_BUCKETS`) alongside the ~18 engine-wide gauges
/// without running out of slots.
pub const MAX_GAUGES: usize = 128;
/// Maximum metric-name length (bytes). 63 ASCII + a NUL terminator
/// keeps `[u8; 64]` aligned on cache lines.
pub const NAME_MAX: usize = 63;

/// Opaque counter handle returned from [`MetricsRegistry::register_counter`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CounterId(u32);

/// Opaque gauge handle.
///
/// `Default` returns a sentinel `GaugeId(0)` — only useful as an
/// initial value when building a `[GaugeId; N]` array that the cli
/// will fill in via `register_gauge` calls before any reads.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct GaugeId(u32);

/// A registered counter. Monotonic; only ever increases.
#[repr(C, align(64))]
pub struct Counter {
    name: [u8; 64],
    name_len: u8,
    _pad: [u8; 7],
    value: AtomicU64,
}

impl Counter {
    const fn empty() -> Self {
        Self {
            name: [0u8; 64],
            name_len: 0,
            _pad: [0u8; 7],
            value: AtomicU64::new(0),
        }
    }

    /// Increment by `n`. Zero-alloc; relaxed ordering.
    #[inline]
    pub fn inc(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    /// Read the current value.
    #[inline]
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    /// Counter name (ASCII slice).
    #[inline]
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
}

/// A registered gauge. Can move up or down; representation is i64
/// so callers can encode signed values (latency deltas, P&L, ...).
#[repr(C, align(64))]
pub struct Gauge {
    name: [u8; 64],
    name_len: u8,
    _pad: [u8; 7],
    value: AtomicI64,
}

impl Gauge {
    const fn empty() -> Self {
        Self {
            name: [0u8; 64],
            name_len: 0,
            _pad: [0u8; 7],
            value: AtomicI64::new(0),
        }
    }

    /// Set the gauge to `v`. Zero-alloc; relaxed ordering.
    #[inline]
    pub fn set(&self, v: i64) {
        self.value.store(v, Ordering::Relaxed);
    }

    /// Read the current value.
    #[inline]
    pub fn get(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }

    /// Gauge name (ASCII slice).
    #[inline]
    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
}

/// Why a `register_*` call rejected.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RegErr {
    /// The respective table is at capacity.
    Full,
    /// Name exceeds [`NAME_MAX`] bytes or is empty.
    BadName,
    /// A metric with that name is already registered.
    Duplicate,
}

/// Why a Prometheus-encode rejected.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EncodeErr {
    /// The destination buffer is too small.
    BufferTooSmall,
}

/// Preallocated counter + gauge registry. Construction allocates
/// once; every subsequent operation is zero-alloc.
pub struct MetricsRegistry {
    counters: [Counter; MAX_COUNTERS],
    gauges: [Gauge; MAX_GAUGES],
    n_counters: u32,
    n_gauges: u32,
}

impl MetricsRegistry {
    /// Build an empty registry. ~8 KiB of zero-init data.
    pub fn new() -> Self {
        // We can't use `[Counter::empty(); 64]` because `Counter`
        // contains an `AtomicU64` which is `!Copy`. Fall back to
        // `from_fn` (boot-time only; doesn't allocate on the hot
        // path).
        Self {
            counters: std::array::from_fn(|_| Counter::empty()),
            gauges: std::array::from_fn(|_| Gauge::empty()),
            n_counters: 0,
            n_gauges: 0,
        }
    }

    /// Register a new counter. Returns a stable handle the caller
    /// uses on every increment.
    pub fn register_counter(&mut self, name: &str) -> Result<CounterId, RegErr> {
        if name.is_empty() || name.len() > NAME_MAX {
            return Err(RegErr::BadName);
        }
        let bytes = name.as_bytes();
        for i in 0..self.n_counters as usize {
            if &self.counters[i].name[..self.counters[i].name_len as usize] == bytes {
                return Err(RegErr::Duplicate);
            }
        }
        if (self.n_counters as usize) >= MAX_COUNTERS {
            return Err(RegErr::Full);
        }
        let idx = self.n_counters as usize;
        self.counters[idx].name[..bytes.len()].copy_from_slice(bytes);
        self.counters[idx].name_len = bytes.len() as u8;
        self.n_counters = self.n_counters.wrapping_add(1);
        Ok(CounterId(idx as u32))
    }

    /// Register a new gauge.
    pub fn register_gauge(&mut self, name: &str) -> Result<GaugeId, RegErr> {
        if name.is_empty() || name.len() > NAME_MAX {
            return Err(RegErr::BadName);
        }
        let bytes = name.as_bytes();
        for i in 0..self.n_gauges as usize {
            if &self.gauges[i].name[..self.gauges[i].name_len as usize] == bytes {
                return Err(RegErr::Duplicate);
            }
        }
        if (self.n_gauges as usize) >= MAX_GAUGES {
            return Err(RegErr::Full);
        }
        let idx = self.n_gauges as usize;
        self.gauges[idx].name[..bytes.len()].copy_from_slice(bytes);
        self.gauges[idx].name_len = bytes.len() as u8;
        self.n_gauges = self.n_gauges.wrapping_add(1);
        Ok(GaugeId(idx as u32))
    }

    /// Borrow a counter by id. Used on the hot path; never
    /// allocates.
    #[inline]
    pub fn counter(&self, id: CounterId) -> &Counter {
        &self.counters[id.0 as usize]
    }

    /// Borrow a gauge by id. Used on the hot path; never
    /// allocates.
    #[inline]
    pub fn gauge(&self, id: GaugeId) -> &Gauge {
        &self.gauges[id.0 as usize]
    }

    /// Number of registered counters.
    #[inline]
    pub fn counters_len(&self) -> usize {
        self.n_counters as usize
    }

    /// Number of registered gauges.
    #[inline]
    pub fn gauges_len(&self) -> usize {
        self.n_gauges as usize
    }

    /// Serialize every registered metric in Prometheus text format
    /// into `dst`. Returns the number of bytes written.
    ///
    /// Prometheus shape:
    /// ```text
    /// # TYPE engine_ticks_total counter
    /// engine_ticks_total 12345
    /// # TYPE engine_book_mid gauge
    /// engine_book_mid 500000
    /// ```
    pub fn encode_prometheus(&self, dst: &mut [u8]) -> Result<usize, EncodeErr> {
        let mut c = Cursor::new(dst);
        for i in 0..self.n_counters as usize {
            let m = &self.counters[i];
            c.put(b"# TYPE ")?;
            c.put(m.name())?;
            c.put(b" counter\n")?;
            c.put(m.name())?;
            c.put(b" ")?;
            c.put_u64(m.get())?;
            c.put(b"\n")?;
        }
        for i in 0..self.n_gauges as usize {
            let m = &self.gauges[i];
            c.put(b"# TYPE ")?;
            c.put(m.name())?;
            c.put(b" gauge\n")?;
            c.put(m.name())?;
            c.put(b" ")?;
            c.put_i64(m.get())?;
            c.put(b"\n")?;
        }
        Ok(c.pos)
    }
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// -----------------------------------------------------------------
// Cursor
// -----------------------------------------------------------------

struct Cursor<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    #[inline]
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline]
    fn put(&mut self, src: &[u8]) -> Result<(), EncodeErr> {
        let end = self
            .pos
            .checked_add(src.len())
            .ok_or(EncodeErr::BufferTooSmall)?;
        if end > self.buf.len() {
            return Err(EncodeErr::BufferTooSmall);
        }
        self.buf[self.pos..end].copy_from_slice(src);
        self.pos = end;
        Ok(())
    }

    /// Decimal u64, max 20 chars.
    fn put_u64(&mut self, mut v: u64) -> Result<(), EncodeErr> {
        if v == 0 {
            return self.put(b"0");
        }
        let mut tmp = [0u8; 20];
        let mut i = tmp.len();
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        self.put(&tmp[i..])
    }

    /// Decimal i64. Max 21 chars (incl. minus).
    fn put_i64(&mut self, v: i64) -> Result<(), EncodeErr> {
        if v < 0 {
            self.put(b"-")?;
            // i64::MIN edge case: -i64::MIN overflows; manual.
            if v == i64::MIN {
                return self.put(b"9223372036854775808");
            }
            return self.put_u64((-v) as u64);
        }
        self.put_u64(v as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_counter_then_inc_and_read() {
        let mut r = MetricsRegistry::new();
        let id = r.register_counter("engine_ticks_total").unwrap();
        r.counter(id).inc(3);
        r.counter(id).inc(2);
        assert_eq!(r.counter(id).get(), 5);
    }

    #[test]
    fn register_gauge_then_set_and_read() {
        let mut r = MetricsRegistry::new();
        let id = r.register_gauge("engine_book_mid").unwrap();
        r.gauge(id).set(-42);
        assert_eq!(r.gauge(id).get(), -42);
    }

    #[test]
    fn register_rejects_empty_name() {
        let mut r = MetricsRegistry::new();
        assert_eq!(r.register_counter(""), Err(RegErr::BadName));
    }

    #[test]
    fn register_rejects_oversized_name() {
        let mut r = MetricsRegistry::new();
        let too_long = "x".repeat(NAME_MAX + 1);
        assert_eq!(r.register_counter(&too_long), Err(RegErr::BadName));
    }

    #[test]
    fn register_rejects_duplicates() {
        let mut r = MetricsRegistry::new();
        r.register_counter("dup").unwrap();
        assert_eq!(r.register_counter("dup"), Err(RegErr::Duplicate));
    }

    #[test]
    fn register_fills_to_capacity() {
        let mut r = MetricsRegistry::new();
        for i in 0..MAX_COUNTERS {
            r.register_counter(&format!("c_{i}")).unwrap();
        }
        assert_eq!(r.register_counter("overflow"), Err(RegErr::Full));
    }

    #[test]
    fn encode_prometheus_emits_canonical_format() {
        let mut r = MetricsRegistry::new();
        let c = r.register_counter("engine_ticks_total").unwrap();
        let g = r.register_gauge("engine_book_mid").unwrap();
        r.counter(c).inc(42);
        r.gauge(g).set(500_000);

        let mut buf = [0u8; 4096];
        let n = r.encode_prometheus(&mut buf).unwrap();
        let s = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(s.contains("# TYPE engine_ticks_total counter\n"));
        assert!(s.contains("engine_ticks_total 42\n"));
        assert!(s.contains("# TYPE engine_book_mid gauge\n"));
        assert!(s.contains("engine_book_mid 500000\n"));
    }

    #[test]
    fn encode_returns_overflow_on_tiny_buffer() {
        let mut r = MetricsRegistry::new();
        r.register_counter("x").unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(r.encode_prometheus(&mut buf), Err(EncodeErr::BufferTooSmall));
    }

    #[test]
    fn encode_handles_negative_gauge() {
        let mut r = MetricsRegistry::new();
        let g = r.register_gauge("neg").unwrap();
        r.gauge(g).set(-1);
        let mut buf = [0u8; 256];
        let n = r.encode_prometheus(&mut buf).unwrap();
        let s = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(s.contains("neg -1\n"));
    }

    #[test]
    fn encode_handles_i64_min() {
        let mut r = MetricsRegistry::new();
        let g = r.register_gauge("min").unwrap();
        r.gauge(g).set(i64::MIN);
        let mut buf = [0u8; 256];
        let n = r.encode_prometheus(&mut buf).unwrap();
        let s = std::str::from_utf8(&buf[..n]).unwrap();
        assert!(s.contains("min -9223372036854775808\n"));
    }
}
