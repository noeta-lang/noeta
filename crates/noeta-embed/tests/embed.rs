//! The embed API's behavior pins (server-hmr E0): call-by-name with the value bridge, state
//! persistence across calls, panic recovery, and hot-swap with the reactive state rule — the
//! same guarantees the dev server has, driven by a host process.

use noeta_embed::{Session, SwapOutcome, Value};

#[test]
fn calls_bridge_values_both_ways() {
    let mut s = Session::new(
        "fn add(a: int, b: int): int { return a + b; }\n\
         fn greet(name: string): string { return \"hi ${name}\"; }\n\
         fn half(x: float): float { return x / 2.0; }\n\
         fn flags(): List<bool> { return [true, false]; }\n\
         fn pair(): Map<string, int> { return {\"a\": 1, \"b\": 2}; }\n",
    )
    .unwrap();
    assert_eq!(
        s.call("add", &[Value::Int(2), Value::Int(3)]).unwrap(),
        Value::Int(5)
    );
    assert_eq!(
        s.call("greet", &[Value::from("Ada")]).unwrap(),
        Value::from("hi Ada")
    );
    assert_eq!(
        s.call("half", &[Value::Float(3.0)]).unwrap(),
        Value::Float(1.5)
    );
    assert_eq!(
        s.call("flags", &[]).unwrap(),
        Value::List(vec![Value::Bool(true), Value::Bool(false)])
    );
    assert_eq!(
        s.call("pair", &[]).unwrap(),
        Value::Map(vec![
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Int(2)),
        ])
    );
}

#[test]
fn lists_and_maps_round_trip_through_arguments() {
    let mut s = Session::new(
        "fn sum(xs: List<int>): int {\n\
         \x20   mut total = 0\n\
         \x20   for x in xs { total = total + x }\n\
         \x20   return total\n\
         }\n\
         fn get(m: Map<string, int>, k: string): int { return m[k]; }\n",
    )
    .unwrap();
    assert_eq!(
        s.call("sum", &[Value::from(vec![1i64, 2, 3, 4])]).unwrap(),
        Value::Int(10)
    );
    let map = Value::Map(vec![("x".to_string(), Value::Int(42))]);
    assert_eq!(
        s.call("get", &[map, Value::from("x")]).unwrap(),
        Value::Int(42)
    );
}

#[test]
fn state_persists_across_calls_and_panics_recover() {
    let mut s = Session::new(
        "use std.reactive.{signal}\n\
         count = signal(0)\n\
         fn bump(): int {\n\
         \x20   count.update(fn(n) { return n + 1 })\n\
         \x20   return count.get()\n\
         }\n\
         fn boom(): int { panic(\"deliberate\"); }\n",
    )
    .unwrap();
    assert_eq!(s.call("bump", &[]).unwrap(), Value::Int(1));
    assert_eq!(s.call("bump", &[]).unwrap(), Value::Int(2));
    // A panic reports and the session SURVIVES with its state.
    let err = s.call("boom", &[]).unwrap_err();
    assert!(err.to_string().contains("deliberate"), "{err}");
    assert_eq!(s.call("bump", &[]).unwrap(), Value::Int(3));
    // Unknown functions are a typed error, not a panic.
    assert!(matches!(
        s.call("nope", &[]),
        Err(noeta_embed::Error::NoSuchFunction(_))
    ));
}

#[test]
fn stdout_is_drained_on_demand() {
    let mut s = Session::new("fn say(): int { echo \"hello\"; return 1; }\n").unwrap();
    s.call("say", &[]).unwrap();
    s.call("say", &[]).unwrap();
    assert_eq!(s.take_stdout(), "hello\nhello\n");
    assert_eq!(s.take_stdout(), "");
}

#[test]
fn hot_swap_keeps_reactive_state_and_reports_the_outcome() {
    let v = |formula: &str| {
        format!(
            "use std.reactive.{{signal}}\n\
             score = signal(0)\n\
             fn update(dt: float): int {{\n\
             \x20   score.update(fn(s) {{ return s + 1 }})\n\
             \x20   return {formula}\n\
             }}\n"
        )
    };
    let mut s = Session::new(&v("score.get()")).unwrap();
    assert_eq!(
        s.call("update", &[Value::Float(0.016)]).unwrap(),
        Value::Int(1)
    );
    assert_eq!(
        s.call("update", &[Value::Float(0.016)]).unwrap(),
        Value::Int(2)
    );

    // The edit: update's body changes; `score` (unchanged reactive anchor) survives.
    let outcome = s.hot_swap(&v("score.get() * 10")).unwrap();
    assert_eq!(
        outcome,
        SwapOutcome::Swapped {
            changed: vec!["update".to_string()],
            preserved: vec![],
        }
    );
    assert_eq!(
        s.call("update", &[Value::Float(0.016)]).unwrap(),
        Value::Int(30),
        "the third frame runs the NEW body over the PRESERVED score"
    );

    // A layout change is the host's decision, not a silent reload.
    let with_struct = format!("{}struct P {{ x: int }}\n", v("score.get() * 10"));
    s.hot_swap(&with_struct).unwrap();
    let changed_layout = format!("{}struct P {{ x: int; y: int }}\n", v("score.get() * 10"));
    let outcome = s.hot_swap(&changed_layout).unwrap();
    let SwapOutcome::NeedsRestart(reasons) = outcome else {
        panic!("a layout change needs a restart, got {outcome:?}");
    };
    assert!(reasons[0].contains("layout"), "{reasons:?}");
    // …and the old code keeps running until the host reloads.
    assert_eq!(
        s.call("update", &[Value::Float(0.016)]).unwrap(),
        Value::Int(40)
    );

    // Red code is transactional: an error changes nothing.
    assert!(s.hot_swap("fn update(: int").is_err());
    assert_eq!(
        s.call("update", &[Value::Float(0.016)]).unwrap(),
        Value::Int(50)
    );
}

#[test]
fn eval_is_the_debug_console_escape_hatch() {
    let mut s = Session::new("use std.reactive.{signal}\nhp = signal(100)\n").unwrap();
    assert_eq!(s.eval("hp.get()").unwrap(), Some("100".to_string()));
    s.eval("hp.set(55)").unwrap();
    assert_eq!(s.eval("hp.get()").unwrap(), Some("55".to_string()));
}
