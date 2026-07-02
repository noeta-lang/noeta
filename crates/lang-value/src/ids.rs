//! Backend runtime-table id newtypes (VM side).
//!
//! A `Sender`/`Receiver`/channel future carries a **channel id**; a task `Handle` carries a
//! `(ScopeId, TaskId)`. These are indices into the VM's *own* concurrency bookkeeping — its channel
//! table and its concurrency-scope stack — so they are backend implementation, **not** shared
//! semantics, and deliberately live here rather than in `lang-stdlib`. The tree-walker keeps its own
//! identical newtypes next to its `Value`; the two backends' tables never meet, so the types are
//! parallel-but-independent by design (the same way each backend owns its own `Value`).
//!
//! Newtyping them keeps a scope id from being passed where a channel id is expected, and pins the
//! representation at `u32` (a table index, so a `usize` narrows in via [`from_index`](ChannelId::from_index)
//! and widens back out via [`index`](ChannelId::index) at the table boundary).

macro_rules! table_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(u32);

        impl $name {
            /// Wrap a backing-table index.
            #[inline]
            pub fn from_index(index: usize) -> Self {
                $name(index as u32)
            }

            /// The index into the backing table.
            #[inline]
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

table_id! {
    /// Index into the VM's channel table — the id a `Sender`/`Receiver` endpoint and a
    /// channel-send/-recv future carry.
    ChannelId
}

table_id! {
    /// Index into the VM's concurrency-scope stack — the scope half of a task `Handle`.
    ScopeId
}

table_id! {
    /// A task's index within its concurrency scope — the task half of a task `Handle`.
    TaskId
}
