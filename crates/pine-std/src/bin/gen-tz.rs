// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: operator tool — regenerates `src/load/tz_table.rs` from TZif files; allocates
//! and copies freely (impl-04 §4.12).
//!
//! Usage: `gen-tz <zoneinfo dir> > crates/pine-std/src/load/tz_table.rs`, then rustfmt the file.
//! Transitions 2000-01-01 … 2040-01-01 come from the v2 data block, extended past its last
//! explicit transition by the POSIX TZ footer (`Mm.w.d` rules only; anything else is a hard error).

use std::fmt::Write as _;

const FROM: i64 = 946_684_800; // 2000-01-01T00:00:00Z
const UNTIL: i64 = 2_208_988_800; // 2040-01-01T00:00:00Z

/// The zone list of impl-04 §4.12, `UTC` first (`TZ_UTC == TzId(0)`). `(name, file)`.
const ZONES: [(&str, &str); 25] = [
    ("UTC", "UTC"),
    ("Etc/UTC", "Etc/UTC"),
    ("GMT", "GMT"),
    ("Etc/GMT", "Etc/GMT"),
    ("America/New_York", "America/New_York"),
    ("America/Chicago", "America/Chicago"),
    ("America/Denver", "America/Denver"),
    ("America/Los_Angeles", "America/Los_Angeles"),
    ("America/Toronto", "America/Toronto"),
    ("America/Sao_Paulo", "America/Sao_Paulo"),
    ("Europe/London", "Europe/London"),
    ("Europe/Berlin", "Europe/Berlin"),
    ("Europe/Paris", "Europe/Paris"),
    ("Europe/Zurich", "Europe/Zurich"),
    ("Europe/Moscow", "Europe/Moscow"),
    ("Asia/Tokyo", "Asia/Tokyo"),
    ("Asia/Shanghai", "Asia/Shanghai"),
    ("Asia/Hong_Kong", "Asia/Hong_Kong"),
    ("Asia/Singapore", "Asia/Singapore"),
    ("Asia/Kolkata", "Asia/Kolkata"),
    ("Asia/Calcutta", "Asia/Kolkata"),
    ("Asia/Dubai", "Asia/Dubai"),
    ("Asia/Seoul", "Asia/Seoul"),
    ("Australia/Sydney", "Australia/Sydney"),
    ("Pacific/Auckland", "Pacific/Auckland"),
];

fn be32(d: &[u8], o: usize) -> i64 {
    i32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]) as i64
}

fn be64(d: &[u8], o: usize) -> i64 {
    let mut b = [0u8; 8];
    // COPY: 8 bytes of a TZif timestamp into a fixed array for from_be_bytes.
    b.copy_from_slice(&d[o..o + 8]);
    i64::from_be_bytes(b)
}

/// (counts: isut, isstd, leap, time, type, char) of the header at `o`.
fn header(d: &[u8], o: usize) -> [usize; 6] {
    assert!(&d[o..o + 4] == b"TZif", "not a TZif file");
    let mut c = [0usize; 6];
    for (k, v) in c.iter_mut().enumerate() {
        *v = be32(d, o + 20 + 4 * k) as usize;
    }
    c
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn year_of(t: i64) -> i64 {
    let z = t.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    yoe + era * 400 + i64::from(m <= 2)
}

struct Posix<'a> {
    s: &'a [u8],
    i: usize,
}

impl Posix<'_> {
    fn name(&mut self) {
        if self.s.get(self.i) == Some(&b'<') {
            while self.s[self.i] != b'>' {
                self.i += 1;
            }
            self.i += 1;
        } else {
            while self.i < self.s.len() && self.s[self.i].is_ascii_alphabetic() {
                self.i += 1;
            }
        }
    }
    fn num(&mut self) -> i64 {
        let mut v = 0i64;
        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
            v = v * 10 + (self.s[self.i] - b'0') as i64;
            self.i += 1;
        }
        v
    }
    /// `[+-]hh[:mm[:ss]]` in seconds.
    fn hms(&mut self) -> i64 {
        let neg = match self.s.get(self.i) {
            Some(b'-') => {
                self.i += 1;
                true
            }
            Some(b'+') => {
                self.i += 1;
                false
            }
            _ => false,
        };
        let mut v = self.num() * 3600;
        if self.s.get(self.i) == Some(&b':') {
            self.i += 1;
            v += self.num() * 60;
            if self.s.get(self.i) == Some(&b':') {
                self.i += 1;
                v += self.num();
            }
        }
        if neg {
            -v
        } else {
            v
        }
    }
    /// `Mm.w.d[/time]` → (m, w, d, secs).
    fn rule(&mut self) -> (i64, i64, i64, i64) {
        assert!(
            self.s.get(self.i) == Some(&b'M'),
            "only Mm.w.d footer rules are supported"
        );
        self.i += 1;
        let m = self.num();
        self.i += 1;
        let w = self.num();
        self.i += 1;
        let d = self.num();
        let t = if self.s.get(self.i) == Some(&b'/') {
            self.i += 1;
            self.hms()
        } else {
            7200
        };
        (m, w, d, t)
    }
}

fn rule_day(y: i64, (m, w, d, _): (i64, i64, i64, i64)) -> i64 {
    let first = days_from_civil(y, m, 1);
    let wd0 = (first + 4).rem_euclid(7);
    let mut day = 1 + (d - wd0).rem_euclid(7) + (w - 1) * 7;
    let next = if m == 12 {
        days_from_civil(y + 1, 1, 1)
    } else {
        days_from_civil(y, m + 1, 1)
    };
    while first + day > next {
        day -= 7;
    }
    first + day - 1
}

/// (base offset, transitions) of one TZif file.
fn zone(d: &[u8]) -> (i32, Vec<(i64, i32)>) {
    let c = header(d, 0);
    let v1 = 44 + c[3] * 5 + c[4] * 6 + c[5] + c[2] * 8 + c[1] + c[0];
    let c2 = header(d, v1);
    let o = v1 + 44;
    let (tc, ty) = (c2[3], c2[4]);
    let times: Vec<i64> = (0..tc).map(|k| be64(d, o + 8 * k)).collect();
    let idx: Vec<usize> = (0..tc).map(|k| d[o + 8 * tc + k] as usize).collect();
    let tt = o + 9 * tc;
    let utoff: Vec<i32> = (0..ty).map(|k| be32(d, tt + 6 * k) as i32).collect();
    let end = tt + 6 * ty + c2[5] + c2[2] * 12 + c2[1] + c2[0];
    let mut base = utoff[0];
    let mut out: Vec<(i64, i32)> = Vec::new();
    let mut last_t = i64::MIN;
    for k in 0..tc {
        let off = utoff[idx[k]];
        last_t = times[k];
        if times[k] <= FROM {
            base = off;
        } else if times[k] < UNTIL {
            out.push((times[k], off));
        }
    }
    let footer = &d[end + 1..d.len() - 1];
    let mut p = Posix { s: footer, i: 0 };
    p.name();
    let std_off = -p.hms();
    if p.i < footer.len() {
        p.name();
        let dst_off = if p.i < footer.len() && footer[p.i] != b',' {
            -p.hms()
        } else {
            std_off + 3600
        };
        assert!(footer.get(p.i) == Some(&b','), "footer without rules");
        p.i += 1;
        let start = p.rule();
        assert!(footer.get(p.i) == Some(&b','), "footer without an end rule");
        p.i += 1;
        let stop = p.rule();
        let y0 = if last_t == i64::MIN {
            2000
        } else {
            year_of(last_t)
        };
        for y in y0..2040 {
            let a = rule_day(y, start) * 86_400 + start.3 - std_off;
            let b = rule_day(y, stop) * 86_400 + stop.3 - dst_off;
            let mut pair = [(a, dst_off as i32), (b, std_off as i32)];
            pair.sort();
            for (t, off) in pair {
                if t > last_t && t > FROM && t < UNTIL {
                    out.push((t, off));
                }
            }
        }
    } else if last_t == i64::MIN {
        base = std_off as i32;
    }
    let mut dedup: Vec<(i64, i32)> = Vec::new();
    let mut cur = base;
    for (t, off) in out {
        if off != cur {
            dedup.push((t, off));
            cur = off;
        }
    }
    (base, dedup)
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/usr/share/zoneinfo".to_string());
    let mut s = String::new();
    s.push_str("// SPDX-License-Identifier: Apache-2.0\n// Copyright 2026 Anton (darkcite)\n\n");
    s.push_str("//! **Generated** by `gen-tz` from TZif files — do not edit (impl-04 §4.12).\n");
    s.push_str(
        "//! Transitions 2000-01-01 … 2040-01-01 (UTC seconds); the first and last offsets\n",
    );
    s.push_str("//! hold outside. Input list (name ← file):\n");
    for (n, f) in ZONES {
        let _ = writeln!(s, "//! - `{n}` ← `{f}`");
    }
    s.push_str("\nuse super::tz::Zone;\n\n");
    let mut files: Vec<&str> = Vec::new();
    let mut names = String::new();
    for (n, f) in ZONES {
        let z = match files.iter().position(|x| *x == f) {
            Some(p) => p,
            None => {
                files.push(f);
                files.len() - 1
            }
        };
        let _ = writeln!(names, "(b\"{n}\", {z}),");
    }
    let _ = writeln!(s, "/// Zone names → `ZONES` index (aliases share a zone).");
    let _ = writeln!(
        s,
        "pub(super) static NAMES: [(&[u8], u16); {}] = [\n{names}];\n",
        ZONES.len()
    );
    let _ = writeln!(
        s,
        "/// One zone per distinct file, in `NAMES` order of first use."
    );
    let _ = writeln!(s, "pub(super) static ZONES: [Zone; {}] = [", files.len());
    for f in &files {
        let path = format!("{dir}/{f}");
        let d = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let (base, tr) = zone(&d);
        let at: Vec<String> = tr.iter().map(|x| x.0.to_string()).collect();
        let off: Vec<String> = tr.iter().map(|x| x.1.to_string()).collect();
        let _ = writeln!(
            s,
            "// {f}\nZone {{ base: {base}, at: &[{}], off: &[{}] }},",
            at.join(", "),
            off.join(", ")
        );
    }
    s.push_str("];\n");
    print!("{s}");
}
