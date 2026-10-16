// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.3 — the shared-series store and the tier-1 `BarCtx` (T6).

use pine_ir::BarCtx;
use pine_screen::{ScreenCtx, SeriesSpec, SeriesStore, SlotKey};

#[test]
fn ctx_series_store_views_are_contiguous() {
    let mut st = SeriesStore::with_slots(&[
        SeriesSpec {
            n_out: 1,
            history: 2,
        },
        SeriesSpec {
            n_out: 2,
            history: 3,
        },
    ]);
    assert_eq!(st.len(), 2);
    for i in 0..10i64 {
        let v = i as f64;
        st.publish(1, &[v, -v], i * 10, i * 10 + 10, true);
    }
    let a = st.view(1, 0).expect("published");
    assert_eq!(a.values, &[7.0, 8.0, 9.0]);
    assert_eq!(a.open_ms, &[70, 80, 90]);
    assert_eq!(a.close_ms, &[80, 90, 100]);
    assert!(a.last_confirmed);
    let b = st.view(1, 1).expect("published");
    assert_eq!(b.values, &[-7.0, -8.0, -9.0]);
    assert!(st.view(0, 0).is_none(), "slot 0 untouched");
    st.clear();
    assert!(st.view(1, 0).is_none(), "clear forgets");
}

#[test]
fn ctx_series_store_developing_is_replaced() {
    let mut st = SeriesStore::with_slots(&[SeriesSpec {
        n_out: 1,
        history: 4,
    }]);
    st.publish(0, &[1.0], 0, 10, true);
    st.publish(0, &[2.0], 10, 20, false);
    st.publish(0, &[3.0], 10, 20, false);
    let v = st.view(0, 0).expect("published");
    assert_eq!(v.values, &[1.0, 3.0]);
    assert!(!v.last_confirmed);
    st.publish(0, &[4.0], 10, 20, true);
    let v = st.view(0, 0).expect("published");
    assert_eq!(v.values, &[1.0, 4.0]);
    assert!(v.last_confirmed);
    st.publish(0, &[5.0], 20, 30, true);
    assert_eq!(st.view(0, 0).expect("published").values, &[1.0, 4.0, 5.0]);
}

#[test]
fn ctx_series_store_empty_slot_is_none() {
    let mut st = SeriesStore::with_slots(&[SeriesSpec {
        n_out: 2,
        history: 2,
    }]);
    assert!(st.view(0, 0).is_none());
    st.publish(0, &[1.0, 2.0], 0, 10, true);
    assert!(st.view(0, 2).is_none(), "out_idx ≥ n_out");
    assert!(st.view(1, 0).is_none(), "no such slot");
    assert!(SeriesStore::with_slots(&[]).is_empty());
}

#[test]
fn ctx_screen_ctx_reads_only_its_own_keys() {
    let mut st = SeriesStore::with_slots(&[
        SeriesSpec {
            n_out: 1,
            history: 2,
        },
        SeriesSpec {
            n_out: 1,
            history: 2,
        },
    ]);
    st.publish(0, &[1.0], 0, 10, true);
    st.publish(1, &[2.0], 0, 10, true);
    let mine = [SlotKey { key: 0xAA, slot: 0 }];
    let ctx = ScreenCtx {
        store: &st,
        keys: &mine,
        broker: None,
        now_ms: 42,
    };
    assert_eq!(ctx.shared_series(0xAA, 0).expect("own key").values, &[1.0]);
    assert!(
        ctx.shared_series(0xBB, 0).is_none(),
        "slot 1's key is not in this reader's map"
    );
    assert!(ctx.broker().is_none());
    assert!(ctx.open_trade(0).is_none());
    assert!(ctx.closed_trade(0).is_none());
    assert_eq!(ctx.now_ms(), 42);
}
