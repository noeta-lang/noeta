//! The engine-shaped demo (server-hmr E3): a host process exposes its own API to scripts through
//! a custom [`Extension`], drives the session from its frame loop, and hot-swaps an "edit" in
//! mid-run — reactive script state (the player's score) survives the swap.
//!
//!     cargo run -p noeta-embed --example engine
//!
//! The engine surface here is one module (`demo.engine`) with `emit(event)` — scripts push
//! events, the engine drains them per frame. A real engine grows this the same way: more
//! functions, extern types for entity handles, a custom `Host` for its IO world.

use std::sync::Mutex;

use noeta_embed::{Session, SwapOutcome, Value};
use noeta_native::registry::{ExtFn, ExtModule, Extension, NativeOut, NativeValue, RetTy, SigType};
use noeta_native::{Host, StdError, no_function_error};

/// The engine's event queue — scripts write via `engine.emit`, the frame loop drains.
static EVENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

static ENGINE_FNS: &[ExtFn] = &[ExtFn {
    name: "emit",
    params: &[SigType::String],
    ret: RetTy::Concrete(SigType::Unit),
}];

fn engine_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "emit" => {
            if let Some(NativeValue::Str(event)) = args.first() {
                EVENTS.lock().unwrap().push(event.clone());
            }
            Ok(NativeOut::Unit)
        }
        _ => Err(no_function_error("engine", func)),
    }
}

struct DemoEngineExtension;

impl Extension for DemoEngineExtension {
    fn name(&self) -> &'static str {
        "demo"
    }
    fn modules(&self) -> &'static [ExtModule] {
        static MODULES: &[ExtModule] = &[ExtModule {
            name: "engine",
            functions: ENGINE_FNS,
            dispatch: engine_dispatch,
            ..ExtModule::DEFAULTS
        }];
        MODULES
    }
}

fn script(multiplier: i64) -> String {
    format!(
        "use demo.engine\n\
         use std.reactive.{{signal}}\n\
         score = signal(0)\n\
         fn update(dt: float): int {{\n\
         \x20   score.update(fn(s) {{ return s + 1 }})\n\
         \x20   if score.get() % 3 == 0 {{\n\
         \x20       engine.emit(\"milestone ${{score.get() * {multiplier}}}\")\n\
         \x20   }}\n\
         \x20   return score.get() * {multiplier}\n\
         }}\n"
    )
}

fn main() {
    // The engine's API surface registers once per process, before the first session.
    noeta_embed::install_extensions(vec![&DemoEngineExtension]);

    let mut session = Session::new(&script(1)).expect("the script loads");
    println!("engine: session up — running 6 frames of v1");
    for frame in 0..6 {
        let score = session.call("update", &[Value::Float(0.016)]).unwrap();
        println!("frame {frame}: score = {score:?}");
    }
    for event in EVENTS.lock().unwrap().drain(..) {
        println!("engine event: {event}");
    }

    // The developer edits the multiplier; the engine's watcher (simulated) swaps it in. The
    // score — reactive state — survives; the next frame runs the new body over it.
    println!("engine: hot-swapping v2 (multiplier 100) — score must survive");
    match session.hot_swap(&script(100)).unwrap() {
        SwapOutcome::Swapped { changed, .. } => println!("engine: swapped {changed:?}"),
        other => panic!("expected a swap, got {other:?}"),
    }
    for frame in 6..9 {
        let score = session.call("update", &[Value::Float(0.016)]).unwrap();
        println!("frame {frame}: score = {score:?}");
    }
    for event in EVENTS.lock().unwrap().drain(..) {
        println!("engine event: {event}");
    }
    let Value::Int(final_score) = session.call("update", &[Value::Float(0.016)]).unwrap() else {
        panic!()
    };
    assert_eq!(
        final_score, 1000,
        "10 frames of preserved score × the swapped multiplier"
    );
    println!("engine: state survived the swap — final score {final_score}");
}
