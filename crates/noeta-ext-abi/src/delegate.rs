//! Host-capability **delegation** (audit-2 F7 / audit-6 F9): forwarding `impl`s for the [`crate::Host`]
//! union's capability traits, generated per capability by [`delegate_host!`].
//!
//! A custom `Host` is otherwise all-or-nothing: the union is 12 supertraits and ~70 required
//! methods, so an embedder that wants to override ONE capability (expose the engine's world as
//! the fs/env, say) used to hand-write ~70 forwarding methods around a `SandboxHost` — and every
//! capability later added to the union silently broke every out-of-tree host. With the macro, a
//! wrapper hand-implements only what it changes and names the rest:
//!
//! ```ignore
//! struct EngineHost { base: SandboxHost, world: World }
//!
//! // Env is hand-written (the override)…
//! impl noeta_ext_abi::Env for EngineHost { /* … the engine's view … */ }
//!
//! // …everything else forwards to the base. `EngineHost` is now a full `Host`.
//! noeta_ext_abi::delegate_host!(EngineHost => base :
//!     FileReader, FileSystem, Rng, Clock, Os, Entropy, Ids, Network, P2pProvider,
//!     Tracing, Metrics, Logging);
//! ```
//!
//! The same arms serve the **component** direction (audit-6 F9): a host that embeds per-capability
//! state structs (a seeded RNG, a logical clock) forwards each capability to its own field with one
//! invocation per field — `SandboxHost` in `noeta-stdlib` is the in-tree proof.
//!
//! Every arm forwards the trait's **provided** methods too, not only the required ones — a wrapper
//! around `RealHost` must keep its async descriptor overrides (`net_spawn`, `net_accept`, …), which
//! the trait defaults would silently replace with the degraded serial bodies. A capability added to
//! the union later shows up here as a new arm name: an out-of-tree host then fails to *compile*
//! until it lists (or hand-writes) it — an honest breakage instead of a silent one.

/// Forward the named [`crate::Host`] capability traits of `$ty` to its field `$field`.
///
/// `delegate_host!(MyHost => base : FileReader, FileSystem, Rng, …)` — see the module docs for
/// the overlay and component patterns. The capability names are the trait names; `FileSystem`
/// requires `FileReader` to be listed (or hand-written) as well, since it is its supertrait.
#[macro_export]
macro_rules! delegate_host {
    ($ty:ty => $field:ident : $($cap:ident),+ $(,)?) => {
        $( $crate::__delegate_host_capability!($ty, $field, $cap); )+
    };
}

/// One capability's forwarding impl — the arms [`delegate_host!`] expands to. `#[doc(hidden)]`:
/// call the front door instead.
#[doc(hidden)]
#[macro_export]
macro_rules! __delegate_host_capability {
    ($ty:ty, $field:ident, FileReader) => {
        impl $crate::FileReader for $ty {
            fn fs_open_read(&mut self, path: &str) -> Result<$crate::ReadSource, $crate::StdError> {
                self.$field.fs_open_read(path)
            }
            fn fs_read_more(&mut self, id: u64) -> Result<Option<String>, $crate::StdError> {
                self.$field.fs_read_more(id)
            }
        }
    };
    ($ty:ty, $field:ident, FileSystem) => {
        impl $crate::FileSystem for $ty {
            fn fs_write(&mut self, path: &str, content: &str) -> Result<(), $crate::StdError> {
                self.$field.fs_write(path, content)
            }
            fn fs_append(&mut self, path: &str, content: &str) -> Result<(), $crate::StdError> {
                self.$field.fs_append(path, content)
            }
            fn fs_read(&self, path: &str) -> Result<String, $crate::StdError> {
                self.$field.fs_read(path)
            }
            fn fs_write_bytes(&mut self, path: &str, data: &[u8]) -> Result<(), $crate::StdError> {
                self.$field.fs_write_bytes(path, data)
            }
            fn fs_read_bytes(&self, path: &str) -> Result<Vec<u8>, $crate::StdError> {
                self.$field.fs_read_bytes(path)
            }
            fn fs_exists(&self, path: &str) -> bool {
                self.$field.fs_exists(path)
            }
            fn fs_remove(&mut self, path: &str) -> Result<bool, $crate::StdError> {
                self.$field.fs_remove(path)
            }
            fn fs_list(&self) -> Result<Vec<String>, $crate::StdError> {
                self.$field.fs_list()
            }
            fn fs_list_dir(&self, dir: &str) -> Result<Vec<String>, $crate::StdError> {
                self.$field.fs_list_dir(dir)
            }
            fn fs_mkdir(&mut self, path: &str) -> Result<(), $crate::StdError> {
                self.$field.fs_mkdir(path)
            }
            fn fs_is_dir(&self, path: &str) -> bool {
                self.$field.fs_is_dir(path)
            }
        }
    };
    ($ty:ty, $field:ident, Rng) => {
        impl $crate::Rng for $ty {
            fn rng_seed(&mut self, seed: i64) {
                self.$field.rng_seed(seed)
            }
            fn rng_int(&mut self, lo: i64, hi: i64) -> Result<i64, $crate::StdError> {
                self.$field.rng_int(lo, hi)
            }
            fn rng_float(&mut self) -> f64 {
                self.$field.rng_float()
            }
        }
    };
    ($ty:ty, $field:ident, Clock) => {
        impl $crate::Clock for $ty {
            fn clock_monotonic(&mut self) -> u64 {
                self.$field.clock_monotonic()
            }
            fn clock_sleep(&mut self, ms: i64) {
                self.$field.clock_sleep(ms)
            }
            fn clock_unix_ms(&mut self) -> u64 {
                self.$field.clock_unix_ms()
            }
        }
    };
    ($ty:ty, $field:ident, Entropy) => {
        impl $crate::Entropy for $ty {
            fn entropy_u64(&mut self) -> u64 {
                self.$field.entropy_u64()
            }
        }
    };
    ($ty:ty, $field:ident, Ids) => {
        impl $crate::Ids for $ty {
            fn id_next(&mut self) -> u64 {
                self.$field.id_next()
            }
        }
    };
    ($ty:ty, $field:ident, Env) => {
        impl $crate::Env for $ty {
            fn env_get(&self, key: &str) -> Option<String> {
                self.$field.env_get(key)
            }
            fn env_set(&mut self, key: &str, value: &str) {
                self.$field.env_set(key, value)
            }
            fn env_keys(&self) -> Vec<String> {
                self.$field.env_keys()
            }
            fn args(&self) -> Vec<String> {
                self.$field.args()
            }
        }
    };
    ($ty:ty, $field:ident, Os) => {
        impl $crate::Os for $ty {
            fn os_platform(&self) -> String {
                self.$field.os_platform()
            }
            fn os_arch(&self) -> String {
                self.$field.os_arch()
            }
            fn os_hostname(&self) -> String {
                self.$field.os_hostname()
            }
            fn os_cpus(&self) -> i64 {
                self.$field.os_cpus()
            }
            fn os_cwd(&self) -> String {
                self.$field.os_cwd()
            }
            fn os_pid(&self) -> i64 {
                self.$field.os_pid()
            }
            fn os_exec(
                &mut self,
                command: &str,
                args: &[String],
            ) -> Result<$crate::ExecResult, $crate::StdError> {
                self.$field.os_exec(command, args)
            }
            fn os_exec_spawn(
                &self,
                command: String,
                args: Vec<String>,
            ) -> Box<dyn $crate::ExternIo> {
                self.$field.os_exec_spawn(command, args)
            }
            fn os_spawn(
                &mut self,
                command: &str,
                args: &[String],
            ) -> Result<u64, $crate::StdError> {
                self.$field.os_spawn(command, args)
            }
            fn os_proc_pid(&self, handle: u64) -> Option<i64> {
                self.$field.os_proc_pid(handle)
            }
            fn os_proc_wait(
                &mut self,
                handle: u64,
            ) -> Result<$crate::ExecResult, $crate::StdError> {
                self.$field.os_proc_wait(handle)
            }
            fn os_proc_try_wait(
                &mut self,
                handle: u64,
            ) -> Result<Option<$crate::ExecResult>, $crate::StdError> {
                self.$field.os_proc_try_wait(handle)
            }
            fn os_proc_kill(&mut self, handle: u64) -> Result<(), $crate::StdError> {
                self.$field.os_proc_kill(handle)
            }
            fn os_proc_read_line(
                &mut self,
                handle: u64,
            ) -> Result<Option<String>, $crate::StdError> {
                self.$field.os_proc_read_line(handle)
            }
            fn os_proc_read(
                &mut self,
                handle: u64,
                count: i64,
            ) -> Result<Option<String>, $crate::StdError> {
                self.$field.os_proc_read(handle, count)
            }
            fn os_proc_read_stderr_line(
                &mut self,
                handle: u64,
            ) -> Result<Option<String>, $crate::StdError> {
                self.$field.os_proc_read_stderr_line(handle)
            }
            fn os_proc_write_stdin(
                &mut self,
                handle: u64,
                data: &str,
            ) -> Result<(), $crate::StdError> {
                self.$field.os_proc_write_stdin(handle, data)
            }
            fn os_proc_close_stdin(&mut self, handle: u64) -> Result<(), $crate::StdError> {
                self.$field.os_proc_close_stdin(handle)
            }
        }
    };
    ($ty:ty, $field:ident, Network) => {
        impl $crate::Network for $ty {
            fn net_fetch(
                &mut self,
                request: $crate::NetRequest,
            ) -> Result<$crate::NetResponse, $crate::StdError> {
                self.$field.net_fetch(request)
            }
            fn net_spawn(&self, request: $crate::NetRequest) -> Box<dyn $crate::ExternIo> {
                self.$field.net_spawn(request)
            }
            fn net_listen(&mut self, addr: &str) -> Result<u64, $crate::StdError> {
                self.$field.net_listen(addr)
            }
            fn net_accept_next(
                &mut self,
                listener: u64,
            ) -> Result<Option<(u64, $crate::NetRequest)>, $crate::StdError> {
                self.$field.net_accept_next(listener)
            }
            fn net_accept(&self, listener: u64) -> Box<dyn $crate::ExternIo> {
                self.$field.net_accept(listener)
            }
            fn net_reply_now(
                &mut self,
                conn: u64,
                response: $crate::NetResponse,
            ) -> Result<(), $crate::StdError> {
                self.$field.net_reply_now(conn, response)
            }
            fn net_reply(
                &self,
                conn: u64,
                response: $crate::NetResponse,
            ) -> Box<dyn $crate::ExternIo> {
                self.$field.net_reply(conn, response)
            }
            fn net_ws_upgrade_now(&mut self, conn: u64, key: &str) -> Result<(), $crate::StdError> {
                self.$field.net_ws_upgrade_now(conn, key)
            }
            fn net_ws_recv_next(&mut self, conn: u64) -> Result<Option<String>, $crate::StdError> {
                self.$field.net_ws_recv_next(conn)
            }
            fn net_ws_send_now(&mut self, conn: u64, text: &str) -> Result<(), $crate::StdError> {
                self.$field.net_ws_send_now(conn, text)
            }
            fn net_ws_close_now(&mut self, conn: u64) -> Result<(), $crate::StdError> {
                self.$field.net_ws_close_now(conn)
            }
            fn net_ws_upgrade(&self, conn: u64, key: String) -> Box<dyn $crate::ExternIo> {
                self.$field.net_ws_upgrade(conn, key)
            }
            fn net_ws_recv(&self, conn: u64) -> Box<dyn $crate::ExternIo> {
                self.$field.net_ws_recv(conn)
            }
            fn net_ws_send(&self, conn: u64, text: String) -> Box<dyn $crate::ExternIo> {
                self.$field.net_ws_send(conn, text)
            }
            fn net_ws_close(&self, conn: u64) -> Box<dyn $crate::ExternIo> {
                self.$field.net_ws_close(conn)
            }
        }
    };
    ($ty:ty, $field:ident, P2pProvider) => {
        impl $crate::P2pProvider for $ty {
            fn real_p2p(&self) -> Option<$crate::RealP2pConfig> {
                self.$field.real_p2p()
            }
        }
    };
    ($ty:ty, $field:ident, Tracing) => {
        impl $crate::Tracing for $ty {
            fn tel_enabled(&self) -> bool {
                self.$field.tel_enabled()
            }
            fn tel_span_start(
                &mut self,
                name: &str,
                kind: $crate::SpanKind,
                parent: Option<$crate::TraceContext>,
            ) -> $crate::SpanId {
                self.$field.tel_span_start(name, kind, parent)
            }
            fn tel_span_set_attr(
                &mut self,
                span: $crate::SpanId,
                key: &str,
                value: $crate::AttrValue,
            ) {
                self.$field.tel_span_set_attr(span, key, value)
            }
            fn tel_span_add_event(
                &mut self,
                span: $crate::SpanId,
                name: &str,
                attrs: Vec<($crate::__private::CompactString, $crate::AttrValue)>,
            ) {
                self.$field.tel_span_add_event(span, name, attrs)
            }
            fn tel_span_set_status(&mut self, span: $crate::SpanId, status: $crate::SpanStatus) {
                self.$field.tel_span_set_status(span, status)
            }
            fn tel_span_end(&mut self, span: $crate::SpanId) {
                self.$field.tel_span_end(span)
            }
            fn tel_span_context(&mut self, span: $crate::SpanId) -> $crate::TraceContext {
                self.$field.tel_span_context(span)
            }
            fn tel_intern_remote(&mut self, context: $crate::TraceContext) -> $crate::SpanId {
                self.$field.tel_intern_remote(context)
            }
            fn tel_is_remote(&self, span: $crate::SpanId) -> bool {
                self.$field.tel_is_remote(span)
            }
            fn tel_release_remote(&mut self, span: $crate::SpanId) {
                self.$field.tel_release_remote(span)
            }
        }
    };
    ($ty:ty, $field:ident, Logging) => {
        impl $crate::Logging for $ty {
            fn tel_logs_enabled(&self) -> bool {
                self.$field.tel_logs_enabled()
            }
            fn log_emit(&mut self, record: $crate::LogRecord) {
                self.$field.log_emit(record)
            }
        }
    };
    ($ty:ty, $field:ident, Metrics) => {
        impl $crate::Metrics for $ty {
            fn tel_metrics_enabled(&self) -> bool {
                self.$field.tel_metrics_enabled()
            }
            fn metric_get_or_create(
                &mut self,
                name: &str,
                unit: &str,
                kind: $crate::InstrumentKind,
            ) -> $crate::InstrumentId {
                self.$field.metric_get_or_create(name, unit, kind)
            }
            fn metric_observe(
                &mut self,
                inst: $crate::InstrumentId,
                value: $crate::MetricValue,
                attrs: Vec<($crate::__private::CompactString, $crate::AttrValue)>,
            ) {
                self.$field.metric_observe(inst, value, attrs)
            }
            fn metric_collect(&mut self) -> Vec<$crate::MetricData> {
                self.$field.metric_collect()
            }
        }
    };
}
