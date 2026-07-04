//! Backend runtime-table id newtypes (tree-walker side).
//!
//! The tree-walker's own copy of the id newtypes the VM defines in `noeta-value`. A
//! `Sender`/`Receiver`/channel future carries a **channel id**; a task `Handle` carries a
//! `(ScopeId, TaskId)`. These index the tree-walker's *own* concurrency bookkeeping (its channel
//! table and concurrency-scope stack) — backend implementation, not shared semantics — so each
//! backend owns its own set rather than sharing one through `noeta-stdlib`. The two backends' tables
//! never meet, so the types are parallel-but-independent by design.
//!
//! Representation is pinned to `u32` to match the VM's (an id is a small table index); a `usize`
//! narrows in via [`from_index`](ChannelId::from_index) and widens back out via
//! [`index`](ChannelId::index) at the table boundary.

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
    /// Index into the interpreter's channel table — the id a `Sender`/`Receiver` endpoint and a
    /// channel-send/-recv future carry.
    ChannelId
}

table_id! {
    /// Index into the interpreter's concurrency-scope stack — the scope half of a task `Handle`.
    ScopeId
}

table_id! {
    /// A task's index within its concurrency scope — the task half of a task `Handle`.
    TaskId
}
