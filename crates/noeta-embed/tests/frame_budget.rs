//! The 60fps proof (server-hmr E2): a host loop calls `update(dt)` every frame, hot-swaps an
//! edit mid-run, and no frame — the swap frame included — blows the 16.6ms budget. `#[ignore]`
//! because it asserts wall-clock timing: `cargo test -p noeta-embed -- --ignored` (release
//! recommended; the debug-build budget below is deliberately generous headroom over 60fps).

use std::time::{Duration, Instant};

use noeta_embed::{Session, SwapOutcome, Value};

fn script(bonus: i64) -> String {
    format!(
        "use std.reactive.{{signal}}\n\
         score = signal(0)\n\
         fn update(dt: float): int {{\n\
         \x20   mut acc = 0\n\
         \x20   for i in 0..200 {{\n\
         \x20       acc = acc + i\n\
         \x20   }}\n\
         \x20   score.update(fn(s) {{ return s + 1 }})\n\
         \x20   return score.get() + {bonus}\n\
         }}\n"
    )
}

#[test]
#[ignore = "asserts wall-clock timing; run explicitly"]
fn a_swap_mid_frame_loop_stays_inside_the_frame_budget() {
    // 16.6ms is the 60fps frame; even a debug build must fit a call AND the swap inside it.
    let budget = Duration::from_micros(16_600);
    let mut session = Session::new(&script(0)).unwrap();

    // Warm-up (map the session's steady state, populate caches).
    for _ in 0..100 {
        session.call("update", &[Value::Float(0.016)]).unwrap();
    }

    let mut worst_call = Duration::ZERO;
    let mut swap_cost = Duration::ZERO;
    for frame in 0..300u32 {
        let start = Instant::now();
        if frame == 150 {
            // The developer saved an edit; the engine swaps it in on the frame boundary.
            let outcome = session.hot_swap(&script(1000)).unwrap();
            assert!(matches!(outcome, SwapOutcome::Swapped { .. }));
            swap_cost = start.elapsed();
        }
        let value = session.call("update", &[Value::Float(0.016)]).unwrap();
        let frame_time = start.elapsed();
        worst_call = worst_call.max(frame_time);
        assert!(
            frame_time < budget,
            "frame {frame} took {frame_time:?} (budget {budget:?}; swap {swap_cost:?})"
        );
        // The swap frame observes the new body over the preserved score.
        if frame >= 150 {
            let Value::Int(n) = value else { panic!() };
            assert!(n > 1000, "post-swap frames run the new body: {n}");
        }
    }
    eprintln!("worst frame {worst_call:?}, swap cost {swap_cost:?} (budget {budget:?})");
}
