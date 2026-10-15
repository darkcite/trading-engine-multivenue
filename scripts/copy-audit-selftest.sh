#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# scripts/copy-audit-selftest.sh — proves copy-audit.sh's reading of
# `#[cfg(test)]` items on fixtures before the gate trusts it (BX0).
#
# The sweep skips a `#[cfg(test)]` ITEM and resumes after it. A wrong
# skip fails in the worst direction — live code silently unaudited — so
# `make copy-audit` runs this first and it checks two things:
#   * ok/: every live copy verb (the lines marked `// FLAG`) is reported
#     and no copy inside a test item is — after a test-only method, an
#     empty-bodied one, a long signature, a braced `use`, an empty impl,
#     a braced const, a char-literal brace, a one-line item, a test
#     statement and a block-shaped one; and a `COPY:` marker inside a
#     test item never covers the live copy below it;
#   * bad-*/: every construct the sweep cannot delimit — `#[cfg(test)]`
#     on a struct field, an enum variant, a match arm, a struct-literal
#     field or a multi-line array, and an item never closed at its
#     indentation — fails the sweep (exit 3) instead of swallowing code.

set -u
cd "$(dirname "$0")/.." || exit 2
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fail=0

mkdir -p "$tmp/ok"
cat > "$tmp/ok/items.rs" <<'EOF'
fn before() { a.copy_from_slice(b); } // FLAG

impl X {
    #[cfg(test)]
    pub fn test_only(&mut self) {
        let v = y.to_vec();
        if v.is_empty() {
            z.copy_from_slice(w);
        }
    }

    fn after_method(&mut self) {
        c.copy_from_slice(d); // FLAG
    }

    #[cfg(test)]
    pub fn reset(&mut self) {}
    fn after_empty_body(&mut self) { e.copy_from_slice(f); } // FLAG

    #[cfg(test)]
    #[inline]
    fn long_signature(
        &mut self,
        x: u8,
    ) -> u8 {
        g.copy_from_slice(h);
        x
    }
    fn after_long_signature(&mut self) { i.copy_from_slice(j); } // FLAG
}

#[cfg(test)]
use std::sync::{Arc, Mutex};
impl Y {
    fn after_braced_use(&self) { k.copy_from_slice(l); } // FLAG
}

#[cfg(test)]
use something::Helper;
fn after_use() { m.copy_from_slice(n); } // FLAG

#[cfg(test)]
impl Marker for T {}
fn after_empty_impl() { o.copy_from_slice(p); } // FLAG

#[cfg(test)]
const C: Cfg = Cfg { depth: 1 };
fn after_const_literal() { q.copy_from_slice(r); } // FLAG

#[cfg(test)]
const OPEN: u8 = b'{';
fn after_char_literal() { s.copy_from_slice(t); } // FLAG

#[cfg(test)] fn one_liner() { u.copy_from_slice(v) } // on the attribute line
fn after_one_liner() { w.copy_from_slice(x); } // FLAG

fn statements(&mut self) {
    #[cfg(test)]
    self.probe += 1;
    y.copy_from_slice(z); // FLAG
    #[cfg(test)]
    let s = S {
        a: 1,
    };
    aa.copy_from_slice(bb); // FLAG
}

fn a_marker_in_a_test_item_covers_nothing() {
    #[cfg(test)]
    fn helper() {
        // COPY: a marker inside a test item
    }
    cc.copy_from_slice(dd); // FLAG
}

fn a_live_marker_still_covers() {
    // COPY: a live, justified copy
    ee.copy_from_slice(ff);
}

#[cfg(test)]
mod tests {
    fn t() {
        gg.copy_from_slice(hh);
    }
}

fn after_tests() { ii.copy_from_slice(jj); } // FLAG
EOF

want="$(grep -n '// FLAG' "$tmp/ok/items.rs" | cut -d: -f1 | sort -n | tr '\n' ' ')"
got="$(bash scripts/copy-audit.sh "$tmp/ok" 2>/dev/null \
    | sed -n 's|^.*/items\.rs:\([0-9][0-9]*\): .*|\1|p' | sort -n | tr '\n' ' ')"
if [ "$want" != "$got" ]; then
    printf 'copy-audit-selftest: FAIL ok/ — want lines [%s] got [%s]\n' "$want" "$got" 1>&2
    fail=1
fi

refused=0
bad_case() {
    mkdir -p "$tmp/bad-$1"
    cat > "$tmp/bad-$1/case.rs"
    bash scripts/copy-audit.sh "$tmp/bad-$1" >/dev/null 2>&1
    local rc=$?
    if [ "$rc" -ne 3 ]; then
        printf 'copy-audit-selftest: FAIL bad-%s — exit %s, want 3\n' "$1" "$rc" 1>&2
        fail=1
    else
        refused=$((refused + 1))
    fi
}

bad_case field <<'EOF'
struct S {
    #[cfg(test)]
    probe: u8,
    live: u8,
}
fn after() { a.copy_from_slice(b); }
EOF

bad_case variant <<'EOF'
enum E {
    #[cfg(test)]
    Probe,
    Live,
}
fn after() { a.copy_from_slice(b); }
EOF

bad_case arm <<'EOF'
fn f(x: u8) -> u8 {
    match x {
        #[cfg(test)]
        0 => 1,
        _ => 2,
    }
}
fn after() { a.copy_from_slice(b); }
EOF

bad_case literal-field <<'EOF'
fn g() -> S {
    S {
        #[cfg(test)]
        probe: 1,
        live: 2,
    }
}
fn after() { a.copy_from_slice(b); }
EOF

bad_case array <<'EOF'
#[cfg(test)]
const CASES: &[Case] = &[
    Case { a: 1 },
];
fn after() { a.copy_from_slice(b); }
EOF

bad_case unclosed <<'EOF'
impl X {
    #[cfg(test)]
    fn broken(&self) {
        a.copy_from_slice(b);
  }
}
fn later() { c.copy_from_slice(d); }
EOF

if [ "$fail" -ne 0 ]; then
    exit 1
fi
flags=$(printf '%s' "$want" | wc -w | tr -d ' ')
printf 'copy-audit-selftest: OK (%s live copies found, none inside a test item; %s undelimitable constructs refused)\n' "$flags" "$refused"
