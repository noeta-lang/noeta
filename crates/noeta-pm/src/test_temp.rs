//! This crate's fixture directories, from the workspace-wide helper.
//!
//! The implementation used to live here — it was written for `noeta-pm`'s unit tests, whose fixtures
//! at machine-global fixed names (`/tmp/noeta_git_test_fetch` and friends) deleted each other's trees
//! mid-setup whenever two test processes or two checkouts overlapped. The same shape then turned up
//! in six further crates, which is the signal that the helper belongs to the workspace rather than to
//! this crate: see [`noeta_test_temp`] for the full account of the bug class and how the per-process
//! root removes it. This module stays only so the call sites here keep reading `crate::test_temp::…`.

pub(crate) use noeta_test_temp::{TempDir, unique_path};
